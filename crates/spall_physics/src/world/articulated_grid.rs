use super::*;

impl PhysicsWorld {
    /// Rebuilds one source-part shape after an authoritative cell edit. The
    /// new collider is fully prepared and budget-checked before removing the
    /// old shape; stale topology/revision tokens leave the old shape untouched.
    pub fn rebuild_articulated_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
    ) -> Result<Duration, crate::articulated::ArticulatedError> {
        self.rebuild_articulated_part_grid_impl(
            id,
            expected_topology_token,
            new_topology_token,
            part_id,
            expected_source_revision,
            new_source_revision,
            new_grid,
            false,
        )
    }

    fn rebuild_articulated_part_grid_impl(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        dynamic_allowed: bool,
    ) -> Result<Duration, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        let id = id.0;
        let assembly = self
            .articulated
            .get(&id)
            .ok_or(ArticulatedError::Hierarchy)?;
        if assembly.dynamic_corpse != dynamic_allowed {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        let part_index = assembly
            .parts
            .iter()
            .position(|part| part.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let part = &assembly.parts[part_index];
        if assembly.topology_token != expected_topology_token
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
            || new_topology_token <= expected_topology_token
            || part.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if !articulated_grid_range_valid(&new_grid, part.cell_m) {
            return Err(ArticulatedError::GeometryRange);
        }
        let dims = new_grid.dims();
        let dense = u64::from(dims[0])
            .checked_mul(u64::from(dims[1]))
            .and_then(|v| v.checked_mul(u64::from(dims[2])))
            .ok_or(ArticulatedError::WorkBudget)?;
        let old_dense = u64::from(part.grid.dims()[0])
            .checked_mul(u64::from(part.grid.dims()[1]))
            .and_then(|v| v.checked_mul(u64::from(part.grid.dims()[2])))
            .ok_or(ArticulatedError::WorkBudget)?;
        let dense_total = assembly
            .parts
            .iter()
            .try_fold(0u64, |sum, part| {
                let dims = part.grid.dims();
                sum.checked_add(
                    u64::from(dims[0])
                        .checked_mul(u64::from(dims[1]))?
                        .checked_mul(u64::from(dims[2]))?,
                )
            })
            .ok_or(ArticulatedError::WorkBudget)?;
        let solid_total = assembly
            .parts
            .iter()
            .try_fold(0u64, |sum, part| sum.checked_add(part.grid.solid_count()))
            .ok_or(ArticulatedError::WorkBudget)?;
        let new_dense_total = dense_total
            .checked_sub(old_dense)
            .and_then(|v| v.checked_add(dense))
            .ok_or(ArticulatedError::WorkBudget)?;
        let new_solid_total = solid_total
            .checked_sub(part.grid.solid_count())
            .and_then(|v| v.checked_add(new_grid.solid_count()))
            .ok_or(ArticulatedError::WorkBudget)?;
        if new_dense_total > MAX_ARTICULATED_DENSE_CELLS
            || new_solid_total > MAX_ARTICULATED_SOLID_CELLS
        {
            return Err(ArticulatedError::WorkBudget);
        }
        if new_grid.solid_count() == 0 {
            return Err(ArticulatedError::EmptyPart(part_id));
        }
        let part_surface_radius_m = source_surface_radius_m(&new_grid, part.cell_m, [0.0; 3], None);
        let root_surface_radius_m =
            source_surface_radius_m(&new_grid, part.cell_m, [0.0; 3], Some(part.assembly_pose));
        let representation = self.entries[entry_index(id, self.id_namespace)].representation;
        let start = Instant::now();
        let built = build_collider(&new_grid, part.cell_m, representation);
        if built.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET
            || assembly
                .parts
                .iter()
                .map(|part| part.primitives)
                .sum::<usize>()
                - part.primitives
                + built.primitives
                > 65_536
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let origin = new_grid.origin();
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
        if !translation.iter().all(|value| value.is_finite()) {
            return Err(ArticulatedError::InvalidTransform);
        }
        let root = self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        let world_offset =
            root.transform_vector(Vector::new(translation[0], translation[1], translation[2]));
        let world_position = root.translation + world_offset;
        if !world_position.x.is_finite()
            || !world_position.y.is_finite()
            || !world_position.z.is_finite()
        {
            return Err(ArticulatedError::InvalidTransform);
        }
        let rotation = Rotation::from_xyzw(
            part.assembly_pose.rotation_xyzw[0],
            part.assembly_pose.rotation_xyzw[1],
            part.assembly_pose.rotation_xyzw[2],
            part.assembly_pose.rotation_xyzw[3],
        );
        let entry = &self.entries[entry_index(id, self.id_namespace)];
        let (mut body, query_only, restitution) = (entry.body, entry.query_only, entry.restitution);
        let kinematic_part_bodies = assembly.kinematic_part_bodies;
        let next_source_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(ArticulatedError::WorkBudget)?;
        if kinematic_part_bodies {
            body = part.body;
        }
        let collider_pose = if kinematic_part_bodies {
            Pose::from_translation(Vector::from(origin_m))
        } else {
            Pose::new(
                Vector::new(translation[0], translation[1], translation[2]),
                rotation.to_scaled_axis(),
            )
        };
        let mut collider_builder = ColliderBuilder::new(built.collider.shared_shape().clone())
            .density(0.0)
            .position(collider_pose);
        if kinematic_part_bodies {
            collider_builder = collider_builder.active_collision_types(
                ActiveCollisionTypes::default() | ActiveCollisionTypes::KINEMATIC_FIXED,
            );
        }
        let mut collider = collider_builder.build();
        if query_only {
            collider.set_solver_groups(query_only_solver_groups());
        }
        collider.set_restitution(restitution);
        collider.set_restitution_combine_rule(CoefficientCombineRule::Max);

        let old_handle = part.collider;
        self.colliders
            .remove(old_handle, &mut self.islands, &mut self.bodies, true);
        let new_handle = self
            .colliders
            .insert_with_parent(collider, body, &mut self.bodies);
        self.tag_collider(new_handle, id);
        self.pending_removed.push(old_handle);
        self.pending_modified.push(new_handle);
        let assembly = self
            .articulated
            .get_mut(&id)
            .expect("assembly validated above");
        let part = &mut assembly.parts[part_index];
        part.grid = new_grid;
        part.source_revision = new_source_revision;
        part.part_surface_radius_m = part_surface_radius_m;
        part.root_surface_radius_m = root_surface_radius_m;
        part.collider = new_handle;
        part.primitives = built.primitives;
        assembly.topology_token = new_topology_token;
        assembly.source_generation = next_source_generation;
        if part_index == 0 {
            self.entries[entry_index(id, self.id_namespace)].collider = new_handle;
        }
        Ok(start.elapsed())
    }

    /// Removes source cells from exactly one live fracture component. The
    /// stable key is mandatory so a source part split across bodies cannot be
    /// edited by accidental vector order. Additions and material replacement
    /// remain unsupported until their incoming momentum policy is defined.
    pub fn rebuild_dynamic_corpse_component_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        component_id: DerivedFractureComponentId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseEditOutcome, crate::articulated::ArticulatedError> {
        use crate::articulated::{
            ArticulatedError, MAX_ARTICULATED_DENSE_CELLS, MAX_ARTICULATED_SOLID_CELLS,
        };
        let started = Instant::now();
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly.fracture_owner_key != Some(component_id.owner_key)
            || component_id.serial == 0
            || component_id.serial >= assembly.next_component_serial
            || assembly.topology_token != expected_topology_token
            || assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || new_topology_token <= expected_topology_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let component = assembly
            .parts
            .iter()
            .filter(|p| p.component_id == Some(component_id))
            .collect::<Vec<_>>();
        let target_index = assembly
            .parts
            .iter()
            .position(|p| p.component_id == Some(component_id) && p.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let target = assembly.parts[target_index].clone();
        if target.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        if !articulated_grid_range_valid(&new_grid, target.cell_m) {
            return Err(ArticulatedError::GeometryRange);
        }
        let next_generation = assembly
            .source_generation
            .checked_add(1)
            .ok_or(ArticulatedError::WorkBudget)?;
        let manifest = assembly
            .density_manifest
            .as_ref()
            .ok_or(ArticulatedError::InvalidDensity)?;
        let mut callback_mismatch = false;
        for part in &component {
            part.grid.for_each_solid(|_, _, _, material| {
                callback_mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
        }
        if callback_mismatch {
            return Err(ArticulatedError::InvalidDensity);
        }
        let stable_density = |material| manifest_density(manifest, material);
        // Admit the aggregate dense and solid workload before mass traversal,
        // shape generation, or cloning any replacement grids.
        let mut dense_preflight = 0_u64;
        let mut solid_preflight = 0_u64;
        for (index, part) in assembly.parts.iter().enumerate() {
            let grid = if index == target_index {
                &new_grid
            } else {
                &part.grid
            };
            if grid.solid_count() == 0 {
                continue;
            }
            let d = grid.dims();
            let dense = u64::from(d[0])
                .checked_mul(u64::from(d[1]))
                .and_then(|v| v.checked_mul(u64::from(d[2])))
                .ok_or(ArticulatedError::WorkBudget)?;
            dense_preflight = dense_preflight
                .checked_add(dense)
                .ok_or(ArticulatedError::WorkBudget)?;
            solid_preflight = solid_preflight
                .checked_add(grid.solid_count())
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if dense_preflight > MAX_ARTICULATED_DENSE_CELLS
            || solid_preflight > MAX_ARTICULATED_SOLID_CELLS
            || assembly.parts.len() > MAX_ARTICULATED_PARTS
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let old_origin = target.grid.origin();
        let new_origin = new_grid.origin();
        let new_dims = new_grid.dims();
        let mut removed_mass = 0.0_f64;
        let mut retained = 0_u64;
        let mut invalid_density = false;
        let mut unsupported = false;
        target.grid.for_each_solid(|x, y, z, material| {
            let Some(gx) = old_origin.x.checked_add(i64::from(x)) else {
                unsupported = true;
                return;
            };
            let Some(gy) = old_origin.y.checked_add(i64::from(y)) else {
                unsupported = true;
                return;
            };
            let Some(gz) = old_origin.z.checked_add(i64::from(z)) else {
                unsupported = true;
                return;
            };
            let (Some(lx), Some(ly), Some(lz)) = (
                gx.checked_sub(new_origin.x),
                gy.checked_sub(new_origin.y),
                gz.checked_sub(new_origin.z),
            ) else {
                unsupported = true;
                return;
            };
            let new_material = (lx >= 0
                && ly >= 0
                && lz >= 0
                && lx < i64::from(new_dims[0])
                && ly < i64::from(new_dims[1])
                && lz < i64::from(new_dims[2]))
            .then(|| new_grid.material(lx as u32, ly as u32, lz as u32))
            .flatten();
            match new_material {
                Some(current) if current == material => retained += 1,
                Some(_) => unsupported = true,
                None => {
                    let rho = stable_density(material);
                    if !rho.is_finite() || rho <= 0.0 {
                        invalid_density = true;
                    } else {
                        removed_mass += rho * f64::from(target.cell_m).powi(3);
                    }
                }
            }
        });
        if invalid_density {
            return Err(ArticulatedError::InvalidDensity);
        }
        if unsupported || new_grid.solid_count() != retained {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        let old_sources = component
            .iter()
            .map(|p| SourcePart {
                part_id: p.part_id,
                parent_id: None,
                volume_id: p.volume_id,
                source_revision: p.source_revision,
                grid: p.grid.clone(),
                cell_m: p.cell_m,
                local_transform: p.assembly_pose,
            })
            .collect::<Vec<_>>();
        let old_poses = old_sources
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let old_mass =
            crate::mass::articulated_mass_properties(&old_sources, &old_poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?
                .to_body_properties();
        validate_body_mass_properties(old_mass)?;
        let mut new_sources = Vec::new();
        for part in &component {
            let grid = if part.part_id == part_id {
                &new_grid
            } else {
                &part.grid
            };
            if grid.solid_count() == 0 {
                continue;
            }
            new_sources.push(SourcePart {
                part_id: part.part_id,
                parent_id: None,
                volume_id: part.volume_id,
                source_revision: if part.part_id == part_id {
                    new_source_revision
                } else {
                    part.source_revision
                },
                grid: grid.clone(),
                cell_m: part.cell_m,
                local_transform: part.assembly_pose,
            });
        }
        let new_mass = if new_sources.is_empty() {
            None
        } else {
            let poses = new_sources
                .iter()
                .map(|p| p.local_transform)
                .collect::<Vec<_>>();
            let mass =
                crate::mass::articulated_mass_properties(&new_sources, &poses, stable_density)
                    .ok_or(ArticulatedError::InvalidDensity)?
                    .to_body_properties();
            validate_body_mass_properties(mass)?;
            Some(mass)
        };
        let mut primitive_total = 0_usize;
        for (index, part) in assembly.parts.iter().enumerate() {
            if index == target_index {
                continue;
            }
            primitive_total = primitive_total
                .checked_add(part.primitives)
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        let representation = self.entries[entry_index(id.0, self.id_namespace)].representation;
        let built = (new_grid.solid_count() > 0)
            .then(|| build_collider(&new_grid, target.cell_m, representation));
        if let Some(built) = &built {
            primitive_total = primitive_total
                .checked_add(built.primitives)
                .ok_or(ArticulatedError::WorkBudget)?;
        }
        if primitive_total > 65_536
            || built
                .as_ref()
                .is_some_and(|b| b.primitives > crate::collider::MERGED_CUBOID_PRIMITIVE_BUDGET)
        {
            return Err(ArticulatedError::WorkBudget);
        }
        let body = target.body;
        let pose = *self.bodies[body].position();
        let old_com = pose.transform_point(Vector::new(
            old_mass.local_com_m[0],
            old_mass.local_com_m[1],
            old_mass.local_com_m[2],
        ));
        let old_linear = self.bodies[body].linvel();
        let angular = self.bodies[body].angvel();
        let new_linear = if let Some(mass) = new_mass {
            let new_com = pose.transform_point(Vector::new(
                mass.local_com_m[0],
                mass.local_com_m[1],
                mass.local_com_m[2],
            ));
            old_linear + angular.cross(new_com - old_com)
        } else {
            old_linear
        };
        if ![new_linear.x, new_linear.y, new_linear.z]
            .iter()
            .all(|x| x.is_finite())
        {
            return Err(ArticulatedError::InvalidMassProperties);
        }
        let (new_handle, part_surface_radius_m, root_surface_radius_m) =
            if let Some(ref built) = built {
                let origin = new_grid.origin();
                let origin_m = [
                    origin.x as f32 * target.cell_m,
                    origin.y as f32 * target.cell_m,
                    origin.z as f32 * target.cell_m,
                ];
                let offset = rotate(target.assembly_pose.rotation_xyzw, origin_m);
                let local = Pose::new(
                    Vector::new(
                        target.assembly_pose.translation_m[0] + offset[0],
                        target.assembly_pose.translation_m[1] + offset[1],
                        target.assembly_pose.translation_m[2] + offset[2],
                    ),
                    Rotation::from_xyzw(
                        target.assembly_pose.rotation_xyzw[0],
                        target.assembly_pose.rotation_xyzw[1],
                        target.assembly_pose.rotation_xyzw[2],
                        target.assembly_pose.rotation_xyzw[3],
                    )
                    .to_scaled_axis(),
                );
                let old_collider = self
                    .colliders
                    .get(target.collider)
                    .ok_or(ArticulatedError::Hierarchy)?;
                let mut collider = ColliderBuilder::new(built.collider.shared_shape().clone())
                    .density(0.0)
                    .position(local)
                    .friction(old_collider.friction())
                    .active_collision_types(old_collider.active_collision_types())
                    .sensor(old_collider.is_sensor())
                    .build();
                collider.set_restitution(old_collider.restitution());
                collider.set_restitution_combine_rule(old_collider.restitution_combine_rule());
                collider.set_friction_combine_rule(old_collider.friction_combine_rule());
                collider.set_collision_groups(old_collider.collision_groups());
                collider.set_solver_groups(old_collider.solver_groups());
                let handle = self
                    .colliders
                    .insert_with_parent(collider, body, &mut self.bodies);
                self.tag_collider(handle, id.0);
                (
                    Some(handle),
                    source_surface_radius_m(&new_grid, target.cell_m, [0.0; 3], None),
                    source_surface_radius_m(
                        &new_grid,
                        target.cell_m,
                        [0.0; 3],
                        Some(target.assembly_pose),
                    ),
                )
            } else {
                (None, 0.0, 0.0)
            };
        let entry_idx = entry_index(id.0, self.id_namespace);
        let entry_primary_removed = self.entries[entry_idx].collider == target.collider;
        self.colliders
            .remove(target.collider, &mut self.islands, &mut self.bodies, true);
        self.pending_removed.push(target.collider);
        let assembly = self.articulated.get_mut(&id.0).expect("validated corpse");
        if let Some(handle) = new_handle {
            self.pending_modified.push(handle);
            let part = &mut assembly.parts[target_index];
            part.grid = new_grid;
            part.source_revision = new_source_revision;
            part.collider = handle;
            part.primitives = built.as_ref().map_or(0, |b| b.primitives);
            part.part_surface_radius_m = part_surface_radius_m;
            part.root_surface_radius_m = root_surface_radius_m;
        } else {
            assembly.parts.remove(target_index);
        }
        for part in &mut assembly.parts {
            if part.volume_id == target.volume_id {
                part.source_revision = new_source_revision;
            }
        }
        assembly.topology_token = new_topology_token;
        assembly.source_generation = next_generation;
        let component_remains = assembly
            .parts
            .iter()
            .any(|p| p.component_id == Some(component_id));
        let owner_remains = !assembly.parts.is_empty();
        if !owner_remains {
            self.retire_articulated_body(id);
            return Ok(CorpseEditOutcome::AggregateRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        if !component_remains {
            self.bodies.remove(
                body,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
            let replacement = assembly.parts.first().expect("owner remains");
            let entry = &mut self.entries[entry_idx];
            if entry.body == body {
                entry.body = replacement.body;
                entry.collider = replacement.collider;
                entry.mass_properties = None;
            }
            self.ccd_solver = CCDSolver::new();
            return Ok(CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        let mass = new_mass.expect("remaining component has source cells");
        self.bodies[body]
            .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
        self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[body].set_position(pose, true);
        self.bodies[body].set_linvel(new_linear, true);
        self.bodies[body].set_angvel(angular, true);
        let entry = &mut self.entries[entry_idx];
        if entry.body == body {
            if entry_primary_removed {
                entry.collider = new_handle.unwrap_or_else(|| {
                    assembly
                        .parts
                        .iter()
                        .find(|p| p.body == body)
                        .expect("surviving component shard")
                        .collider
                });
            }
            entry.mass_properties = Some(mass);
        }
        Ok(if new_handle.is_none() {
            CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            }
        } else {
            CorpseEditOutcome::Rebuilt {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            }
        })
    }

    /// Edits dynamic-corpse source geometry with an open-system removal rule:
    /// removed cells leave carrying their instantaneous rigid-body velocity
    /// field, while the surviving body's world pose and angular velocity stay
    /// fixed. Its COM linear velocity is adjusted to preserve the velocity
    /// field at every surviving point. Additions and material replacement are
    /// rejected until their incoming momentum policy is defined.
    pub fn rebuild_dynamic_corpse_part_grid(
        &mut self,
        id: ArticulatedBodyId,
        expected_topology_token: u64,
        new_topology_token: u64,
        part_id: u32,
        expected_source_revision: u64,
        new_source_revision: u64,
        new_grid: OccupancyGrid,
        density: impl Fn(spall_core::MaterialId) -> f64,
    ) -> Result<CorpseEditOutcome, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::Hierarchy)?;
        if !assembly.dynamic_corpse {
            return Err(ArticulatedError::DynamicCorpsePose);
        }
        if assembly
            .parts
            .iter()
            .any(|part| part.component_id.is_some())
        {
            // The legacy operation addresses a whole source part, which may
            // span multiple independently moving fragments. It must not pick
            // one shard by vector order and silently leave siblings untouched.
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        if assembly.topology_token != expected_topology_token
            || assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || new_topology_token <= expected_topology_token
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let index_part = assembly
            .parts
            .iter()
            .position(|p| p.part_id == part_id)
            .ok_or(ArticulatedError::StaleTopology)?;
        let old = &assembly.parts[index_part];
        if old.source_revision != expected_source_revision
            || new_source_revision <= expected_source_revision
        {
            return Err(ArticulatedError::StaleTopology);
        }
        let original_parts = assembly.parts.clone();
        let manifest = assembly
            .density_manifest
            .clone()
            .ok_or(ArticulatedError::InvalidDensity)?;
        let mut density_mismatch = false;
        for part in &original_parts {
            part.grid.for_each_solid(|_, _, _, material| {
                density_mismatch |= manifest
                    .get(&material)
                    .is_none_or(|bits| density(material).to_bits() != *bits);
            });
        }
        if density_mismatch {
            return Err(ArticulatedError::InvalidDensity);
        }
        let stable_density = |material| manifest_density(&manifest, material);
        let mut removed_mass = 0.0;
        let mut invalid_density = false;
        let mut unsupported_edit = false;
        let mut retained_cells = 0_u64;
        let old_origin = old.grid.origin();
        let new_origin = new_grid.origin();
        let new_dims = new_grid.dims();
        old.grid.for_each_solid(|x, y, z, material| {
            let global = [
                old_origin.x + i64::from(x),
                old_origin.y + i64::from(y),
                old_origin.z + i64::from(z),
            ];
            let local = [
                global[0] - new_origin.x,
                global[1] - new_origin.y,
                global[2] - new_origin.z,
            ];
            let new_material = (0..3)
                .all(|axis| local[axis] >= 0 && local[axis] < i64::from(new_dims[axis]))
                .then(|| new_grid.material(local[0] as u32, local[1] as u32, local[2] as u32))
                .flatten();
            match new_material {
                Some(current) if current == material => retained_cells += 1,
                Some(_) => unsupported_edit = true,
                None => {
                    let rho = stable_density(material);
                    if !rho.is_finite() || rho <= 0.0 {
                        invalid_density = true;
                    } else {
                        removed_mass += rho * f64::from(old.cell_m).powi(3);
                    }
                }
            }
        });
        if invalid_density {
            return Err(ArticulatedError::InvalidDensity);
        }
        if unsupported_edit {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        if new_grid.solid_count() != retained_cells {
            return Err(ArticulatedError::UnsupportedCorpseEdit);
        }
        let pose = *self.bodies[self.entries[entry_index(id.0, self.id_namespace)].body].position();
        if new_grid.solid_count() == 0 {
            if original_parts.len() == 1 {
                self.retire_articulated_body(id);
                return Ok(CorpseEditOutcome::AggregateRetired {
                    elapsed: Duration::ZERO,
                    removed_mass_kg: removed_mass,
                });
            }
            let remaining = original_parts
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index_part)
                .map(|(_, p)| SourcePart {
                    part_id: p.part_id,
                    // Frozen corpse hierarchy is pose metadata only. Flatten
                    // every surviving part to its already-resolved assembly
                    // pose so removing a parent cannot orphan descendants.
                    parent_id: None,
                    volume_id: p.volume_id,
                    source_revision: p.source_revision,
                    grid: p.grid.clone(),
                    cell_m: p.cell_m,
                    local_transform: p.assembly_pose,
                })
                .collect::<Vec<_>>();
            let poses = remaining
                .iter()
                .map(|p| p.local_transform)
                .collect::<Vec<_>>();
            let mass = crate::mass::articulated_mass_properties(&remaining, &poses, stable_density)
                .ok_or(ArticulatedError::InvalidDensity)?
                .to_body_properties();
            validate_body_mass_properties(mass)?;
            let body = self.entries[entry_index(id.0, self.id_namespace)].body;
            let (linear, angular) = corpse_edit_velocity(&self.bodies[body], pose, mass)?;
            let started = Instant::now();
            let old_handle = original_parts[index_part].collider;
            self.colliders
                .remove(old_handle, &mut self.islands, &mut self.bodies, true);
            self.pending_removed.push(old_handle);
            self.bodies[body]
                .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
            self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
            self.bodies[body].set_position(pose, true);
            self.bodies[body].set_linvel(linear, true);
            self.bodies[body].set_angvel(angular, true);
            let a = self.articulated.get_mut(&id.0).unwrap();
            a.parts.remove(index_part);
            for part in &mut a.parts {
                part.parent_id = None;
                part.local_pose = part.assembly_pose;
            }
            a.topology_token = new_topology_token;
            self.entries[entry_index(id.0, self.id_namespace)].collider = a.parts[0].collider;
            self.entries[entry_index(id.0, self.id_namespace)].mass_properties = Some(mass);
            return Ok(CorpseEditOutcome::PartRetired {
                elapsed: started.elapsed(),
                removed_mass_kg: removed_mass,
            });
        }
        let sources = original_parts
            .iter()
            .enumerate()
            .map(|(i, p)| SourcePart {
                part_id: p.part_id,
                parent_id: p.parent_id,
                volume_id: p.volume_id,
                source_revision: if i == index_part {
                    new_source_revision
                } else {
                    p.source_revision
                },
                grid: if i == index_part {
                    new_grid.clone()
                } else {
                    p.grid.clone()
                },
                cell_m: p.cell_m,
                local_transform: p.local_pose,
            })
            .collect::<Vec<_>>();
        let poses = sources
            .iter()
            .map(|p| p.local_transform)
            .collect::<Vec<_>>();
        let mass = crate::mass::articulated_mass_properties(&sources, &poses, stable_density)
            .ok_or(ArticulatedError::InvalidDensity)?
            .to_body_properties();
        validate_body_mass_properties(mass)?;
        let index = entry_index(id.0, self.id_namespace);
        let body = self.entries[index].body;
        let (linear, angular) = corpse_edit_velocity(&self.bodies[body], pose, mass)?;
        let elapsed = self.rebuild_articulated_part_grid_impl(
            id,
            expected_topology_token,
            new_topology_token,
            part_id,
            expected_source_revision,
            new_source_revision,
            new_grid,
            true,
        )?;
        self.bodies[body]
            .set_additional_mass_properties(rapier_mass_properties(mass, [0.0; 3]), true);
        self.bodies[body].recompute_mass_properties_from_colliders(&self.colliders);
        self.bodies[body].set_position(pose, true);
        self.bodies[body].set_linvel(linear, true);
        self.bodies[body].set_angvel(angular, true);
        self.entries[index].mass_properties = Some(mass);
        Ok(CorpseEditOutcome::Rebuilt {
            elapsed,
            removed_mass_kg: removed_mass,
        })
    }
}
