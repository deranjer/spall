use super::*;

impl PhysicsWorld {
    pub(super) fn prepare_corpse_restore_owner(
        &self,
        restore: crate::articulated::DynamicCorpseRestore,
        representation: Representation,
        density: &impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<PreparedCorpseRestoreOwner, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_PARTS,
            MAX_ARTICULATED_SOLID_CELLS,
        };
        use std::collections::{BTreeMap, BTreeSet};

        if restore.owner_key == 0
            || restore.source_generation == 0
            || restore.topology_revision == 0
            || restore.pose_epoch == 0
            || restore.next_component_serial == 0
            || restore.source_parts.is_empty()
            || restore.source_parts.len() > MAX_ARTICULATED_PARTS
            || restore.components.is_empty()
            || restore.components.len() > crate::fracture::MAX_FRACTURE_COMPONENTS
            || self.fracture_owner_keys.contains_key(&restore.owner_key)
            || self
                .articulated
                .values()
                .any(|assembly| assembly.fracture_owner_key == Some(restore.owner_key))
            || representation == Representation::SmoothConvex
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        let mut sources = restore.source_parts;
        sources.sort_by_key(|part| part.part_id);
        let mut source_part_ids = BTreeSet::new();
        let mut source_volumes = BTreeSet::new();
        let mut roots = 0;
        let mut source_dense = 0_u64;
        let mut source_solid = 0_u64;
        for part in &mut sources {
            if part.part_id == 0
                || !source_part_ids.insert(part.part_id)
                || !source_volumes.insert(part.volume_id)
                || !part.local_transform.is_valid()
                || part.source_revision == 0
                || !part.cell_m.is_finite()
                || part.cell_m <= 0.0
                || !articulated_grid_range_valid(&part.grid, part.cell_m)
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            roots += usize::from(part.parent_id.is_none());
            part.local_transform = part
                .local_transform
                .normalized()
                .ok_or(ArticulatedError::InvalidRestore)?;
            let dims = part.grid.dims();
            let dense = u64::from(dims[0])
                .checked_mul(u64::from(dims[1]))
                .and_then(|value| value.checked_mul(u64::from(dims[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            source_dense = source_dense
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            source_solid = source_solid
                .checked_add(part.grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if roots != 1
            || sources.iter().any(|part| {
                part.parent_id
                    .is_some_and(|parent| !source_part_ids.contains(&parent))
            })
            || source_dense > MAX_ARTICULATED_DENSE_CELLS
            || source_solid == 0
            || source_solid > MAX_ARTICULATED_SOLID_CELLS
        {
            return Err(ArticulatedError::InvalidRestore);
        }
        let frozen_poses = flatten_source_parts(&sources)?;
        if frozen_poses
            .iter()
            .any(|pose| !articulated_translation_in_range(pose.translation_m))
        {
            return Err(ArticulatedError::GeometryRange);
        }

        // A source VolumeId can be repeated by different derived components,
        // but never by two original source parts or another live owner.
        let live_volumes: BTreeSet<_> = self
            .articulated
            .values()
            .flat_map(|assembly| assembly.source_volume_ids.iter().copied())
            .collect();
        if source_volumes
            .iter()
            .any(|volume| live_volumes.contains(volume))
        {
            return Err(ArticulatedError::DuplicateVolume);
        }

        if restore.components.iter().any(|component| {
            component
                .component_id
                .is_some_and(|id| id.owner_key != restore.owner_key)
                || !component.translation_m.iter().all(|v| v.is_finite())
                || !component.rotation_xyzw.iter().all(|v| v.is_finite())
                || !component.linear_velocity_m_s.iter().all(|v| v.is_finite())
                || !component
                    .angular_velocity_rad_s
                    .iter()
                    .all(|v| v.is_finite())
                || !articulated_translation_in_range(component.translation_m)
                || !(PartTransform {
                    translation_m: [0.0; 3],
                    rotation_xyzw: component.rotation_xyzw,
                })
                .is_valid()
                || component.cells.is_empty()
        }) {
            return Err(ArticulatedError::InvalidRestore);
        }
        let intact = restore.components.len() == 1 && restore.components[0].component_id.is_none();
        if (intact && restore.next_component_serial != 1)
            || restore.components.iter().any(|component| {
                component.component_id.is_none() != intact
                    || component.component_id.is_some_and(|id| {
                        id.owner_key != restore.owner_key
                            || id.serial == 0
                            || id.serial >= restore.next_component_serial
                    })
            })
        {
            return Err(ArticulatedError::InvalidRestore);
        }
        let mut component_ids = BTreeSet::new();
        if restore
            .components
            .iter()
            .filter_map(|c| c.component_id)
            .any(|id| !component_ids.insert(id))
        {
            return Err(ArticulatedError::InvalidRestore);
        }

        // Read the canonical source occupancy once. Component masks are routed
        // through this index, so work scales with source cells plus mask cells,
        // not source cells multiplied by the number of components.
        let mut expected = BTreeMap::new();
        for (part_index, part) in sources.iter().enumerate() {
            let origin = part.grid.origin();
            let mut range_error = false;
            part.grid.for_each_solid(|x, y, z, material| {
                let xyz = [
                    origin.x.checked_add(i64::from(x)),
                    origin.y.checked_add(i64::from(y)),
                    origin.z.checked_add(i64::from(z)),
                ];
                let [Some(cx), Some(cy), Some(cz)] =
                    xyz.map(|coord| coord.and_then(|value| i32::try_from(value).ok()))
                else {
                    range_error = true;
                    return;
                };
                let key = SourceCellRef {
                    volume_id: part.volume_id,
                    part_id: part.part_id,
                    cell: [cx, cy, cz],
                    material,
                    source_revision: part.source_revision,
                };
                if expected.insert(key, (part_index, [x, y, z])).is_some() {
                    range_error = true;
                }
            });
            if range_error {
                return Err(ArticulatedError::GeometryRange);
            }
        }
        let mut assigned = BTreeSet::new();
        let mut component_owned = Vec::with_capacity(restore.components.len());
        for component in &restore.components {
            let mut local = BTreeSet::new();
            let mut by_part = vec![Vec::new(); sources.len()];
            for cell in &component.cells {
                let Some((part_index, local_coord)) = expected.get(cell).copied() else {
                    return Err(ArticulatedError::InvalidRestore);
                };
                if !local.insert(*cell) || !assigned.insert(*cell) {
                    return Err(ArticulatedError::InvalidRestore);
                }
                by_part[part_index].push((
                    local_coord[0],
                    local_coord[1],
                    local_coord[2],
                    cell.material,
                ));
            }
            component_owned.push(by_part);
        }
        if assigned.len() != expected.len() {
            return Err(ArticulatedError::InvalidRestore);
        }

        let manifest = density_manifest(&sources, &density)?;
        let stable_density = |material| manifest_density(&manifest, material);
        let mut prepared = Vec::with_capacity(restore.components.len());
        let mut primitive_count = 0_usize;
        let mut derived_dense = 0_u64;
        let mut total_shards = 0_usize;
        for (component_index, component) in restore.components.iter().enumerate() {
            let rotation = normalize(component.rotation_xyzw);
            let mut source_shards = Vec::new();
            let mut flat_poses = Vec::new();
            let mut parts = Vec::new();
            for (part_index, (part, frozen_pose)) in sources.iter().zip(&frozen_poses).enumerate() {
                let origin = part.grid.origin();
                let owned = &component_owned[component_index][part_index];
                if owned.is_empty() {
                    continue;
                }
                let min = std::array::from_fn::<_, 3, _>(|axis| {
                    owned.iter().map(|c| [c.0, c.1, c.2][axis]).min().unwrap()
                });
                let max = std::array::from_fn::<_, 3, _>(|axis| {
                    owned.iter().map(|c| [c.0, c.1, c.2][axis]).max().unwrap()
                });
                let dims = [
                    max[0] - min[0] + 1,
                    max[1] - min[1] + 1,
                    max[2] - min[2] + 1,
                ];
                let dense = u64::from(dims[0])
                    .checked_mul(u64::from(dims[1]))
                    .and_then(|v| v.checked_mul(u64::from(dims[2])))
                    .ok_or(ArticulatedError::WorkBudget)?;
                derived_dense = derived_dense
                    .checked_add(dense)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if derived_dense > MAX_ARTICULATED_DENSE_CELLS {
                    return Err(ArticulatedError::WorkBudget);
                }
                let volume = usize::try_from(dense).map_err(|_| ArticulatedError::WorkBudget)?;
                let mut mask = vec![false; volume];
                let mut materials = vec![MaterialId::AIR; volume];
                for &(x, y, z, material) in owned {
                    let local = [x - min[0], y - min[1], z - min[2]];
                    let index = local[0] as usize
                        + dims[0] as usize
                            * (local[1] as usize + dims[1] as usize * local[2] as usize);
                    mask[index] = true;
                    materials[index] = material;
                }
                let grid = OccupancyGrid::from_solid_mask(
                    GlobalCell::new(
                        origin.x + i64::from(min[0]),
                        origin.y + i64::from(min[1]),
                        origin.z + i64::from(min[2]),
                    ),
                    dims,
                    mask,
                    materials,
                )
                .map_err(|_| ArticulatedError::InvalidRestore)?;
                source_shards.push(SourcePart {
                    part_id: part.part_id,
                    parent_id: None,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    grid: grid.clone(),
                    cell_m: part.cell_m,
                    local_transform: *frozen_pose,
                });
                flat_poses.push(*frozen_pose);
                let built = build_collider(&grid, part.cell_m, representation);
                primitive_count = primitive_count
                    .checked_add(built.primitives)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
                    || primitive_count > 65_536
                {
                    return Err(ArticulatedError::WorkBudget);
                }
                let origin = grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(frozen_pose.rotation_xyzw, origin_m);
                let local_translation = [
                    frozen_pose.translation_m[0] + offset[0],
                    frozen_pose.translation_m[1] + offset[1],
                    frozen_pose.translation_m[2] + offset[2],
                ];
                let world_offset = rotate(rotation, local_translation);
                if !articulated_translation_in_range(local_translation)
                    || !articulated_translation_in_range([
                        component.translation_m[0] + world_offset[0],
                        component.translation_m[1] + world_offset[1],
                        component.translation_m[2] + world_offset[2],
                    ])
                {
                    return Err(ArticulatedError::GeometryRange);
                }
                let local_pose = Pose::new(
                    Vector::new(
                        local_translation[0],
                        local_translation[1],
                        local_translation[2],
                    ),
                    Rotation::from_xyzw(
                        frozen_pose.rotation_xyzw[0],
                        frozen_pose.rotation_xyzw[1],
                        frozen_pose.rotation_xyzw[2],
                        frozen_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local_pose)
                    .build();
                total_shards = total_shards
                    .checked_add(1)
                    .ok_or(ArticulatedError::WorkBudget)?;
                if total_shards > MAX_ARTICULATED_PARTS {
                    return Err(ArticulatedError::WorkBudget);
                }
                parts.push((
                    PreparedCorpsePartIdentity {
                        part_id: part.part_id,
                        volume_id: part.volume_id,
                        source_revision: part.source_revision,
                        cell_m: part.cell_m,
                    },
                    grid,
                    *frozen_pose,
                    built.primitives,
                    collider,
                ));
            }
            if parts.is_empty() {
                return Err(ArticulatedError::EmptyAggregate);
            }
            let mass = crate::mass::articulated_mass_properties(
                &source_shards,
                &flat_poses,
                stable_density,
            )
            .ok_or(ArticulatedError::InvalidMassProperties)?
            .to_body_properties();
            validate_body_mass_properties(mass)?;
            let translation = component.translation_m;
            if !articulated_translation_in_range(translation) {
                return Err(ArticulatedError::GeometryRange);
            }
            prepared.push((
                PreparedCorpseComponentState {
                    component_id: component.component_id,
                    linear_velocity_m_s: component.linear_velocity_m_s,
                    angular_velocity_rad_s: component.angular_velocity_rad_s,
                },
                mass,
                rotation,
                translation,
                parts,
            ));
        }

        Ok(PreparedCorpseRestoreOwner {
            owner_key: restore.owner_key,
            source_generation: restore.source_generation,
            topology_revision: restore.topology_revision,
            pose_epoch: restore.pose_epoch,
            next_component_serial: restore.next_component_serial,
            source_volumes,
            manifest,
            prepared,
            representation,
            primitive_count,
            derived_dense,
        })
    }

    pub(super) fn publish_corpse_restore_owner(
        &mut self,
        restore: PreparedCorpseRestoreOwner,
    ) -> ArticulatedBodyId {
        let PreparedCorpseRestoreOwner {
            owner_key,
            source_generation,
            topology_revision,
            pose_epoch,
            next_component_serial,
            source_volumes,
            manifest,
            prepared,
            representation,
            ..
        } = restore;
        let first_translation = prepared[0].3;
        let first_rotation = prepared[0].2;
        let first_mass = prepared[0].1;
        let first_linear = prepared[0].0.linear_velocity_m_s;
        let first_angular = prepared[0].0.angular_velocity_rad_s;
        let first_body = self.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(Vector::new(
                    first_translation[0],
                    first_translation[1],
                    first_translation[2],
                ))
                .rotation(
                    Rotation::from_xyzw(
                        first_rotation[0],
                        first_rotation[1],
                        first_rotation[2],
                        first_rotation[3],
                    )
                    .to_scaled_axis(),
                )
                .additional_mass_properties(rapier_mass_properties(first_mass, [0.0; 3]))
                .linvel(Vector::from(first_linear))
                .angvel(Vector::from(first_angular))
                .build(),
        );
        let id = BodyId((u64::from(self.id_namespace) << 32) | self.entries.len() as u64);
        let mut built_parts = Vec::new();
        let mut primary_collider = None;
        let mut primary_cell_m = 1.0;
        let mut primary_origin = [0.0; 3];
        let mut first_body_handle = first_body;
        for (component_index, (component, mass, rotation, translation, parts)) in
            prepared.into_iter().enumerate()
        {
            let body = if component_index == 0 {
                first_body_handle
            } else {
                self.bodies.insert(
                    RigidBodyBuilder::dynamic()
                        .translation(Vector::new(translation[0], translation[1], translation[2]))
                        .rotation(
                            Rotation::from_xyzw(rotation[0], rotation[1], rotation[2], rotation[3])
                                .to_scaled_axis(),
                        )
                        .additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]))
                        .linvel(Vector::from(component.linear_velocity_m_s))
                        .angvel(Vector::from(component.angular_velocity_rad_s))
                        .build(),
                )
            };
            if component_index == 0 {
                first_body_handle = body;
            }
            let component_id = component.component_id;
            for (part, grid, frozen_pose, primitives, collider) in parts {
                let collider = self
                    .colliders
                    .insert_with_parent(collider, body, &mut self.bodies);
                self.tag_collider(collider, id);
                self.pending_modified.push(collider);
                if primary_collider.is_none() {
                    primary_collider = Some(collider);
                    primary_cell_m = part.cell_m;
                    let origin = grid.origin();
                    primary_origin = [
                        origin.x as f32 * part.cell_m,
                        origin.y as f32 * part.cell_m,
                        origin.z as f32 * part.cell_m,
                    ];
                }
                built_parts.push(ArticulatedPartEntry {
                    part_id: part.part_id,
                    parent_id: None,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    grid: grid.clone(),
                    cell_m: part.cell_m,
                    local_pose: frozen_pose,
                    assembly_pose: frozen_pose,
                    part_surface_radius_m: source_surface_radius_m(
                        &grid,
                        part.cell_m,
                        [0.0; 3],
                        None,
                    ),
                    root_surface_radius_m: source_surface_radius_m(
                        &grid,
                        part.cell_m,
                        [0.0; 3],
                        Some(frozen_pose),
                    ),
                    body,
                    collider,
                    primitives,
                    component_id,
                });
            }
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[body]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[body].set_linvel(Vector::from(component.linear_velocity_m_s), true);
            self.bodies[body].set_angvel(Vector::from(component.angular_velocity_rad_s), true);
        }
        self.entries.push(Entry {
            body: first_body_handle,
            collider: primary_collider.expect("nonempty prepared restore has a primary collider"),
            cell_m: primary_cell_m,
            representation,
            density: 0.0,
            collider_offset_m: primary_origin,
            mass_properties: Some(first_mass),
            retired: false,
            dormant: false,
            query_only: false,
            restitution: 0.0,
        });
        self.articulated.insert(
            id,
            ArticulatedEntry {
                topology_token: topology_revision,
                pose_token: pose_epoch,
                root_pose: PartTransform {
                    translation_m: first_translation,
                    rotation_xyzw: first_rotation,
                },
                pending_root_pose_token: None,
                pending_pose: None,
                kinematic_part_bodies: false,
                velocity_based_restore: false,
                dynamic_corpse: true,
                source_generation,
                fracture_owner_key: Some(owner_key),
                next_component_serial,
                density_manifest: Some(manifest),
                source_volume_ids: source_volumes,
                parts: built_parts,
            },
        );
        self.fracture_owner_keys.insert(owner_key, id);
        self.ccd_solver = CCDSolver::new();
        ArticulatedBodyId(id)
    }

    /// Converts the existing ENG-210 multi-body kinematic owner in place. It
    /// derives exact source-cell mass and momenta from each part body's current
    /// pose and velocity; an invalid/stale/pending transfer mutates nothing.
    pub fn transfer_articulated_to_dynamic_corpse(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseTransferOutcome, crate::articulated::ArticulatedError> {
        self.transfer_articulated_to_dynamic_corpse_checked(
            id,
            expected_topology_token,
            expected_pose_token,
            density,
            |_| true,
        )
    }

    /// As [`Self::transfer_articulated_to_dynamic_corpse`], with a final
    /// value-only owner validation hook. The hook runs after exact mass,
    /// resultant motion, and every replacement shape are prepared, but before
    /// any collider/body ownership mutation. Returning false rejects atomically.
    pub fn transfer_articulated_to_dynamic_corpse_checked(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
        validate_final: impl FnOnce(&CorpseTransferOutcome) -> bool,
    ) -> Result<CorpseTransferOutcome, crate::articulated::ArticulatedError> {
        let prepared = self.prepare_articulated_batch(
            vec![ArticulatedBatchOperation::CheckedDeathTransfer {
                operation_tag: 0,
                id,
                expected_topology_token,
                expected_pose_token,
            }],
            density,
        )?;
        let transfer_outcome = prepared
            .changes
            .iter()
            .find_map(|change| {
                if let PreparedArticulatedChange::CheckedDeathTransfer { transfer, .. } = change {
                    Some(&transfer.outcome)
                } else {
                    None
                }
            })
            .expect("singleton transfer prepares one transfer");
        let accepted = validate_final(transfer_outcome);
        if !accepted {
            return Err(crate::articulated::ArticulatedError::TransferRejected);
        }
        let mut outcomes = self.commit_articulated_batch(prepared)?;
        Ok(outcomes
            .pop()
            .and_then(|outcome| outcome.transfer)
            .expect("singleton transfer includes its witness"))
    }

    pub(super) fn prepare_dynamic_corpse_transfer(
        &self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        expected_pose_token: u64,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<PreparedCorpseTransfer, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let started = Instant::now();
        let body_id = id.0;
        let assembly = self
            .articulated
            .get(&body_id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.kinematic_part_bodies || assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.topology_token != expected_topology_token
            || assembly.pose_token != expected_pose_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if assembly.pending_root_pose_token.is_some() || assembly.pending_pose.is_some() {
            return Err(ArticulatedError::PendingRootPose);
        }
        let other_volumes: std::collections::BTreeSet<_> = self
            .articulated
            .iter()
            .filter(|(owner, _)| **owner != body_id)
            .flat_map(|(_, a)| a.source_volume_ids.iter().copied())
            .collect();
        if assembly.source_volume_ids.is_empty()
            || assembly
                .source_volume_ids
                .iter()
                .any(|volume| other_volumes.contains(volume))
        {
            return Err(ArticulatedError::DuplicateVolume);
        }
        if assembly.parts.is_empty()
            || assembly
                .parts
                .iter()
                .map(|p| p.grid.solid_count())
                .sum::<u64>()
                > crate::articulated::MAX_ARTICULATED_SOLID_CELLS
            || assembly.parts.iter().map(|p| p.primitives).sum::<usize>() > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }

        let source_parts = assembly
            .parts
            .iter()
            .map(|p| SourcePart {
                part_id: p.part_id,
                parent_id: p.parent_id,
                volume_id: p.volume_id,
                source_revision: p.source_revision,
                grid: p.grid.clone(),
                cell_m: p.cell_m,
                local_transform: p.local_pose,
            })
            .collect::<Vec<_>>();
        let manifest = density_manifest(&source_parts, &density)?;
        if assembly.density_manifest.as_ref().is_some_and(|bound| {
            manifest
                .iter()
                .any(|(material, bits)| bound.get(material) != Some(bits))
        }) {
            return Err(ArticulatedError::InvalidDensity);
        }
        let local_poses = source_parts
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let stable_density = |material| manifest_density(&manifest, material);
        let mass64 =
            crate::mass::articulated_mass_properties(&source_parts, &local_poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?;
        let mass = mass64.to_body_properties();
        validate_body_mass_properties(mass)?;

        let entry_index = entry_index(body_id, self.id_namespace);
        let root_body = self.entries[entry_index].body;
        let root_pose = *self.bodies[root_body].position();
        let mut cell_samples = Vec::with_capacity(
            assembly
                .parts
                .iter()
                .map(|p| p.grid.solid_count())
                .sum::<u64>() as usize,
        );
        let mut total_mass = 0.0_f64;
        let mut first_moment = [0.0_f64; 3];
        let mut linear_momentum = [0.0_f64; 3];
        for part in &assembly.parts {
            let rb = &self.bodies[part.body];
            let pose = *rb.position();
            let linear = rb.linvel();
            let angular = rb.angvel();
            if ![
                linear.x, linear.y, linear.z, angular.x, angular.y, angular.z,
            ]
            .iter()
            .all(|v| v.is_finite())
            {
                return Err(ArticulatedError::InvalidTransform);
            }
            let origin = part.grid.origin();
            let s = part.cell_m;
            let mut density_error = false;
            part.grid.for_each_solid(|x, y, z, material| {
                let rho = stable_density(material);
                if !rho.is_finite() || rho <= 0.0 {
                    density_error = true;
                    return;
                }
                let m = rho * f64::from(s).powi(3);
                let local = Vector::new(
                    (origin.x as f32 + x as f32 + 0.5) * s,
                    (origin.y as f32 + y as f32 + 0.5) * s,
                    (origin.z as f32 + z as f32 + 0.5) * s,
                );
                let point = pose.transform_point(local);
                let r = point - pose.translation;
                let velocity = linear + angular.cross(r);
                let position = [point.x as f64, point.y as f64, point.z as f64];
                let v = [velocity.x as f64, velocity.y as f64, velocity.z as f64];
                total_mass += m;
                for axis in 0..3 {
                    first_moment[axis] += m * position[axis];
                    linear_momentum[axis] += m * v[axis];
                }
                cell_samples.push((
                    position,
                    v,
                    m,
                    f64::from(s),
                    [
                        f64::from(angular.x),
                        f64::from(angular.y),
                        f64::from(angular.z),
                    ],
                ));
            });
            if density_error {
                return Err(ArticulatedError::InvalidDensity);
            }
        }
        if !total_mass.is_finite() || (total_mass - mass64.mass_kg).abs() > mass64.mass_kg * 1e-5 {
            return Err(ArticulatedError::InvalidMassProperties);
        }
        let com_world = first_moment.map(|v| v / total_mass);
        let mut inertia_world = [[0.0_f64; 3]; 3];
        let mut angular_momentum = [0.0_f64; 3];
        for (position, velocity, m, s, omega) in cell_samples {
            let r = [
                position[0] - com_world[0],
                position[1] - com_world[1],
                position[2] - com_world[2],
            ];
            let p = velocity.map(|v| v * m);
            let spin = cross64(r, p);
            for axis in 0..3 {
                angular_momentum[axis] += spin[axis];
            }
            let r2 = r.iter().map(|v| v * v).sum::<f64>();
            for a in 0..3 {
                for b in 0..3 {
                    let diagonal = if a == b { 1.0 } else { 0.0 };
                    inertia_world[a][b] +=
                        m * s * s / 6.0 * diagonal + m * (diagonal * r2 - r[a] * r[b]);
                }
            }
            // The body's point velocity contains only orbital motion; a cube's
            // spin contribution is its isotropic self-inertia times omega.
            let self_i = m * s * s / 6.0;
            for axis in 0..3 {
                angular_momentum[axis] += self_i * omega[axis];
            }
        }
        let omega_world = solve_symmetric3(inertia_world, angular_momentum)
            .ok_or(ArticulatedError::InvalidMassProperties)?;
        let linear_velocity = linear_momentum.map(|v| v / total_mass);
        let com_local = root_pose.inverse_transform_point(Vector::new(
            com_world[0] as f32,
            com_world[1] as f32,
            com_world[2] as f32,
        ));
        let props = BodyMassProperties {
            mass_kg: mass.mass_kg,
            local_com_m: [com_local.x, com_local.y, com_local.z],
            inertia_com_kg_m2: mass.inertia_com_kg_m2,
        };
        validate_body_mass_properties(props)?;

        // Prepare every replacement collider and its root-local pose before
        // changing ownership. Insertion is infallible; no fallible check below
        // this point can leave a partially transferred rig.
        let prepared = assembly
            .parts
            .iter()
            .map(|part| {
                let origin = part.grid.origin();
                let origin_m = [
                    origin.x as f32 * part.cell_m,
                    origin.y as f32 * part.cell_m,
                    origin.z as f32 * part.cell_m,
                ];
                let offset = rotate(part.assembly_pose.rotation_xyzw, origin_m);
                let translation = [
                    part.assembly_pose.translation_m[0] + offset[0],
                    part.assembly_pose.translation_m[1] + offset[1],
                    part.assembly_pose.translation_m[2] + offset[2],
                ];
                let pose = Pose::new(
                    Vector::new(translation[0], translation[1], translation[2]),
                    Rotation::from_xyzw(
                        part.assembly_pose.rotation_xyzw[0],
                        part.assembly_pose.rotation_xyzw[1],
                        part.assembly_pose.rotation_xyzw[2],
                        part.assembly_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                (part.collider, part.body, pose)
            })
            .collect::<Vec<_>>();
        let prepared_colliders = prepared
            .iter()
            .map(|(old, _, pose)| {
                let old_collider = &self.colliders[*old];
                let restitution = old_collider.restitution();
                let restitution_rule = old_collider.restitution_combine_rule();
                let friction = old_collider.friction();
                let friction_rule = old_collider.friction_combine_rule();
                let collision_groups = old_collider.collision_groups();
                let solver_groups = old_collider.solver_groups();
                let active_collision_types = old_collider.active_collision_types();
                let sensor = old_collider.is_sensor();
                let mut collider = ColliderBuilder::new(old_collider.shared_shape().clone())
                    .density(0.0)
                    .position(*pose)
                    .friction(friction)
                    .active_collision_types(active_collision_types)
                    .sensor(sensor)
                    .build();
                collider.set_restitution(restitution);
                collider.set_restitution_combine_rule(restitution_rule);
                collider.set_friction_combine_rule(friction_rule);
                collider.set_collision_groups(collision_groups);
                collider.set_solver_groups(solver_groups);
                collider
            })
            .collect::<Vec<_>>();
        let mut outcome = CorpseTransferOutcome {
            preparation_time: started.elapsed(),
            mass_kg: total_mass,
            com_world_m: com_world,
            linear_momentum_kg_m_s: linear_momentum,
            angular_momentum_about_com_kg_m2_s: angular_momentum,
            linear_velocity_m_s: linear_velocity,
            angular_velocity_rad_s: omega_world,
        };
        let source_volume_ids = assembly.source_volume_ids.clone();
        let primitive_count = assembly.parts.iter().try_fold(0_usize, |total, part| {
            total
                .checked_add(part.primitives)
                .ok_or(ArticulatedError::WorkBudget)
        })?;
        let snapshots = assembly
            .parts
            .iter()
            .map(|part| {
                let body = &self.bodies[part.body];
                let mut source_cells = Vec::with_capacity(part.grid.solid_count() as usize);
                part.grid
                    .for_each_solid(|x, y, z, material| source_cells.push((x, y, z, material)));
                PreparedCorpseTransferPartSnapshot {
                    body: part.body,
                    collider: part.collider,
                    position: *body.position(),
                    linvel: body.linvel(),
                    angvel: body.angvel(),
                    part_id: part.part_id,
                    volume_id: part.volume_id,
                    source_revision: part.source_revision,
                    cell_m: part.cell_m,
                    grid_origin: part.grid.origin(),
                    grid_dims: part.grid.dims(),
                    source_solid_count: part.grid.solid_count(),
                    source_cells,
                    local_pose: part.local_pose,
                    assembly_pose: part.assembly_pose,
                }
            })
            .collect();
        let mut old_bodies = prepared
            .iter()
            .map(|(_, body, _)| *body)
            .collect::<Vec<_>>();
        old_bodies.sort_by_key(|body| body.into_raw_parts());
        old_bodies.dedup();
        outcome.preparation_time = started.elapsed();
        Ok(PreparedCorpseTransfer {
            id,
            expected_topology_token,
            expected_pose_token,
            source_generation: assembly.source_generation,
            source_volume_ids,
            parts: snapshots,
            entry_body: root_body,
            entry_position: *self.bodies[root_body].position(),
            entry_linvel: self.bodies[root_body].linvel(),
            entry_angvel: self.bodies[root_body].angvel(),
            entry_collider: self.entries[entry_index].collider,
            props,
            manifest,
            prepared,
            replacement_colliders: prepared_colliders,
            primitive_count,
            outcome,
        })
    }

    pub(super) fn validate_prepared_corpse_transfer(
        &self,
        transfer: &PreparedCorpseTransfer,
    ) -> bool {
        let body_id = transfer.id.0;
        let Some(assembly) = self.articulated.get(&body_id) else {
            return false;
        };
        if !assembly.kinematic_part_bodies
            || assembly.dynamic_corpse
            || assembly.topology_token != transfer.expected_topology_token
            || assembly.pose_token != transfer.expected_pose_token
            || assembly.source_generation != transfer.source_generation
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
            || assembly.source_volume_ids != transfer.source_volume_ids
            || assembly.parts.len() != transfer.parts.len()
        {
            return false;
        }
        let index = entry_index(body_id, self.id_namespace);
        let Some(entry) = self.entries.get(index) else {
            return false;
        };
        if entry.body != transfer.entry_body || entry.collider != transfer.entry_collider {
            return false;
        }
        let Some(entry_body) = self.bodies.get(transfer.entry_body) else {
            return false;
        };
        if *entry_body.position() != transfer.entry_position
            || entry_body.linvel() != transfer.entry_linvel
            || entry_body.angvel() != transfer.entry_angvel
        {
            return false;
        }
        for (part, snapshot) in assembly.parts.iter().zip(&transfer.parts) {
            let Some(body) = self.bodies.get(part.body) else {
                return false;
            };
            if part.body != snapshot.body
                || part.collider != snapshot.collider
                || part.part_id != snapshot.part_id
                || part.volume_id != snapshot.volume_id
                || part.source_revision != snapshot.source_revision
                || part.cell_m.to_bits() != snapshot.cell_m.to_bits()
                || part.grid.origin() != snapshot.grid_origin
                || part.grid.dims() != snapshot.grid_dims
                || part.grid.solid_count() != snapshot.source_solid_count
                || part.local_pose.translation_m != snapshot.local_pose.translation_m
                || part.local_pose.rotation_xyzw != snapshot.local_pose.rotation_xyzw
                || part.assembly_pose.translation_m != snapshot.assembly_pose.translation_m
                || part.assembly_pose.rotation_xyzw != snapshot.assembly_pose.rotation_xyzw
                || *body.position() != snapshot.position
                || body.linvel() != snapshot.linvel
                || body.angvel() != snapshot.angvel
            {
                return false;
            }
            let mut idx = 0usize;
            let mut same = true;
            part.grid.for_each_solid(|x, y, z, material| {
                if snapshot.source_cells.get(idx).copied() != Some((x, y, z, material)) {
                    same = false;
                }
                idx += 1;
            });
            if !same || idx != snapshot.source_cells.len() {
                return false;
            }
        }
        true
    }

    pub(super) fn publish_prepared_corpse_transfer(
        &mut self,
        transfer: PreparedCorpseTransfer,
    ) -> (ArticulatedBodyId, CorpseTransferOutcome) {
        let PreparedCorpseTransfer {
            id,
            props,
            manifest,
            prepared,
            replacement_colliders,
            outcome,
            ..
        } = transfer;
        let body_id = id.0;
        let index = entry_index(body_id, self.id_namespace);
        let root_body = self.entries[index].body;
        let mut replacement_handles = Vec::with_capacity(prepared.len());
        for collider in replacement_colliders {
            let handle = self
                .colliders
                .insert_with_parent(collider, root_body, &mut self.bodies);
            self.tag_collider(handle, body_id);
            replacement_handles.push(handle);
        }
        let old_root_pose = *self.bodies[root_body].position();
        self.bodies[root_body].set_body_type(RigidBodyType::Dynamic, true);
        self.bodies[root_body]
            .set_additional_mass_properties(rapier_mass_properties(props, [0.0; 3]), true);
        self.bodies[root_body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[root_body].set_position(old_root_pose, true);
        self.bodies[root_body].set_linvel(
            Vector::new(
                outcome.linear_velocity_m_s[0] as f32,
                outcome.linear_velocity_m_s[1] as f32,
                outcome.linear_velocity_m_s[2] as f32,
            ),
            true,
        );
        self.bodies[root_body].set_angvel(
            Vector::new(
                outcome.angular_velocity_rad_s[0] as f32,
                outcome.angular_velocity_rad_s[1] as f32,
                outcome.angular_velocity_rad_s[2] as f32,
            ),
            true,
        );
        let mut old_bodies = prepared
            .iter()
            .map(|(_, body, _)| *body)
            .collect::<Vec<_>>();
        old_bodies.sort_by_key(|body| body.into_raw_parts());
        old_bodies.dedup();
        for (old, _, _) in &prepared {
            self.colliders
                .remove(*old, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(*old);
        }
        for old in old_bodies {
            self.bodies.remove(
                old,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
        }
        self.ccd_solver = CCDSolver::new();
        self.pending_modified
            .extend(replacement_handles.iter().copied());
        let assembly = self
            .articulated
            .get_mut(&body_id)
            .expect("batch transfer was prevalidated");
        for (part, handle) in assembly
            .parts
            .iter_mut()
            .zip(replacement_handles.iter().copied())
        {
            part.body = root_body;
            part.collider = handle;
        }
        assembly.kinematic_part_bodies = false;
        assembly.velocity_based_restore = false;
        assembly.dynamic_corpse = true;
        assembly.density_manifest = Some(manifest);
        let entry = &mut self.entries[index];
        entry.body = root_body;
        entry.collider = replacement_handles[0];
        entry.collider_offset_m = [0.0; 3];
        entry.density = 0.0;
        entry.mass_properties = Some(props);
        (id, outcome)
    }
}
