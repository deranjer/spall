use super::*;

impl PhysicsWorld {
    /// Validates the exact original source cells and applied hierarchy used by
    /// an Alive capture. Air padding may differ; absolute occupied cells may not.
    pub fn validate_alive_capture_sources(
        &self,
        id: ArticulatedBodyId,
        sources: &[SourcePart],
    ) -> Result<(), crate::articulated::ArticulatedError> {
        use crate::articulated::{ArticulatedError, MAX_ARTICULATED_DENSE_CELLS};
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(ArticulatedError::InvalidRestore)?;
        if assembly.dynamic_corpse
            || sources.len() > crate::articulated::MAX_ARTICULATED_PARTS
            || assembly.pending_pose.is_some()
            || assembly.pending_root_pose_token.is_some()
            || sources.windows(2).any(|p| p[0].part_id >= p[1].part_id)
            || sources.len() != assembly.source_volume_ids.len()
            || sources
                .iter()
                .map(|p| p.volume_id)
                .collect::<std::collections::BTreeSet<_>>()
                != assembly.source_volume_ids
            || sources
                .iter()
                .map(|p| p.grid.dims().into_iter().map(u64::from).product::<u64>())
                .sum::<u64>()
                > MAX_ARTICULATED_DENSE_CELLS
            || sources.iter().filter(|p| p.grid.solid_count() != 0).count() != assembly.parts.len()
        {
            return Err(ArticulatedError::InvalidRestore);
        }
        for actual in &assembly.parts {
            let source = sources
                .iter()
                .find(|source| source.part_id == actual.part_id)
                .ok_or(ArticulatedError::InvalidRestore)?;
            if source.volume_id != actual.volume_id
                || source.source_revision != actual.source_revision
                || source.parent_id != actual.parent_id
                || source.local_transform != actual.local_pose
                || source.cell_m.to_bits() != actual.cell_m.to_bits()
                || source.grid.solid_count() != actual.grid.solid_count()
            {
                return Err(ArticulatedError::InvalidRestore);
            }
            let a = actual.grid.origin();
            let s = source.grid.origin();
            let mut equal = true;
            actual.grid.for_each_solid(|x, y, z, material| {
                let index = [
                    i128::from(a.x) + i128::from(x) - i128::from(s.x),
                    i128::from(a.y) + i128::from(y) - i128::from(s.y),
                    i128::from(a.z) + i128::from(z) - i128::from(s.z),
                ];
                let index = index.map(u32::try_from);
                equal &= match index {
                    [Ok(x), Ok(y), Ok(z)] => source.grid.material(x, y, z) == Some(material),
                    _ => false,
                };
            });
            if !equal {
                return Err(ArticulatedError::InvalidRestore);
            }
        }
        Ok(())
    }

    /// Returns current owner tokens and root motion without exposing handles.
    pub fn articulated_owner_snapshot(
        &self,
        id: ArticulatedBodyId,
    ) -> Option<ArticulatedOwnerSnapshot> {
        let assembly = self.articulated.get(&id.0)?;
        Some(ArticulatedOwnerSnapshot {
            owner_key: assembly.fracture_owner_key,
            source_generation: assembly.source_generation,
            next_component_serial: assembly.next_component_serial,
            topology_token: assembly.topology_token,
            pose_token: assembly.pose_token,
            pending_pose: assembly.pending_root_pose_token.is_some()
                || assembly.pending_pose.is_some(),
            root: self.body_state(id.0),
        })
    }

    /// Returns the applied authored local pose for each source part in stable
    /// part-ID order. Pending samples are not reported as applied state.
    pub fn articulated_pose_snapshot(
        &self,
        id: ArticulatedBodyId,
    ) -> Option<crate::articulated::ArticulatedPoseSnapshot> {
        let assembly = self.articulated.get(&id.0)?;
        let mut parts = assembly
            .parts
            .iter()
            .map(|part| (part.part_id, part.local_pose))
            .collect::<Vec<_>>();
        parts.sort_by_key(|(part_id, _)| *part_id);
        Some(crate::articulated::ArticulatedPoseSnapshot {
            topology_token: assembly.topology_token,
            pose_token: assembly.pose_token,
            parts,
        })
    }

