use super::*;

impl PhysicsWorld {
    pub(super) fn add_articulated_body_mode(
        &mut self,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        topology_token: u64,
        parts: Vec<SourcePart>,
        representation: Representation,
        kinematic_part_bodies: bool,
        dynamic_mass: Option<BodyMassProperties>,
    ) -> Result<ArticulatedBodyId, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        if parts.is_empty() || parts.len() > MAX_ARTICULATED_PARTS {
            return Err(ArticulatedError::PartCount);
        }
        if !articulated_translation_in_range(translation_m)
            || !(PartTransform {
                translation_m: [0.0; 3],
                rotation_xyzw,
            })
            .is_valid()
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let rotation_xyzw = normalize(rotation_xyzw);
        let already_owned_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|assembly| assembly.source_volume_ids.iter().copied())
            .collect();
        let mut sorted = parts;
        sorted.sort_by_key(|part| part.part_id);
        let mut ids = std::collections::BTreeSet::new();
        let mut volume_ids = std::collections::BTreeSet::new();
        let mut roots = 0usize;
        let mut dense_cells = 0u64;
        let mut solid_cells = 0u64;
        let cell_m = sorted[0].cell_m;
        for part in &mut sorted {
            if part.part_id == 0 || !ids.insert(part.part_id) {
                return Err(ArticulatedError::DuplicatePart);
            }
            if !volume_ids.insert(part.volume_id) || already_owned_volumes.contains(&part.volume_id)
            {
                return Err(ArticulatedError::DuplicateVolume);
            }
            if part.parent_id.is_none() {
                roots += 1;
            }
            if !part.local_transform.is_valid()
                || !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || (part.cell_m - cell_m).abs() > 1.0e-7
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            if !articulated_translation_in_range(part.local_transform.translation_m)
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::GeometryRange);
            }
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidTransform)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|v| v.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense_cells = dense_cells
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solid_cells = solid_cells
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
            if part.grid.solid_count() == 0 {
                return Err(ArticulatedError::EmptyPart(part.part_id));
            }
            if dense_cells > MAX_ARTICULATED_DENSE_CELLS
                || solid_cells > MAX_ARTICULATED_SOLID_CELLS
            {
                return Err(ArticulatedError::WorkBudget);
            }
        }
        if roots != 1
            || sorted
                .iter()
                .any(|part| part.parent_id.is_some_and(|parent| !ids.contains(&parent)))
        {
            return Err(ArticulatedError::Hierarchy);
        }
        let flattened = flatten_source_parts(&sorted)?;
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let source_volume_ids = sorted.iter().map(|part| part.volume_id).collect();
        let builds: Vec<_> = sorted
            .iter()
            .map(|part| build_collider(&part.grid, cell_m, representation))
            .collect();
        let primitive_count = builds.iter().map(|build| build.primitives).sum::<usize>();
        if builds
            .iter()
            .any(|build| build.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
            || primitive_count > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let collider_poses: Vec<_> = sorted
            .iter()
            .zip(&flattened)
            .map(|(part, pose)| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                let offset = rotate(pose.rotation_xyzw, origin_m);
                let translation = [
                    pose.translation_m[0] + offset[0],
                    pose.translation_m[1] + offset[1],
                    pose.translation_m[2] + offset[2],
                ];
                (translation, pose.rotation_xyzw)
            })
            .collect();
        if collider_poses.iter().any(|(translation, rotation)| {
            !translation.iter().all(|v| v.is_finite())
                || !rotation.iter().all(|v| v.is_finite())
                || !rotation.iter().map(|v| v * v).sum::<f32>().is_finite()
        }) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if collider_poses.iter().any(|(local_translation, _)| {
            let rotated = rotate(rotation_xyzw, *local_translation);
            !articulated_translation_in_range(*local_translation)
                || !articulated_translation_in_range([
                    translation_m[0] + rotated[0],
                    translation_m[1] + rotated[1],
                    translation_m[2] + rotated[2],
                ])
        }) {
            return Err(ArticulatedError::GeometryRange);
        }

        let root_rotation = Rotation::from_xyzw(
            rotation_xyzw[0],
            rotation_xyzw[1],
            rotation_xyzw[2],
            rotation_xyzw[3],
        );
        let root_builder = if let Some(mass) = dynamic_mass {
            RigidBodyBuilder::dynamic()
                .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
        } else {
            RigidBodyBuilder::kinematic_position_based()
        };
        let root = self.bodies.insert(
            root_builder
                .translation(Vector::new(
                    translation_m[0],
                    translation_m[1],
                    translation_m[2],
                ))
                .rotation(root_rotation.to_scaled_axis())
                .build(),
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let mut built_parts = Vec::with_capacity(sorted.len());
        for (((part, pose), (local_translation, local_rotation_xyzw)), build) in sorted
            .into_iter()
            .zip(flattened)
            .zip(collider_poses)
            .zip(builds)
        {
            let part_surface_radius_m = source_surface_radius_m(&part.grid, cell_m, [0.0; 3], None);
            let root_surface_radius_m =
                source_surface_radius_m(&part.grid, cell_m, [0.0; 3], Some(pose));
            let local_rotation = Rotation::from_xyzw(
                local_rotation_xyzw[0],
                local_rotation_xyzw[1],
                local_rotation_xyzw[2],
                local_rotation_xyzw[3],
            );
            let (part_body, collider_pose) = if kinematic_part_bodies {
                let world_pose = compose(
                    PartTransform {
                        translation_m,
                        rotation_xyzw,
                    },
                    pose,
                );
                let body_pose = Pose::new(
                    Vector::new(
                        world_pose.translation_m[0],
                        world_pose.translation_m[1],
                        world_pose.translation_m[2],
                    ),
                    Rotation::from_xyzw(
                        world_pose.rotation_xyzw[0],
                        world_pose.rotation_xyzw[1],
                        world_pose.rotation_xyzw[2],
                        world_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let body = self.bodies.insert(
                    RigidBodyBuilder::kinematic_position_based()
                        .pose(body_pose)
                        .build(),
                );
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * cell_m,
                    origin.y as f32 * cell_m,
                    origin.z as f32 * cell_m,
                ];
                (body, Pose::from_translation(Vector::from(origin_m)))
            } else {
                (
                    root,
                    Pose::new(
                        Vector::new(
                            local_translation[0],
                            local_translation[1],
                            local_translation[2],
                        ),
                        local_rotation.to_scaled_axis(),
                    ),
                )
            };
            let mut collider_builder = ColliderBuilder::new(build.collider.shared_shape().clone())
                .density(0.0)
                .position(collider_pose);
            if kinematic_part_bodies {
                collider_builder = collider_builder.active_collision_types(
                    ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
                );
            }
            let collider = collider_builder.build();
            let handle = self
                .colliders
                .insert_with_parent(collider, part_body, &mut self.bodies);
            self.pending_modified.push(handle);
            built_parts.push(ArticulatedPartEntry {
                part_id: part.part_id,
                parent_id: part.parent_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                grid: part.grid,
                cell_m,
                local_pose: part.local_transform,
                assembly_pose: pose,
                part_surface_radius_m,
                root_surface_radius_m,
                body: part_body,
                collider: handle,
                primitives: build.primitives,
                component_id: None,
            });
        }
        let primary = built_parts[0].collider;
        let first_origin = built_parts[0].grid.origin();
        self.entries.push(Entry {
            body: root,
            collider: primary,
            cell_m,
            representation,
            density: 0.0,
            collider_offset_m: [
                first_origin.x as f32 * cell_m,
                first_origin.y as f32 * cell_m,
                first_origin.z as f32 * cell_m,
            ],
            mass_properties: dynamic_mass,
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        for part in &built_parts {
            self.tag_collider(part.collider, id);
        }
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token,
                pose_token: 0,
                root_pose: PartTransform {
                    translation_m,
                    rotation_xyzw,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies,
                velocity_based_restore: false,
                dynamic_corpse: dynamic_mass.is_some(),
                source_generation: 1,
                fracture_owner_key: None,
                next_component_serial: 1,
                density_manifest: None,
                source_volume_ids,
                parts: built_parts,
            },
        );
        if let Some(mass) = dynamic_mass {
            let pose = *self.bodies[root].position();
            self.bodies[root]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[root].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[root].set_position(pose, true);
        }
        Ok(ArticulatedBodyId(id))
    }

    /// Atomically installs a complete sampled part-pose set after validating
    /// both version tokens and all transforms. Invalid/stale updates do not
    /// mutate any collider or token.
    pub fn update_articulated_pose(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let id = id.0;
        let current = self
            .articulated
            .get(&id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if current.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if update.topology_token != current.topology_token
            || current.pending_root_pose_token.is_some()
            || current.pending_pose.is_some()
            || update.base_pose_token != current.pose_token
            || update.pose_token <= current.pose_token
            || update.parts.len() != current.parts.len()
            || update
                .parts
                .iter()
                .zip(&current.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        if update
            .parts
            .iter()
            .any(|(_, pose)| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let locals: Vec<_> = update
            .parts
            .iter()
            .map(|(id, pose)| {
                pose.normalized()
                    .map(|pose| (*id, pose))
                    .ok_or(ArticulatedError::InvalidTransform)
            })
            .collect::<Result<_, _>>()?;
        let flattened = flatten_local_poses(&current.parts, &locals)?;
        if flattened.iter().any(|pose| !pose.is_valid()) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if flattened
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }
        let prepared: Vec<_> = current
            .parts
            .iter()
            .zip(locals.iter().zip(flattened.iter()))
            .map(|(part, ((part_id, local), world))| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(world.rotation_xyzw, origin_m);
                let translation = [
                    world.translation_m[0] + offset[0],
                    world.translation_m[1] + offset[1],
                    world.translation_m[2] + offset[2],
                ];
                (*part_id == part.part_id && articulated_translation_in_range(translation))
                    .then_some((*local, *world, translation))
            })
            .collect();
        if prepared.len() != current.parts.len() || prepared.iter().any(Option::is_none) {
            return Err(ArticulatedError::InvalidTransform);
        }
        if current.kinematic_part_bodies {
            let root_body = self.entries[entry_index(id, self.id_namespace)].body;
            let root = *self.bodies[root_body].position();
            let targets = current
                .parts
                .iter()
                .zip(prepared.iter().flatten())
                .map(|(part, (_, world, _))| {
                    let local = Pose::new(
                        Vector::new(
                            world.translation_m[0],
                            world.translation_m[1],
                            world.translation_m[2],
                        ),
                        Rotation::from_xyzw(
                            world.rotation_xyzw[0],
                            world.rotation_xyzw[1],
                            world.rotation_xyzw[2],
                            world.rotation_xyzw[3],
                        )
                        .to_scaled_axis(),
                    );
                    (part.body, root * local)
                })
                .collect::<Vec<_>>();
            if targets.iter().any(|(_, pose)| {
                !articulated_translation_in_range([
                    pose.translation.x,
                    pose.translation.y,
                    pose.translation.z,
                ])
            }) {
                return Err(ArticulatedError::GeometryRange);
            }
            self.validate_kinematic_surface_travel(&targets)?;
            if current.velocity_based_restore {
                self.bodies[root_body].set_body_type(RigidBodyType::KinematicPositionBased, true);
                for part in &current.parts {
                    self.bodies[part.body]
                        .set_body_type(RigidBodyType::KinematicPositionBased, true);
                }
                self.articulated
                    .get_mut(&id)
                    .expect("validated assembly exists")
                    .velocity_based_restore = false;
            }
            for (body, pose) in targets {
                self.bodies[body].set_next_kinematic_position(pose);
            }
            let pending = locals
                .into_iter()
                .zip(flattened)
                .map(|((part_id, local), world)| (part_id, local, world))
                .collect::<Vec<_>>();
            self.articulated
                .get_mut(&id)
                .expect("validated assembly exists")
                .pending_pose = Some(PendingArticulatedPose {
                pose_token: update.pose_token,
                parts: pending,
            });
            return Ok(());
        }
        let root = self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        if prepared.iter().flatten().any(|(_, _, local_translation)| {
            let world_offset = root.transform_vector(Vector::new(
                local_translation[0],
                local_translation[1],
                local_translation[2],
            ));
            let world = root.translation + world_offset;
            !articulated_translation_in_range([world.x, world.y, world.z])
        }) {
            return Err(ArticulatedError::InvalidTransform);
        }
        let assembly = self
            .articulated
            .get_mut(&id)
            .expect("validated assembly exists");
        for (part, prepared) in assembly
            .parts
            .iter_mut()
            .zip(prepared.into_iter().flatten())
        {
            let (local, world, translation) = prepared;
            let rotation = Rotation::from_xyzw(
                world.rotation_xyzw[0],
                world.rotation_xyzw[1],
                world.rotation_xyzw[2],
                world.rotation_xyzw[3],
            );
            let collider_local = Pose::new(
                Vector::new(translation[0], translation[1], translation[2]),
                rotation.to_scaled_axis(),
            );
            let collider = &mut self.colliders[part.collider];
            collider.set_position_wrt_parent(collider_local);
            // Rapier propagates collider-local changes during its next step.
            // Queries at this tick boundary need the installed pose now, even
            // when the unchanged parent is no longer in its modified-body list.
            // Preserve the native dirty flags for the real solver/broad phase.
            collider.set_position(*root * collider_local);
            part.local_pose = local.normalized().expect("validated local pose");
            part.assembly_pose = world;
            self.pending_modified.push(part.collider);
        }
        assembly.pose_token = update.pose_token;
        Ok(())
    }
}
