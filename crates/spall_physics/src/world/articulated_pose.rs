use super::*;

impl PhysicsWorld {
    /// Moves the single kinematic assembly root after validating every
    /// transformed source-part collider pose. Part-local sampled poses stay
    /// unchanged and all attached colliders are refreshed together.
    pub fn set_articulated_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let body_id = id.0;
        let rotation = PartTransform {
            translation_m: [0.0; 3],
            rotation_xyzw,
        }
        .normalized()
        .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.kinematic_part_bodies {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        if assembly.pending_pose.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let collider_offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
            let part_local = [
                part.assembly_pose.translation_m[0] + collider_offset[0],
                part.assembly_pose.translation_m[1] + collider_offset[1],
                part.assembly_pose.translation_m[2] + collider_offset[2],
            ];
            let rotated = rotate(rotation.rotation_xyzw, part_local);
            if (0..3).any(|axis| !(translation_m[axis] + rotated[axis]).is_finite()) {
                return Err(ArticulatedError::InvalidRootPose);
            }
        }
        let body = self.entries[entry_index(body_id, self.id_namespace)].body;
        self.bodies[body].set_translation(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            true,
        );
        self.bodies[body].set_rotation(
            Rotation::from_xyzw(
                rotation.rotation_xyzw[0],
                rotation.rotation_xyzw[1],
                rotation.rotation_xyzw[2],
                rotation.rotation_xyzw[3],
            ),
            true,
        );
        self.pending_modified
            .extend(assembly.parts.iter().map(|part| part.collider));
        self.articulated
            .get_mut(&body_id)
            .expect("validated assembly exists")
            .pose_token = pose_token;
        Ok(())
    }

    /// Schedules an authored root movement for the next Rapier fixed step. Its
    /// pose token becomes visible only after the solver installs that target.
    pub fn set_next_articulated_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        self.set_next_articulated_root_pose_impl(
            id,
            topology_token,
            base_pose_token,
            pose_token,
            translation_m,
            rotation_xyzw,
            false,
        )
    }

    fn set_next_articulated_root_pose_impl(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        translation_m: [f32; 3],
        rotation_xyzw: [f32; 4],
        allow_kinematic_part_rig: bool,
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let body_id = id.0;
        let rotation = PartTransform {
            translation_m: [0.0; 3],
            rotation_xyzw,
        }
        .normalized()
        .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.kinematic_part_bodies && !allow_kinematic_part_rig {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
        {
            return Err(ArticulatedError::StalePose);
        }
        if assembly.pending_root_pose_token.is_some()
            || (assembly.pending_pose.is_some() && !allow_kinematic_part_rig)
        {
            return Err(ArticulatedError::PendingRootPose);
        }
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let collider_offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
            let part_local = [
                part.assembly_pose.translation_m[0] + collider_offset[0],
                part.assembly_pose.translation_m[1] + collider_offset[1],
                part.assembly_pose.translation_m[2] + collider_offset[2],
            ];
            let rotated = rotate(rotation.rotation_xyzw, part_local);
            if (0..3).any(|axis| !(translation_m[axis] + rotated[axis]).is_finite()) {
                return Err(ArticulatedError::InvalidRootPose);
            }
        }
        let root_target = Pose::new(
            Vector::new(translation_m[0], translation_m[1], translation_m[2]),
            Rotation::from_xyzw(
                rotation.rotation_xyzw[0],
                rotation.rotation_xyzw[1],
                rotation.rotation_xyzw[2],
                rotation.rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let part_targets = if assembly.kinematic_part_bodies {
            assembly
                .parts
                .iter()
                .enumerate()
                .map(|(index, part)| {
                    let part_pose = assembly
                        .pending_pose
                        .as_ref()
                        .map(|pending| pending.parts[index].2)
                        .unwrap_or(part.assembly_pose);
                    let local = Pose::new(
                        Vector::new(
                            part_pose.translation_m[0],
                            part_pose.translation_m[1],
                            part_pose.translation_m[2],
                        ),
                        Rotation::from_xyzw(
                            part_pose.rotation_xyzw[0],
                            part_pose.rotation_xyzw[1],
                            part_pose.rotation_xyzw[2],
                            part_pose.rotation_xyzw[3],
                        )
                        .to_scaled_axis(),
                    );
                    (part.body, root_target * local)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let body = self.entries[entry_index(body_id, self.id_namespace)].body;
        let mut all_targets = vec![(body, root_target)];
        all_targets.extend(part_targets.iter().copied());
        self.validate_kinematic_surface_travel(&all_targets)?;
        if assembly.velocity_based_restore {
            self.bodies[body].set_body_type(RigidBodyType::KinematicPositionBased, true);
            for part in &assembly.parts {
                self.bodies[part.body].set_body_type(RigidBodyType::KinematicPositionBased, true);
            }
            self.articulated
                .get_mut(&body_id)
                .expect("validated assembly exists")
                .velocity_based_restore = false;
        }
        self.bodies[body].set_next_kinematic_position(root_target);
        for (part_body, target) in part_targets {
            self.bodies[part_body].set_next_kinematic_position(target);
        }
        self.articulated
            .get_mut(&body_id)
            .expect("validated assembly exists")
            .pending_root_pose_token = Some(pose_token);
        Ok(())
    }

    /// Conservatively sweeps a sphere enclosing the complete current assembly
    /// against live colliders, then schedules the clipped root and all part
    /// bodies for the same physics boundary. The sphere contract can block
    /// clear narrow passages; it never claims exact articulated clearance.
    pub fn sweep_articulated_root_translation(
        &mut self,
        id: ArticulatedBodyId,
        topology_token: u64,
        base_pose_token: u64,
        pose_token: u64,
        requested_translation_m: [f32; 3],
    ) -> Result<crate::articulated::ArticulatedMove, crate::articulated::ArticulatedError> {
        use crate::articulated::{ArticulatedError, ArticulatedMove};
        if !requested_translation_m
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(ArticulatedError::InvalidRootPose);
        }
        // Broad-phase query casts must observe edits/pose changes made since
        // the last step, even when the caller has not explicitly synced.
        self.sync_queries();
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies {
            return Err(ArticulatedError::Hierarchy);
        }
        if assembly.topology_token != topology_token
            || assembly.pose_token != base_pose_token
            || pose_token <= base_pose_token
            || assembly.pending_root_pose_token.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        let mut bounds_min = [f32::INFINITY; 3];
        let mut bounds_max = [f32::NEG_INFINITY; 3];
        for part in &assembly.parts {
            let origin = part.grid.origin();
            let dims = part.grid.dims();
            for corner in 0..8 {
                let source = [
                    (origin.x as f32 + if corner & 1 == 0 { 0.0 } else { dims[0] as f32 })
                        * part.cell_m,
                    (origin.y as f32 + if corner & 2 == 0 { 0.0 } else { dims[1] as f32 })
                        * part.cell_m,
                    (origin.z as f32 + if corner & 4 == 0 { 0.0 } else { dims[2] as f32 })
                        * part.cell_m,
                ];
                let rotated = rotate(part.assembly_pose.rotation_xyzw, source);
                for axis in 0..3 {
                    let value = part.assembly_pose.translation_m[axis] + rotated[axis];
                    bounds_min[axis] = bounds_min[axis].min(value);
                    bounds_max[axis] = bounds_max[axis].max(value);
                }
            }
        }
        let (local_center, radius) = if let Some(pending) = &assembly.pending_pose {
            let mut radius_squared = 0.0f32;
            for (index, part) in assembly.parts.iter().enumerate() {
                let origin = part.grid.origin();
                let dims = part.grid.dims();
                for corner in 0..8 {
                    let source = [
                        (origin.x as f32 + if corner & 1 == 0 { 0.0 } else { dims[0] as f32 })
                            * part.cell_m,
                        (origin.y as f32 + if corner & 2 == 0 { 0.0 } else { dims[1] as f32 })
                            * part.cell_m,
                        (origin.z as f32 + if corner & 4 == 0 { 0.0 } else { dims[2] as f32 })
                            * part.cell_m,
                    ];
                    for pose in [part.assembly_pose, pending.parts[index].2] {
                        let translation_len = pose
                            .translation_m
                            .iter()
                            .map(|value| value * value)
                            .sum::<f32>()
                            .sqrt();
                        let source_len =
                            source.iter().map(|value| value * value).sum::<f32>().sqrt();
                        // Translation interpolation stays in the endpoint
                        // convex hull; this sum bounds every source corner
                        // under any intermediate part rotation.
                        radius_squared = radius_squared.max((translation_len + source_len).powi(2));
                    }
                }
            }
            ([0.0; 3], radius_squared.sqrt().max(0.001))
        } else {
            let center =
                std::array::from_fn::<_, 3, _>(|axis| 0.5 * (bounds_min[axis] + bounds_max[axis]));
            let radius = (0..3)
                .map(|axis| (0.5 * (bounds_max[axis] - bounds_min[axis])).powi(2))
                .sum::<f32>()
                .sqrt()
                .max(0.001);
            (center, radius)
        };
        let root = assembly.root_pose;
        let rotated_center = rotate(root.rotation_xyzw, local_center);
        let start = Pose::from_translation(Vector::from([
            root.translation_m[0] + rotated_center[0],
            root.translation_m[1] + rotated_center[1],
            root.translation_m[2] + rotated_center[2],
        ]));
        let displacement = Vector::from(requested_translation_m);
        let displacement_length = displacement.length();
        let displacement_direction = if displacement_length > 0.0 {
            displacement / displacement_length
        } else {
            Vector::ZERO
        };
        // Parry's conservative advancement loses sub-10µm displacement at
        // world-coordinate float precision. Cast at a fixed minimum distance,
        // discard contacts beyond the actual request, and convert TOI back to
        // a fraction of that request.
        let query_distance = displacement_length.max(1.0e-5);
        let query_displacement = displacement_direction * query_distance;
        let excluded: Vec<_> = assembly.parts.iter().map(|part| part.collider).collect();
        // Root translation is explicitly a terrain sweep. Dynamic/kinematic
        // bodies are handled by the shared Rapier solver after scheduling;
        // sensors do not block locomotion.
        let terrain_colliders: Vec<_> = self
            .colliders
            .iter()
            .filter_map(|(handle, collider)| {
                let fixed = collider
                    .parent()
                    .and_then(|parent| self.bodies.get(parent))
                    .is_some_and(|body| body.is_fixed());
                (fixed && !collider.is_sensor()).then_some(handle)
            })
            .collect();
        let hit = if displacement.length_squared() == 0.0 {
            None
        } else {
            let shape = Ball::new(radius);
            let mut nearest: Option<(ColliderHandle, rapier3d::parry::query::ShapeCastHit)> = None;
            for &terrain_handle in &terrain_colliders {
                let only_terrain = |handle: ColliderHandle, _: &Collider| {
                    handle == terrain_handle && !excluded.contains(&handle)
                };
                let filter = QueryFilter::default().predicate(&only_terrain);
                let queries = self.broad_phase.as_query_pipeline(
                    self.narrow_phase.query_dispatcher(),
                    &self.bodies,
                    &self.colliders,
                    filter,
                );
                let Some(candidate) = queries.cast_shape(
                    &start,
                    query_displacement,
                    &shape,
                    rapier3d::parry::query::ShapeCastOptions {
                        max_time_of_impact: 1.0,
                        target_distance: 0.0,
                        stop_at_penetration: true,
                        compute_impact_geometry_on_penetration: true,
                    },
                ) else {
                    continue;
                };
                if candidate.1.time_of_impact * query_distance > displacement_length {
                    continue;
                }
                // At exact support tangency Rapier may report TOI=0 for a
                // tangent or separating request. Ignore only that contact;
                // retain zero-time hits whose normalized motion direction
                // points into terrain. The angular tolerance is independent of
                // request distance, so tiny inward moves are still blocked.
                if candidate.1.time_of_impact <= 1.0e-6
                    && candidate.1.normal1.dot(displacement_direction) >= -1.0e-6
                {
                    continue;
                }
                if nearest.is_none_or(|(_, hit): (ColliderHandle, _)| {
                    candidate.1.time_of_impact < hit.time_of_impact
                }) {
                    nearest = Some(candidate);
                }
            }
            nearest
        };
        let blocked = hit.is_some();
        let fraction = hit.map_or(1.0, |(_, hit)| {
            (hit.time_of_impact * query_distance / displacement_length).clamp(0.0, 1.0)
        });
        let applied_translation_m = requested_translation_m.map(|value| value * fraction);
        let target = [
            root.translation_m[0] + applied_translation_m[0],
            root.translation_m[1] + applied_translation_m[1],
            root.translation_m[2] + applied_translation_m[2],
        ];
        self.set_next_articulated_root_pose_impl(
            id,
            topology_token,
            base_pose_token,
            pose_token,
            target,
            root.rotation_xyzw,
            true,
        )?;
        Ok(ArticulatedMove {
            requested_translation_m,
            applied_translation_m,
            fraction,
            blocked,
        })
    }

    /// Atomically stages one complete sampled pose and a swept root
    /// translation for the same physics boundary. The sweep uses a root-
    /// centered sphere enclosing old and proposed source geometry under any
    /// interpolated part rotation/translation; this is conservative and may
    /// overblock. It does not claim exact limb motion or terrain response for
    /// arbitrary discontinuous authored pose changes.
    pub fn update_articulated_pose_with_swept_root_translation(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        requested_translation_m: [f32; 3],
    ) -> Result<crate::articulated::ArticulatedMove, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        if !requested_translation_m
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(ArticulatedError::InvalidRootPose);
        }
        if !articulated_translation_in_range(requested_translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies
            || assembly.topology_token != update.topology_token
            || assembly.pose_token != update.base_pose_token
            || update.pose_token <= update.base_pose_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        if update.parts.len() != assembly.parts.len()
            || update
                .parts
                .iter()
                .zip(&assembly.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        let locals = update
            .parts
            .iter()
            .map(|(part_id, pose)| {
                if !articulated_translation_in_range(pose.translation_m) {
                    return Err(ArticulatedError::GeometryRange);
                }
                Ok((*part_id, pose.normalized().expect("validated transform")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let flattened = flatten_local_poses(&assembly.parts, &locals)?;
        let root_target = std::array::from_fn::<_, 3, _>(|axis| {
            assembly.root_pose.translation_m[axis] + requested_translation_m[axis]
        });
        if !articulated_translation_in_range(root_target) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let root_body = self.entries[entry_index(id.0, self.id_namespace)].body;
        let root_start = *self.bodies[root_body].position();
        let root_target_pose = Pose::new(
            Vector::from(root_target),
            root_start.rotation.to_scaled_axis(),
        );
        let mut scheduled_targets = vec![(root_body, root_target_pose)];
        for (part, world_pose) in assembly.parts.iter().zip(flattened) {
            if !articulated_translation_in_range(world_pose.translation_m) {
                return Err(ArticulatedError::GeometryRange);
            }
            let origin = part.grid.origin();
            let origin_m = [
                origin.x as f32 * part.cell_m,
                origin.y as f32 * part.cell_m,
                origin.z as f32 * part.cell_m,
            ];
            let local_offset = rotate(world_pose.rotation_xyzw, origin_m);
            let local_translation = std::array::from_fn::<_, 3, _>(|axis| {
                world_pose.translation_m[axis] + local_offset[axis]
            });
            let world_offset = rotate(assembly.root_pose.rotation_xyzw, local_translation);
            let world_translation =
                std::array::from_fn::<_, 3, _>(|axis| root_target[axis] + world_offset[axis]);
            if !articulated_translation_in_range(world_translation) {
                return Err(ArticulatedError::GeometryRange);
            }
            let local_pose = Pose::new(
                Vector::from(world_pose.translation_m),
                Rotation::from_xyzw(
                    world_pose.rotation_xyzw[0],
                    world_pose.rotation_xyzw[1],
                    world_pose.rotation_xyzw[2],
                    world_pose.rotation_xyzw[3],
                )
                .to_scaled_axis(),
            );
            scheduled_targets.push((part.body, root_target_pose * local_pose));
        }
        self.validate_kinematic_surface_travel(&scheduled_targets)?;
        // All pose, topology, and target coordinates have been validated
        // before either half is staged; the root sweep then cannot fail on
        // these inputs, and both targets commit at the same step boundary.
        self.update_articulated_pose(id, update.clone())?;
        self.sweep_articulated_root_translation(
            id,
            update.topology_token,
            update.base_pose_token,
            update.pose_token,
            requested_translation_m,
        )
    }

    /// Sweeps every exact occupied source cell in each source-part collider
    /// through its declared one-tick world-space interpolation (linear
    /// translation and shortest-arc rotation), including optional root motion,
    /// against current fixed terrain. Each source cell is swept as a Cuboid
    /// query shape so Parry's translating-only moving-Voxels nonlinear cast
    /// path is not used for rotating NativeVoxels parts. A terrain hit rejects
    /// the complete sample without staging any body or token. This contract
    /// does not claim joint-constrained intermediate poses or solver-driven
    /// terrain response.
    pub fn update_articulated_pose_with_swept_root_pose(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        root_target: PartTransform,
    ) -> Result<crate::articulated::ArticulatedTransition, crate::articulated::ArticulatedError>
    {
        self.update_articulated_pose_impl(id, update, root_target, true)
    }

    /// Stages the same complete pose and root target **without** casting the
    /// source cells against terrain. For a caller that has already resolved the
    /// root's motion against terrain with a conservative proxy (a character
    /// capsule around the creature); the pose and root are still validated and
    /// versioned exactly like the swept path. Terrain is not consulted, so
    /// limbs may overlap terrain the proxy does not cover.
    pub fn update_articulated_pose_trusted(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        root_target: PartTransform,
    ) -> Result<crate::articulated::ArticulatedTransition, crate::articulated::ArticulatedError>
    {
        self.update_articulated_pose_impl(id, update, root_target, false)
    }

    fn update_articulated_pose_impl(
        &mut self,
        id: ArticulatedBodyId,
        update: ArticulatedPose,
        root_target: PartTransform,
        terrain_cast: bool,
    ) -> Result<crate::articulated::ArticulatedTransition, crate::articulated::ArticulatedError>
    {
        use crate::articulated::{ArticulatedError, ArticulatedTransition};
        if !root_target.is_valid() {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let root_target = root_target
            .normalized()
            .ok_or(ArticulatedError::InvalidRootPose)?;
        if !articulated_translation_in_range(root_target.translation_m) {
            return Err(ArticulatedError::InvalidRootPose);
        }
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if !assembly.kinematic_part_bodies
            || assembly.topology_token != update.topology_token
            || assembly.pose_token != update.base_pose_token
            || update.pose_token <= update.base_pose_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
        {
            return Err(ArticulatedError::StalePose);
        }
        if update.parts.len() != assembly.parts.len()
            || update
                .parts
                .iter()
                .zip(&assembly.parts)
                .any(|((part_id, pose), part)| {
                    *part_id != part.part_id || pose.normalized().is_none()
                })
        {
            return Err(ArticulatedError::StalePose);
        }
        let locals = update
            .parts
            .iter()
            .map(|(part_id, pose)| {
                if !articulated_translation_in_range(pose.translation_m) {
                    return Err(ArticulatedError::GeometryRange);
                }
                Ok((*part_id, pose.normalized().expect("validated transform")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let flattened = flatten_local_poses(&assembly.parts, &locals)?;
        let target_root = Pose::new(
            Vector::from(root_target.translation_m),
            Rotation::from_xyzw(
                root_target.rotation_xyzw[0],
                root_target.rotation_xyzw[1],
                root_target.rotation_xyzw[2],
                root_target.rotation_xyzw[3],
            )
            .to_scaled_axis(),
        );
        let mut targets = Vec::with_capacity(assembly.parts.len());
        for (part, world_pose) in assembly.parts.iter().zip(flattened.iter()) {
            let local = Pose::new(
                Vector::from(world_pose.translation_m),
                Rotation::from_xyzw(
                    world_pose.rotation_xyzw[0],
                    world_pose.rotation_xyzw[1],
                    world_pose.rotation_xyzw[2],
                    world_pose.rotation_xyzw[3],
                )
                .to_scaled_axis(),
            );
            let body_target = target_root * local;
            let collider_local = self.colliders[part.collider]
                .position_wrt_parent()
                .copied()
                .unwrap_or_else(Pose::identity);
            let collider_target = body_target * collider_local;
            if !articulated_translation_in_range([
                body_target.translation.x,
                body_target.translation.y,
                body_target.translation.z,
            ]) || !articulated_translation_in_range([
                collider_target.translation.x,
                collider_target.translation.y,
                collider_target.translation.z,
            ]) {
                return Err(ArticulatedError::GeometryRange);
            }
            targets.push((
                part.body,
                part.collider,
                *self.colliders[part.collider].position(),
                collider_target,
                body_target,
            ));
        }
        let root_body = self.entries[entry_index(body_id, self.id_namespace)].body;
        let mut scheduled_targets = vec![(root_body, target_root)];
        scheduled_targets.extend(
            targets
                .iter()
                .map(|(body, _, _, _, body_target)| (*body, *body_target)),
        );
        self.validate_kinematic_surface_travel(&scheduled_targets)?;
        let part_ids = assembly
            .parts
            .iter()
            .map(|part| part.part_id)
            .collect::<Vec<_>>();
        let source_parts = assembly
            .parts
            .iter()
            .map(|part| (part.body, part.grid.clone(), part.cell_m))
            .collect::<Vec<_>>();
        let total_cells = source_parts
            .iter()
            .map(|(_, grid, _)| grid.solid_count() as usize)
            .sum::<usize>();
        let candidate_limit = transition_candidate_limit(total_cells);

        // Query freshness is part of this gate: voxel edits/removals made since
        // the previous step must affect this same transition decision.
        let sync_started = Instant::now();
        self.sync_queries();
        let query_sync_time = sync_started.elapsed();
        let mut earliest: Option<(u32, f32)> = None;
        // Cull fixed terrain using a conservative bound for the complete
        // declared part-origin path. Every cell corner stays within `radius`
        // of its body origin: rotation preserves distance, while translation
        // follows the segment between the two validated poses. This includes
        // mid-arc orientations and off-center/negative-origin source grids.
        // The exact nonlinear source-cell casts below remain authoritative.
        let mut swept_bounds = Vec::with_capacity(targets.len());
        for ((part_body, collider_handle, _, _, body_target), (_, grid, cell_m)) in
            targets.iter().zip(&source_parts).filter(|_| terrain_cast)
        {
            let body_start = *self.bodies[*part_body].position();
            let collider_local = self.colliders[*collider_handle]
                .position_wrt_parent()
                .copied()
                .unwrap_or_else(Pose::identity);
            let half_diagonal = (3.0_f32).sqrt() * *cell_m * 0.5;
            let radius = grid
                .solid_indices()
                .into_iter()
                .map(|[x, y, z]| {
                    let cell_center = Vector::new(
                        (x as f32 + 0.5) * *cell_m,
                        (y as f32 + 0.5) * *cell_m,
                        (z as f32 + 0.5) * *cell_m,
                    );
                    let local_center = collider_local * Pose::from_translation(cell_center);
                    local_center.translation.length_squared().sqrt() + half_diagonal
                })
                .fold(0.0_f32, f32::max);
            if !radius.is_finite() {
                return Err(ArticulatedError::GeometryRange);
            }
            let start = body_start.translation;
            let end = body_target.translation;
            // Outward rounding padding only adds candidates.
            let margin = 1.0e-4_f32;
            swept_bounds.push((
                Vector::new(
                    start.x.min(end.x) - radius - margin,
                    start.y.min(end.y) - radius - margin,
                    start.z.min(end.z) - radius - margin,
                ),
                Vector::new(
                    start.x.max(end.x) + radius + margin,
                    start.y.max(end.y) + radius + margin,
                    start.z.max(end.z) + radius + margin,
                ),
            ));
        }
        let fixed_nonsensor = |_: ColliderHandle, collider: &Collider| {
            !collider.is_sensor()
                && collider
                    .parent()
                    .and_then(|parent| self.bodies.get(parent))
                    .is_some_and(|body| body.is_fixed())
        };
        let candidate_started = Instant::now();
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            QueryFilter::default().predicate(&fixed_nonsensor),
        );
        let mut terrain_colliders = Vec::with_capacity(candidate_limit.min(4096));
        let mut seen_candidates = HashSet::with_capacity(candidate_limit.min(4096));
        for (min, max) in swept_bounds.iter().filter(|_| terrain_cast) {
            let bounds = rapier3d::parry::bounding_volume::Aabb::new(*min, *max);
            for (handle, _) in query.intersect_aabb_conservative(bounds) {
                if !seen_candidates.contains(&handle) {
                    if seen_candidates.len() == candidate_limit {
                        return Err(ArticulatedError::TerrainTransitionWorkBudget {
                            source_cells: total_cells,
                            terrain_candidates_at_least: candidate_limit.saturating_add(1),
                        });
                    }
                    seen_candidates.insert(handle);
                    terrain_colliders.push(handle);
                }
            }
        }
        let candidate_query_time = candidate_started.elapsed();
        let terrain_candidate_count = terrain_colliders.len();
        // Parry 0.30.2 explicitly treats a moving `Voxels` shape as translation
        // only during nonlinear casts and builds an end AABB from endpoint
        // orientations. A rotating off-center source grid therefore is not a
        // sound broadphase/cast primitive for this operation. Cast each exact
        // occupied source cell instead. This is still one source-part body and
        // one live collider; cell cuboids exist only as bounded query shapes.
        // Their center follows the same linear part-origin / shortest-arc
        // rotation path as the kinematic body, so their union is the source
        // grid at every time (up to Parry's documented iterative TOI tolerance).
        let mut cell_casts = 0usize;
        let casts_started = Instant::now();
        for ((part_id, (_, _, collider_start, _, body_target)), (body, grid, cell_m)) in part_ids
            .iter()
            .zip(&targets)
            .zip(&source_parts)
            .filter(|_| terrain_cast)
        {
            let body_start = *self.bodies[*body].position();
            let body_delta = body_target.translation - body_start.translation;
            let delta_rotation = body_target.rotation * body_start.rotation.inverse();
            let angular_delta = delta_rotation.to_scaled_axis();
            let origin = grid.origin();
            let grid_origin = Vector::new(
                origin.x as f32 * *cell_m,
                origin.y as f32 * *cell_m,
                origin.z as f32 * *cell_m,
            );
            let half = Vector::splat(*cell_m * 0.5);
            let cell_shape = rapier3d::prelude::Cuboid::new(half);
            for [x, y, z] in grid.solid_indices() {
                let cell_center = Vector::new(
                    (x as f32 + 0.5) * *cell_m,
                    (y as f32 + 0.5) * *cell_m,
                    (z as f32 + 0.5) * *cell_m,
                );
                let cell_start = *collider_start * Pose::from_translation(cell_center);
                let motion = rapier3d::parry::query::NonlinearRigidMotion::new(
                    cell_start,
                    -(grid_origin + cell_center),
                    body_delta,
                    angular_delta,
                );
                // Sound culling: a cell cannot touch terrain unless some solid
                // terrain voxel lies inside the bound of everything that cell
                // sweeps through. The bound is the chord between the cell's start
                // and end centers inflated by the cell's circumscribed radius and
                // twice the rotational excursion (|v| * angle at both ends of
                // the arc). Voxel-grid and compound terrain shapes are culled
                // against their own spatial index; any other shape is cast.
                let end_center = motion.position_at_time(1.0).translation;
                let start_center = cell_start.translation;
                let cell_radius = (grid_origin + cell_center).length();
                let reach = half.length() + 2.0 * cell_radius * angular_delta.length() + 1.0e-3;
                let swept = rapier3d::parry::bounding_volume::Aabb::new(
                    start_center.min(end_center) - Vector::splat(reach),
                    start_center.max(end_center) + Vector::splat(reach),
                );
                for &terrain_handle in &terrain_colliders {
                    let terrain = &self.colliders[terrain_handle];
                    let shape = terrain.shape();
                    if let Some(voxels) = shape.as_voxels() {
                        let local = swept.transform_by(&terrain.position().inverse());
                        if voxels
                            .voxels_intersecting_local_aabb(&local)
                            .next()
                            .is_none()
                        {
                            continue;
                        }
                    } else if let Some(compound) = shape.as_compound() {
                        let local = swept.transform_by(&terrain.position().inverse());
                        if compound.bvh().intersect_aabb(&local).next().is_none() {
                            continue;
                        }
                    }
                    cell_casts += 1;
                    // The terrain candidate list came from the synchronized live
                    // broad phase above. Querying one candidate by constructing a
                    // filtered QueryPipeline here would rebuild a view of every
                    // collider for every source cell. Cast directly against that
                    // candidate's exact live shape and world pose instead.
                    let terrain = &self.colliders[terrain_handle];
                    let stationary = rapier3d::parry::query::NonlinearRigidMotion::new(
                        *terrain.position(),
                        Vector::ZERO,
                        Vector::ZERO,
                        Vector::ZERO,
                    );
                    let cast_result = rapier3d::parry::query::cast_shapes_nonlinear(
                        &motion,
                        &cell_shape,
                        &stationary,
                        terrain.shape(),
                        0.0,
                        1.0,
                        true,
                    );
                    let Some(hit) = require_supported_transition_cast(*part_id, cast_result)?
                    else {
                        continue;
                    };
                    if earliest.is_none_or(|(_, toi)| hit.time_of_impact < toi) {
                        earliest = Some((*part_id, hit.time_of_impact));
                    }
                }
            }
        }
        if let Some((part_id, time_of_impact)) = earliest {
            return Ok(ArticulatedTransition {
                blocked: true,
                part_id: Some(part_id),
                time_of_impact: Some(time_of_impact),
                terrain_candidates: terrain_candidate_count,
                cell_casts,
                query_sync_time,
                candidate_query_time,
                exact_cast_time: casts_started.elapsed(),
            });
        }

        // All input validation and continuous terrain casts succeeded. Stage
        // root and part body targets together; both version tokens publish at
        // the same following physics boundary.
        self.bodies[root_body].set_next_kinematic_position(target_root);
        for (body, _, _, _, body_target) in &targets {
            self.bodies[*body].set_next_kinematic_position(*body_target);
        }
        let pending = locals
            .into_iter()
            .zip(flattened)
            .map(|((part_id, local), world)| (part_id, local, world))
            .collect();
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("validated assembly exists");
        assembly.pending_root_pose_token = Some(update.pose_token);
        assembly.pending_pose = Some(PendingArticulatedPose {
            pose_token: update.pose_token,
            parts: pending,
        });
        Ok(ArticulatedTransition {
            blocked: false,
            part_id: None,
            time_of_impact: None,
            terrain_candidates: terrain_candidate_count,
            cell_casts,
            query_sync_time,
            candidate_query_time,
            exact_cast_time: casts_started.elapsed(),
        })
    }

    /// Retires the kinematic root and all of its attached source-part colliders.
    pub fn retire_articulated_body(&mut self, id: ArticulatedBodyId) {
        let entry_body = self
            .entries
            .get(entry_index(id.0, self.id_namespace))
            .map(|entry| entry.body);
        let part_bodies = self
            .articulated
            .get(&id.0)
            .filter(|assembly| {
                assembly.kinematic_part_bodies
                    || assembly
                        .parts
                        .iter()
                        .any(|part| part.component_id.is_some())
            })
            .map(|assembly| {
                assembly
                    .parts
                    .iter()
                    .map(|part| part.body)
                    .filter(|body| Some(*body) != entry_body)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let had_part_bodies = !part_bodies.is_empty();
        for body in part_bodies {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        if had_part_bodies {
            self.ccd_solver = CCDSolver::new();
        }
        self.retire_body(id.0);
    }

    /// Number of exact source-part colliders installed for an assembly.
    pub fn articulated_collider_count(&self, id: ArticulatedBodyId) -> usize {
        self.articulated
            .get(&id.0)
            .map_or(0, |assembly| assembly.parts.len())
    }

    /// Number of physical Rapier bodies used by an articulated candidate,
    /// including its empty conceptual root bookkeeping body when present.
    pub fn articulated_rigid_body_count(&self, id: ArticulatedBodyId) -> usize {
        self.articulated.get(&id.0).map_or(0, |assembly| {
            if assembly.kinematic_part_bodies {
                assembly.parts.len() + 1
            } else if assembly
                .parts
                .iter()
                .any(|part| part.component_id.is_some())
            {
                assembly
                    .parts
                    .iter()
                    .map(|part| part.body)
                    .collect::<HashSet<_>>()
                    .len()
            } else {
                1
            }
        })
    }

    /// Reads the assembly's primary rigid body state for bounded feasibility
    /// diagnostics. Kinematic rigs expose their stationary root; a corpse
    /// aggregate exposes its dynamic owner body.
    pub fn articulated_body_state(&self, id: ArticulatedBodyId) -> BodyState {
        self.body_state(id.0)
    }
}