    /// Returns each live kinematic source part's installed twist in stable
    /// part-ID order. Pending pose frames and corpse owners have no snapshot.
    /// Capture this together with the source, owner, and pose snapshots on the
    /// owning simulation thread, with no intervening world mutation; these
    /// independent reads do not form a cross-thread snapshot transaction.
    pub fn articulated_part_motion_snapshot(
        &self,
        id: ArticulatedBodyId,
    ) -> Option<Vec<crate::articulated::AliveArticulatedPartMotion>> {
        let assembly = self.articulated.get(&id.0)?;
        if !assembly.kinematic_part_bodies
            || assembly.dynamic_corpse
            || assembly.pending_root_pose_token.is_some()
            || assembly.pending_pose.is_some()
        {
            return None;
        }
        let mut parts = assembly
            .parts
            .iter()
            .map(|part| {
                let body = self.bodies.get(part.body)?;
                let linear = body.linvel();
                let angular = body.angvel();
                Some(crate::articulated::AliveArticulatedPartMotion {
                    part_id: part.part_id,
                    linear_velocity_m_s: [linear.x, linear.y, linear.z],
                    angular_velocity_rad_s: [angular.x, angular.y, angular.z],
                })
            })
            .collect::<Option<Vec<_>>>()?;
        if parts.iter().any(|part| {
            !part
                .linear_velocity_m_s
                .iter()
                .chain(&part.angular_velocity_rad_s)
                .all(|value| value.is_finite())
        }) {
            return None;
        }
        parts.sort_by_key(|part| part.part_id);
        Some(parts)
    }

    /// Returns live state for a currently owned stable fracture component.
    /// Retired, foreign-owner, and never-allocated keys fail closed.
    pub fn fracture_component_state(
        &self,
        id: ArticulatedBodyId,
        component_id: DerivedFractureComponentId,
    ) -> Result<BodyState, FractureError> {
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse {
            return Err(FractureError::StaleOwner);
        }
        if assembly.fracture_owner_key != Some(component_id.owner_key)
            || component_id.serial == 0
            || component_id.serial >= assembly.next_component_serial
        {
            return Err(FractureError::StaleOwner);
        }
        let mut bodies = assembly
            .parts
            .iter()
            .filter(|part| part.component_id == Some(component_id))
            .map(|part| part.body);
        let body = bodies.next().ok_or(FractureError::StaleOwner)?;
        if bodies.any(|other| other != body) {
            return Err(FractureError::InvalidPlan);
        }
        let rb = self.bodies.get(body).ok_or(FractureError::StaleOwner)?;
        let t = rb.translation();
        let q = rb.rotation();
        let lv = rb.linvel();
        let av = rb.angvel();
        Ok(BodyState {
            translation_m: [t.x, t.y, t.z],
            rotation: [q.x, q.y, q.z, q.w],
            linvel_m_s: [lv.x, lv.y, lv.z],
            angvel_rad_s: [av.x, av.y, av.z],
            sleeping: rb.is_sleeping(),
            mass_kg: rb.mass(),
        })
    }

    /// Sets a feasibility-test velocity on the unsplit corpse (`None`) or one
    /// explicitly keyed live component. This never exposes Rapier handles.
    pub fn set_dynamic_corpse_component_velocity(
        &mut self,
        id: ArticulatedBodyId,
        component_key: Option<DerivedFractureComponentId>,
        linear_m_s: [f32; 3],
        angular_rad_s: [f32; 3],
    ) -> Result<(), FractureError> {
        if !linear_m_s
            .iter()
            .chain(&angular_rad_s)
            .all(|v| v.is_finite())
        {
            return Err(FractureError::InvalidPlan);
        }
        let assembly = self
            .articulated
            .get(&id.0)
            .ok_or(FractureError::StaleOwner)?;
        if !assembly.dynamic_corpse {
            return Err(FractureError::StaleOwner);
        }
        let body = if let Some(key) = component_key {
            if assembly.fracture_owner_key != Some(key.owner_key)
                || key.serial == 0
                || key.serial >= assembly.next_component_serial
            {
                return Err(FractureError::StaleOwner);
            }
            assembly
                .parts
                .iter()
                .find(|p| p.component_id == Some(key))
                .map(|part| part.body)
                .ok_or(FractureError::StaleOwner)?
        } else {
            if assembly.parts.iter().any(|p| p.component_id.is_some()) {
                return Err(FractureError::StaleOwner);
            }
            self.entries[entry_index(id.0, self.id_namespace)].body
        };
        let rb = self.bodies.get_mut(body).ok_or(FractureError::StaleOwner)?;
        rb.set_linvel(
            Vector::new(linear_m_s[0], linear_m_s[1], linear_m_s[2]),
            true,
        );
        rb.set_angvel(
            Vector::new(angular_rad_s[0], angular_rad_s[1], angular_rad_s[2]),
            true,
        );
        Ok(())
    }

