//! Read-only owner-thread capture. No journal, counters or physics are mutated.
use super::*;
use spall_core::{LocalCell, MaterialId};
use spall_physics::{DynamicCorpseOwnerFence, OccupancyGrid, SourcePart};
use spall_protocol::{creature_owner::*, creature_snapshot::*, *};
use spall_voxel::Volume;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum CreatureCaptureError {
    #[error("animal host, source ownership or physical state is inconsistent")]
    Inconsistent,
    #[error("animal capture exceeds the bounded population work or memory budget")]
    Limit,
    #[error("animal has an unapplied physical pose")]
    Pending,
    #[error(transparent)]
    Physics(#[from] spall_physics::ArticulatedError),
    #[error(transparent)]
    Portable(#[from] CreatureOwnershipError),
}

#[derive(Default)]
struct Budget {
    bricks: usize,
    dense: usize,
    solids: usize,
    masks: usize,
    grids: u64,
    bytes: usize,
}

fn add(value: &mut usize, extra: usize, limit: usize) -> Result<(), CreatureCaptureError> {
    *value = value
        .checked_add(extra)
        .ok_or(CreatureCaptureError::Limit)?;
    if *value > limit {
        return Err(CreatureCaptureError::Limit);
    }
    Ok(())
}

impl Simulation {
    /// Captures exact currently applied animal state on the owning thread.
    /// None means an uninitialized host; Some(empty) retains its allocator.
    /// This does not replace the guarded geometry-only checkpoint path.
    /// Population memory/mask admission is deliberately conservative.
    pub fn capture_creature_snapshot(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Option<CreatureSnapshotV2>, CreatureCaptureError> {
        let counts = self.world.creature_source_counts();
        let Some(host) = &self.creatures else {
            return if counts == [0; 5] {
                Ok(None)
            } else {
                Err(CreatureCaptureError::Inconsistent)
            };
        };
        let count = host.record_count();
        if count > MAX_CREATURE_OWNER_SET_RECORDS {
            return Err(CreatureCaptureError::Limit);
        }
        if counts[0] != count
            || counts[1] != count
            || counts[3] != count
            || content_manifest_hash(catalog.materials)
                != content_manifest_hash(self.world.materials())
        {
            return Err(CreatureCaptureError::Inconsistent);
        }
        let mut population = Budget::default();
        let mut binding_count = 0;
        let mut corpse_count = 0;
        let mut physics_ids = HashSet::new();
        // Pass 1: admit the entire population's scalar metadata and nonuniform
        // payload work before counting/scanning a dense or paletted brick.
        for state in host.records() {
            if state.source_bindings.len() > MAX_CREATURE_OWNER_PARTS {
                return Err(CreatureCaptureError::Limit);
            }
            state
                .validate(catalog)
                .map_err(CreatureOwnershipError::from)?;
            let n = state.source_bindings.len();
            binding_count += n;
            if self.world.creature_source_part_counts(state.key) != Some((n, n)) {
                return Err(CreatureCaptureError::Inconsistent);
            }
            let id = self
                .world
                .creature_physics_owner(state.key)
                .ok_or(CreatureCaptureError::Inconsistent)?;
            if !physics_ids.insert(id) {
                return Err(CreatureCaptureError::Inconsistent);
            }
            let owner = self
                .world
                .physics()
                .articulated_owner_snapshot(id)
                .ok_or(CreatureCaptureError::Inconsistent)?;
            if owner.pending_pose {
                return Err(CreatureCaptureError::Pending);
            }
            if owner.topology_token != state.aggregate_topology_revision {
                return Err(CreatureCaptureError::Inconsistent);
            }
            let frozen = self.world.creature_frozen_poses(state.key);
            if state.lifecycle == CreatureLifecycleState::Corpse {
                corpse_count += 1;
                if frozen.is_none_or(|poses| {
                    poses
                        .iter()
                        .map(|(id, _)| *id)
                        .ne(state.source_bindings.iter().map(|b| b.part_id))
                }) {
                    return Err(CreatureCaptureError::Inconsistent);
                }
            } else if frozen.is_some() {
                return Err(CreatureCaptureError::Inconsistent);
            }
            let mut record = Budget::default();
            add(
                &mut population.bytes,
                16 * 1024 + n * 512,
                MAX_CREATURE_OWNER_SET_BYTES,
            )?;
            for binding in &state.source_bindings {
                let volume = self
                    .world
                    .creature_source_part(state.key, binding.part_id)
                    .ok_or(CreatureCaptureError::Inconsistent)?;
                if volume.id() != binding.source_volume_id
                    || self.world.creature_source_owner(volume.id())
                        != Some((state.key, binding.part_id))
                    || self.world.volume_ref(volume.id()).is_some()
                    || volume.id().get() >= self.world.registry().counters().1
                {
                    return Err(CreatureCaptureError::Inconsistent);
                }
                let bricks = volume.resident_brick_count();
                add(&mut record.bricks, bricks, MAX_CREATURE_OWNER_BRICKS)?;
                add(&mut population.bricks, bricks, MAX_CREATURE_OWNER_BRICKS)?;
                let mut slots = 0;
                volume.for_each_resident_brick(|_, brick| {
                    if brick.uniform_storage_material().is_none() {
                        slots += spall_core::CELLS_PER_BRICK;
                    }
                });
                add(&mut record.dense, slots, MAX_CREATURE_OWNER_DENSE_CELLS)?;
                add(
                    &mut population.dense,
                    slots,
                    MAX_CREATURE_OWNER_SET_DENSE_CELLS,
                )?;
                add(
                    &mut population.bytes,
                    slots * 2 + bricks * 128,
                    MAX_CREATURE_OWNER_SET_BYTES,
                )?;
            }
        }
        if counts[2] != binding_count || counts[4] != corpse_count {
            return Err(CreatureCaptureError::Inconsistent);
        }
        // Pass 2: bounded source scan, exact provenance and reconstruction-grid
        // admission. Only after this pass may source arrays or masks be copied.
        for state in host.records() {
            let id = self.world.creature_physics_owner(state.key).unwrap();
            let components = if state.lifecycle == CreatureLifecycleState::Alive {
                1
            } else {
                self.world.physics().articulated_rigid_body_count(id)
            };
            if components == 0 || components > MAX_CREATURE_OWNER_COMPONENTS {
                return Err(CreatureCaptureError::Limit);
            }
            let component_bytes = components * 2 * std::mem::size_of::<CreatureOwnedComponentV1>();
            add(
                &mut population.bytes,
                component_bytes,
                MAX_CREATURE_OWNER_SET_BYTES,
            )?;
            let mut record = Budget {
                bytes: 16 * 1024 + state.source_bindings.len() * 512 + component_bytes,
                ..Default::default()
            };
            for binding in &state.source_bindings {
                let volume = self
                    .world
                    .creature_source_part(state.key, binding.part_id)
                    .unwrap();
                let metadata = self
                    .world
                    .creature_source_metadata(state.key, binding.part_id)
                    .ok_or(CreatureCaptureError::Inconsistent)?;
                if metadata.brick_revisions.len() != volume.resident_brick_count()
                    || creature_source_freshness(
                        volume,
                        metadata.parent_id,
                        metadata.initial_local_transform,
                    )
                    .as_ref()
                        != Some(metadata)
                {
                    return Err(CreatureCaptureError::Inconsistent);
                }
                let mut source_bytes = volume.resident_brick_count() * 128;
                let mut solids = 0;
                volume.for_each_resident_brick(|_, brick| {
                    if brick.uniform_storage_material().is_none() {
                        source_bytes += spall_core::CELLS_PER_BRICK * 2;
                    }
                    solids += brick.solid_cells() as usize;
                });
                add(&mut record.solids, solids, MAX_CREATURE_OWNER_SOLID_CELLS)?;
                add(
                    &mut population.solids,
                    solids,
                    MAX_CREATURE_OWNER_SET_SOLID_CELLS,
                )?;
                let work = if solids == 0 {
                    CreatureSourceWork {
                        grid_cells: 1,
                        ..Default::default()
                    }
                } else {
                    creature_source_work(volume).map_err(|_| CreatureCaptureError::Limit)?
                };
                record.grids += work.grid_cells;
                population.grids += work.grid_cells;
                if record.grids > spall_physics::articulated::MAX_ARTICULATED_DENSE_CELLS
                    || population.grids > MAX_CREATURE_OWNER_SET_DENSE_CELLS as u64
                {
                    return Err(CreatureCaptureError::Limit);
                }
                // Conservative upper bound: each component can touch each
                // occupied brick, with at least one distinct solid per mask.
                let masks = (work.mask_bricks * components).min(solids);
                add(&mut record.masks, masks, MAX_CREATURE_OWNER_MASK_BRICKS)?;
                add(&mut population.masks, masks, MAX_CREATURE_OWNER_MASK_BRICKS)?;
                // Geometrically grown cell vectors can reserve nearly twice
                // their length; include that capacity before mask construction.
                let mask_bytes = solids * 2 * std::mem::size_of::<CreatureMaskCellV1>()
                    + masks * (256 + 2 * std::mem::size_of::<CreatureSourceMaskBrickV1>());
                add(
                    &mut record.bytes,
                    source_bytes + mask_bytes,
                    MAX_CREATURE_OWNER_RECORD_BYTES,
                )?;
                add(
                    &mut population.bytes,
                    mask_bytes,
                    MAX_CREATURE_OWNER_SET_BYTES,
                )?;
            }
        }
        let records = host
            .records()
            .map(|state| self.capture_owner(state))
            .collect::<Result<Vec<_>, _>>()?;
        let snapshot = CreatureSnapshotV2 {
            schema: CREATURE_SNAPSHOT_SCHEMA,
            allocator: host.allocator_snapshot(),
            next_volume_id: self.world.registry().counters().1,
            owners: CreatureOwnershipSetV4 {
                schema: CREATURE_OWNER_SCHEMA,
                records,
            },
        };
        snapshot.validate(catalog)?;
        Ok(Some(snapshot))
    }

    fn capture_owner(
        &self,
        state: &CreatureStateV1,
    ) -> Result<CreatureOwnershipRecordV4, CreatureCaptureError> {
        let id = self.world.creature_physics_owner(state.key).unwrap();
        let owner = self
            .world
            .physics()
            .articulated_owner_snapshot(id)
            .ok_or(CreatureCaptureError::Inconsistent)?;
        let owner_key = owner
            .owner_key
            .unwrap_or_else(|| stable_creature_owner_key(state.key));
        let poses = if state.lifecycle == CreatureLifecycleState::Corpse {
            self.world
                .creature_frozen_poses(state.key)
                .unwrap()
                .to_vec()
        } else {
            let applied = self
                .world
                .physics()
                .articulated_pose_snapshot(id)
                .ok_or(CreatureCaptureError::Inconsistent)?;
            if applied.pose_token != owner.pose_token
                || applied.topology_token != owner.topology_token
            {
                return Err(CreatureCaptureError::Inconsistent);
            }
            applied.parts
        };
        if poses
            .iter()
            .map(|(id, _)| *id)
            .ne(state.source_bindings.iter().map(|b| b.part_id))
        {
            return Err(CreatureCaptureError::Inconsistent);
        }
        let mut parts = Vec::with_capacity(poses.len());
        for (binding, (_, transform)) in state.source_bindings.iter().zip(&poses) {
            let volume = self
                .world
                .creature_source_part(state.key, binding.part_id)
                .unwrap();
            let metadata = self
                .world
                .creature_source_metadata(state.key, binding.part_id)
                .unwrap();
            parts.push(capture_source(
                volume,
                binding,
                metadata,
                *transform,
                owner.source_generation,
            ));
        }
        let mut sampled = state.clone();
        sampled.root_translation_m = self
            .world
            .physics_origin()
            .to_world_f64(owner.root.translation_m);
        sampled.root_rotation_xyzw = owner.root.rotation;
        sampled.linear_velocity_m_s = owner.root.linvel_m_s;
        sampled.angular_velocity_rad_s = owner.root.angvel_rad_s;
        let mut components;
        let primary_component_serial;
        let part_velocities;
        let sources = self.capture_physical_sources(state.key, &parts)?;
        if state.lifecycle == CreatureLifecycleState::Alive {
            self.world
                .physics()
                .validate_alive_capture_sources(id, &sources)?;
            part_velocities = self
                .world
                .physics()
                .articulated_part_motion_snapshot(id)
                .ok_or(CreatureCaptureError::Inconsistent)?
                .into_iter()
                .map(|motion| CreaturePartVelocityV2 {
                    part_id: motion.part_id,
                    linear_velocity_m_s: motion.linear_velocity_m_s,
                    angular_velocity_rad_s: motion.angular_velocity_rad_s,
                })
                .collect();
            primary_component_serial = None;
            components = vec![CreatureOwnedComponentV1 {
                owner_key,
                serial: None,
                translation_m: sampled.root_translation_m,
                rotation_xyzw: sampled.root_rotation_xyzw,
                linear_velocity_m_s: sampled.linear_velocity_m_s,
                angular_velocity_rad_s: sampled.angular_velocity_rad_s,
                source_masks: intact_masks(&parts),
            }];
        } else {
            part_velocities = Vec::new();
            let captured = self.world.physics().capture_dynamic_corpse_owner_at_epoch(
                id,
                DynamicCorpseOwnerFence {
                    owner_key,
                    source_generation: owner.source_generation,
                    topology_revision: owner.topology_token,
                    pose_token: owner.pose_token,
                    next_component_serial: owner.next_component_serial,
                },
                &sources,
                state.pose_epoch,
            )?;
            primary_component_serial = captured.components[0].component_id.map(|id| id.serial);
            components = captured
                .components
                .into_iter()
                .map(|component| {
                    Ok(CreatureOwnedComponentV1 {
                        owner_key,
                        serial: component.component_id.map(|id| id.serial),
                        translation_m: self
                            .world
                            .physics_origin()
                            .to_world_f64(component.translation_m),
                        rotation_xyzw: component.rotation_xyzw,
                        linear_velocity_m_s: component.linear_velocity_m_s,
                        angular_velocity_rad_s: component.angular_velocity_rad_s,
                        source_masks: corpse_masks(&parts, component.cells)?,
                    })
                })
                .collect::<Result<Vec<_>, CreatureCaptureError>>()?;
            let primary = &components[0];
            sampled.root_translation_m = primary.translation_m;
            sampled.root_rotation_xyzw = primary.rotation_xyzw;
            sampled.linear_velocity_m_s = primary.linear_velocity_m_s;
            sampled.angular_velocity_rad_s = primary.angular_velocity_rad_s;
            components.sort_by_key(|component| component.serial);
        }
        Ok(CreatureOwnershipRecordV4 {
            schema: CREATURE_OWNER_SCHEMA,
            state: sampled,
            owner_key,
            source_generation: owner.source_generation,
            next_component_serial: owner.next_component_serial,
            primary_component_serial,
            part_velocities,
            parts,
            components,
        })
    }
    fn capture_physical_sources(
        &self,
        key: spall_core::CreatureKey,
        parts: &[CreatureSourcePartV2],
    ) -> Result<Vec<SourcePart>, CreatureCaptureError> {
        parts
            .iter()
            .map(|part| {
                let volume = self.world.creature_source_part(key, part.part_id).unwrap();
                let grid = OccupancyGrid::from_volume(volume)
                    .map_err(|_| CreatureCaptureError::Inconsistent)?;
                let grid = match grid {
                    Some(grid) => grid,
                    None => OccupancyGrid::from_solid_mask(
                        spall_core::GlobalCell::new(0, 0, 0),
                        [1; 3],
                        vec![false],
                        vec![MaterialId::AIR],
                    )
                    .map_err(|_| CreatureCaptureError::Inconsistent)?,
                };
                Ok(SourcePart {
                    part_id: part.part_id,
                    parent_id: part.parent_id,
                    volume_id: part.source_volume_id,
                    source_revision: part.source_revision,
                    grid,
                    cell_m: volume.cell_size().metres() as f32,
                    local_transform: spall_physics::PartTransform {
                        translation_m: part.frozen_local_pose.translation_m,
                        rotation_xyzw: part.frozen_local_pose.rotation_xyzw,
                    },
                })
            })
            .collect::<Result<Vec<_>, CreatureCaptureError>>()
    }
}

fn local_pose(transform: spall_physics::PartTransform) -> CreatureLocalPoseV1 {
    CreatureLocalPoseV1 {
        translation_m: transform.translation_m,
        rotation_xyzw: transform.rotation_xyzw,
    }
}

fn capture_source(
    volume: &Volume,
    binding: &CreaturePartBinding,
    metadata: &crate::world::CreatureSourceMetadata,
    applied: spall_physics::PartTransform,
    generation: u64,
) -> CreatureSourcePartV2 {
    let mut bricks = Vec::with_capacity(volume.resident_brick_count());
    volume.for_each_resident_brick(|coord, brick| {
        let cells = match brick.uniform_storage_material() {
            Some(material) => BaselineCells::Uniform(material.0),
            None => {
                let snapshot = brick.snapshot();
                let ids = (0..spall_core::CELLS_PER_BRICK)
                    .map(|index| {
                        snapshot
                            .get(LocalCell::from_linear_index(index as u16).unwrap())
                            .0
                    })
                    .collect::<Vec<_>>();
                if ids.iter().all(|id| *id == ids[0]) {
                    BaselineCells::Uniform(ids[0])
                } else {
                    BaselineCells::Dense(ids)
                }
            }
        };
        bricks.push(BaselineBrick {
            coord: [coord.x, coord.y, coord.z],
            revision: brick.revision().get(),
            edited: brick.is_edited(),
            cells,
        });
    });
    bricks.sort_by_key(|brick| (brick.coord[2], brick.coord[1], brick.coord[0]));
    let mut part = CreatureSourcePartV2 {
        next_brick_revision: volume.next_revision().get(),
        part_id: binding.part_id,
        source_volume_id: volume.id(),
        source_generation: generation,
        source_revision: binding.source_revision,
        cell_size_code: volume.cell_size().to_u8(),
        bounds: volume
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
        source_content_hash: Hash32::ZERO,
        parent_id: metadata.parent_id,
        initial_local_pose: local_pose(metadata.initial_local_transform),
        frozen_local_pose: local_pose(applied),
        bricks,
    };
    part.source_content_hash = part.canonical_source_content_hash();
    part
}

fn intact_masks(parts: &[CreatureSourcePartV2]) -> Vec<CreatureSourceMaskBrickV1> {
    let occupied = parts
        .iter()
        .flat_map(|part| &part.bricks)
        .filter(|brick| !matches!(brick.cells, BaselineCells::Uniform(0)))
        .count();
    let mut masks = Vec::with_capacity(occupied);
    for part in parts {
        for brick in &part.bricks {
            let cells: Vec<CreatureMaskCellV1> = match &brick.cells {
                BaselineCells::Uniform(0) => continue,
                BaselineCells::Uniform(material) => (0..spall_core::CELLS_PER_BRICK)
                    .map(|index| CreatureMaskCellV1 {
                        index: index as u16,
                        material_id: *material,
                    })
                    .collect(),
                BaselineCells::Dense(ids) => {
                    let mut cells = Vec::with_capacity(ids.iter().filter(|id| **id != 0).count());
                    for (index, id) in ids.iter().enumerate() {
                        if *id != 0 {
                            cells.push(CreatureMaskCellV1 {
                                index: index as u16,
                                material_id: *id,
                            });
                        }
                    }
                    cells
                }
                BaselineCells::Digest { .. } => unreachable!("source capture never emits a digest"),
            };
            if !cells.is_empty() {
                masks.push(CreatureSourceMaskBrickV1 {
                    volume_id: part.source_volume_id,
                    part_id: part.part_id,
                    coord: brick.coord,
                    source_revision: part.source_revision,
                    brick_revision: brick.revision,
                    cells,
                });
            }
        }
    }
    masks.sort_by_key(|mask| {
        (
            mask.volume_id,
            mask.part_id,
            mask.coord[2],
            mask.coord[1],
            mask.coord[0],
        )
    });
    masks
}

fn corpse_masks(
    parts: &[CreatureSourcePartV2],
    cells: Vec<spall_physics::fracture::SourceCellRef>,
) -> Result<Vec<CreatureSourceMaskBrickV1>, CreatureCaptureError> {
    let mut grouped = BTreeMap::<_, Vec<CreatureMaskCellV1>>::new();
    for cell in cells {
        let c = cell.cell.map(i64::from);
        let coord = c.map(|v| v.div_euclid(32));
        let local = c.map(|v| v.rem_euclid(32));
        grouped
            .entry((
                cell.volume_id,
                cell.part_id,
                coord[2],
                coord[1],
                coord[0],
                cell.source_revision,
            ))
            .or_default()
            .push(CreatureMaskCellV1 {
                index: (local[0] + 32 * local[1] + 1024 * local[2]) as u16,
                material_id: cell.material.0,
            });
    }
    grouped
        .into_iter()
        .map(
            |((volume_id, part_id, z, y, x, source_revision), mut cells)| {
                let part = parts
                    .iter()
                    .find(|p| p.part_id == part_id && p.source_volume_id == volume_id)
                    .ok_or(CreatureCaptureError::Inconsistent)?;
                let brick = part
                    .bricks
                    .iter()
                    .find(|b| b.coord == [x, y, z])
                    .ok_or(CreatureCaptureError::Inconsistent)?;
                cells.sort_by_key(|cell| cell.index);
                Ok(CreatureSourceMaskBrickV1 {
                    volume_id,
                    part_id,
                    coord: [x, y, z],
                    source_revision,
                    brick_revision: brick.revision,
                    cells,
                })
            },
        )
        .collect()
}

#[cfg(test)]
#[path = "creature_capture_tests.rs"]
mod tests;