    /// Casts against the live Rapier shape of every source part and maps the
    /// selected TOI back to the exact occupied source cell. TOIs within 10 µm
    /// tie by the smallest stable part ID.
    pub fn raycast_articulated(
        &mut self,
        id: ArticulatedBodyId,
        origin_m: [f32; 3],
        direction: [f32; 3],
        max_toi_m: f32,
    ) -> Result<SourceCellHit, crate::articulated::ArticulatedError> {
        use crate::articulated::ArticulatedError;
        let id = id.0;
        let norm = direction.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !origin_m.iter().all(|v| v.is_finite())
            || !norm.is_finite()
            || norm <= 1.0e-6
            || !max_toi_m.is_finite()
            || max_toi_m <= 0.0
        {
            return Err(ArticulatedError::Miss);
        }
        if !self.articulated.contains_key(&id) {
            return Err(ArticulatedError::Miss);
        }
        self.sync_queries();
        let direction = Vector::new(
            direction[0] / norm,
            direction[1] / norm,
            direction[2] / norm,
        );
        let ray = rapier3d::parry::query::Ray::new(
            Vector::new(origin_m[0], origin_m[1], origin_m[2]),
            direction,
        );
        let hit_tois: HashMap<_, _> = {
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                QueryFilter::default(),
            );
            queries
                .intersect_ray(ray, max_toi_m, true)
                .filter(|(handle, _, _)| {
                    self.articulated[&id]
                        .parts
                        .iter()
                        .any(|part| part.collider == *handle)
                })
                .map(|(handle, _, intersection)| (handle, intersection.time_of_impact))
                .collect()
        };
        let assembly = self.articulated.get(&id).ok_or(ArticulatedError::Miss)?;
        let root_pose =
            *self.bodies[self.entries[entry_index(id, self.id_namespace)].body].position();
        let mut best: Option<(f32, u32, [i32; 3], u64, SourceCellHit)> = None;
        for part in &assembly.parts {
            let Some(toi) = hit_tois.get(&part.collider).copied() else {
                continue;
            };
            let collider = self
                .colliders
                .get(part.collider)
                .ok_or(ArticulatedError::Miss)?;
            let collider_pose = if assembly.kinematic_part_bodies || part.component_id.is_some() {
                *collider.position()
            } else {
                let local_pose = *collider
                    .position_wrt_parent()
                    .ok_or(ArticulatedError::Miss)?;
                root_pose * local_pose
            };
            let world_hit = ray.point_at(toi);
            let local = collider_pose.inverse_transform_point(world_hit);
            let cell_coord = [
                local.x / part.cell_m,
                local.y / part.cell_m,
                local.z / part.cell_m,
            ];
            let dims = part.grid.dims();
            let mut candidates: [Vec<i64>; 3] = std::array::from_fn(|axis| {
                let value = cell_coord[axis];
                let nearest = value.round();
                let tolerance =
                    (1.0e-5 / part.cell_m).max(4.0 * f32::EPSILON * value.abs().max(1.0));
                if (value - nearest).abs() <= tolerance {
                    vec![nearest as i64 - 1, nearest as i64]
                } else {
                    vec![value.floor() as i64]
                }
            });
            for (axis, values) in candidates.iter_mut().enumerate() {
                values.retain(|value| *value >= 0 && *value < i64::from(dims[axis]));
            }
            let grid_origin = part.grid.origin();
            let mut cells = Vec::new();
            for &x in &candidates[0] {
                for &y in &candidates[1] {
                    for &z in &candidates[2] {
                        if part.grid.is_solid(x as u32, y as u32, z as u32) {
                            cells.push([x, y, z]);
                        }
                    }
                }
            }
            cells.sort_unstable();
            let Some(idx) = cells.first().copied() else {
                continue;
            };
            let source = [
                grid_origin.x + idx[0],
                grid_origin.y + idx[1],
                grid_origin.z + idx[2],
            ];
            if source
                .iter()
                .any(|v| *v < i32::MIN as i64 || *v > i32::MAX as i64)
            {
                continue;
            }
            let hit = SourceCellHit {
                part_id: part.part_id,
                volume_id: part.volume_id,
                source_revision: part.source_revision,
                cell: [source[0] as i32, source[1] as i32, source[2] as i32],
                toi_m: toi,
                topology_token: assembly.topology_token,
                pose_token: assembly.pose_token,
            };
            let component_serial = part.component_id.map_or(0, |key| key.serial);
            if best.is_none_or(|(old_toi, old_part, old_cell, old_component, _)| {
                toi < old_toi - 1.0e-5
                    || ((toi - old_toi).abs() <= 1.0e-5
                        && (part.part_id, hit.cell, component_serial)
                            < (old_part, old_cell, old_component))
            }) {
                best = Some((toi, part.part_id, hit.cell, component_serial, hit));
            }
        }
        best.map(|(_, _, _, _, hit)| hit)
            .ok_or(ArticulatedError::Miss)
    }
}
