//! Cold installation after geometry recovery; no IDs or journal entries are produced.
use super::*;
use crate::{
    creatures::CreatureHost,
    world::{CreatureSourceStore, GeometryCommitFence},
};
use spall_core::{GlobalCell, MaterialId, WorldId};
use spall_physics::{
    AliveArticulatedPartMotion, AliveArticulatedRestore, ArticulatedBatchOperation,
    DerivedFractureComponentId, DynamicCorpseComponentRestore, DynamicCorpseRestore,
    MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS, MAX_CORPSE_RESTORE_BATCH_OWNERS,
    MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS, OccupancyGrid, PartTransform, PreparedArticulatedBatch,
    Representation, SourceCellRef, SourcePart,
};
use spall_protocol::{
    CreatureLifecycleState, CreatureLocalPoseV1, CreatureOwnershipError, CreatureSnapshotV2,
    CreatureValidationCatalog, Hash32, content_manifest_hash,
};

#[derive(Debug, thiserror::Error)]
pub enum CreatureRestoreError {
    #[error(
        "cold animal restore requires an uninitialized host, empty source indexes and matching world/counters/catalog"
    )]
    Context,
    #[error("prepared animal restore no longer matches the world")]
    Stale,
    #[error("animal restore exceeds the bounded physical grid or coordinate budget")]
    Limit,
    #[error("animal source reconstruction failed: {0}")]
    Source(String),
    #[error(transparent)]
    Portable(#[from] CreatureOwnershipError),
    #[error(transparent)]
    Physics(#[from] spall_physics::ArticulatedError),
}

/// Opaque, unpublished, world-bound candidate. Commit immediately revalidates
/// all participants before publishing any physics, sources or host state.
pub struct PreparedCreatureRestore {
    fence: GeometryCommitFence,
    catalog_hash: Hash32,
    host: CreatureHost,
    sources: CreatureSourceStore,
    keys: Vec<spall_core::CreatureKey>,
    physics: Option<PreparedArticulatedBatch>,
}

fn transform(pose: CreatureLocalPoseV1) -> PartTransform {
    PartTransform {
        translation_m: pose.translation_m,
        rotation_xyzw: pose.rotation_xyzw,
    }
}

// Bound every catalog entry before fingerprinting, including unused entries and
// the empty-population path (record validation only visits referenced assets).
fn catalog_hash(catalog: &CreatureValidationCatalog<'_>) -> Result<Hash32, CreatureRestoreError> {
    use spall_protocol::*;
    if catalog.assets.len() > MAX_CREATURE_CATALOG_ASSETS
        || catalog.species.len() > MAX_CREATURE_CATALOG_SPECIES
        || catalog.material_mapping.len() > spall_core::MAX_MATERIALS
        || catalog.assets.iter().any(|asset| {
            asset.part_ids.len() > MAX_CREATURE_ASSET_PARTS
                || asset.clip_names.len() > MAX_CREATURE_ASSET_CLIPS
                || asset.portable_material_keys.len() > MAX_CREATURE_ASSET_MATERIAL_KEYS
                || asset
                    .clip_names
                    .iter()
                    .any(|name| name.len() > MAX_CREATURE_CLIP_NAME_BYTES)
                || asset
                    .portable_material_keys
                    .iter()
                    .any(|key| key.len() > 255)
        })
        || catalog
            .species
            .iter()
            .any(|entry| entry.key.len() > MAX_CREATURE_SPECIES_KEY_BYTES)
        || catalog
            .material_mapping
            .iter()
            .any(|entry| entry.portable_key.len() > 255)
    {
        return Err(CreatureRestoreError::Limit);
    }
    Ok(creature_catalog_fingerprint(catalog))
}

// Restore must not silently round an authoritative saved world position when
// the geometry's local frame cannot represent it exactly.
fn restore_translation(
    world: &SimWorld,
    position: [f64; 3],
) -> Result<[f32; 3], CreatureRestoreError> {
    let local = world
        .physics_translation(position)
        .map_err(|_| CreatureRestoreError::Limit)?;
    if world.physics_origin().to_world_f64(local) != position {
        return Err(CreatureRestoreError::Limit);
    }
    Ok(local)
}

fn mask_cell(coord: [i64; 3], index: u16) -> Result<[i32; 3], CreatureRestoreError> {
    let offsets = [index % 32, (index / 32) % 32, index / 1024];
    let mut cell = [0; 3];
    for axis in 0..3 {
        cell[axis] = coord[axis]
            .checked_mul(32)
            .and_then(|base| base.checked_add(i64::from(offsets[axis])))
            .and_then(|value| i32::try_from(value).ok())
            .ok_or(CreatureRestoreError::Limit)?;
    }
    Ok(cell)
}

impl Simulation {
    /// Prepares a complete saved animal population after geometry recovery.
    /// Geometry must already carry exactly the snapshot's shared volume counter.
    /// Even an initialized empty host is refused; this never replaces live actors.
    pub fn prepare_creature_restore(
        &self,
        expected_world: WorldId,
        snapshot: &CreatureSnapshotV2,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<PreparedCreatureRestore, CreatureRestoreError> {
        let hash = catalog_hash(catalog)?;
        if self.creatures.is_some()
            || self.world.creature_source_counts() != [0; 5]
            || expected_world != snapshot.allocator.world_id
            || self.world.registry().counters().1 != snapshot.next_volume_id
            || content_manifest_hash(catalog.materials)
                != content_manifest_hash(self.world.materials())
        {
            return Err(CreatureRestoreError::Context);
        }
        // Validation admits the entire portable payload before source copies.
        let reconstructed = crate::reconstruct_creature_sources(snapshot, catalog)
            .map_err(|error| CreatureRestoreError::Source(error.to_string()))?;
        if reconstructed.len() > MAX_CORPSE_RESTORE_BATCH_OWNERS {
            return Err(CreatureRestoreError::Limit);
        }
        let fence = self
            .world
            .capture_geometry_commit_fence(
                self.world.terrain().volume.id(),
                &[],
                self.journal.cursor(),
            )
            .map_err(CreatureRestoreError::Source)?;
        let mut total_grids = 0_u64;
        let mut total_solids = 0_u64;
        // Complete physical-span admission BEFORE allocating any grids/masks.
        for (record, sources) in snapshot.owners.records.iter().zip(&reconstructed) {
            let mut grids = 0;
            let mut solids = 0;
            for (_, volume) in &sources.parts {
                if self.world.volume_ref(volume.id()).is_some() {
                    return Err(CreatureRestoreError::Context);
                }
                let mut count = 0_u64;
                volume.for_each_resident_brick(|_, brick| count += u64::from(brick.solid_cells()));
                let cells = if count == 0 {
                    1
                } else {
                    creature_source_work(volume)
                        .map_err(|_| CreatureRestoreError::Limit)?
                        .grid_cells
                };
                grids += cells;
                solids += count;
            }
            total_grids += grids;
            total_solids += solids;
            if grids > spall_physics::articulated::MAX_ARTICULATED_DENSE_CELLS
                || solids > spall_physics::articulated::MAX_ARTICULATED_SOLID_CELLS
                || total_grids > MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
                || total_solids > MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS
            {
                return Err(CreatureRestoreError::Limit);
            }
            for component in &record.components {
                for mask in &component.source_masks {
                    for cell in &mask.cells {
                        mask_cell(mask.coord, cell.index)?;
                    }
                }
            }
        }
        let host = CreatureHost::from_ownership_snapshot(snapshot, catalog)?;
        let mut staged_sources = CreatureSourceStore::default();
        let mut keys = Vec::with_capacity(reconstructed.len());
        let mut operations = Vec::with_capacity(reconstructed.len());
        for (record, sources) in snapshot.owners.records.iter().zip(reconstructed) {
            let mut physical_parts = Vec::with_capacity(sources.parts.len());
            let mut parts = Vec::with_capacity(sources.parts.len());
            let mut poses = Vec::with_capacity(sources.parts.len());
            for (portable, (part_id, volume)) in record.parts.iter().zip(sources.parts) {
                let grid = OccupancyGrid::from_volume(&volume)
                    .map_err(|error| CreatureRestoreError::Source(error.to_string()))?;
                let grid = match grid {
                    Some(grid) => grid,
                    None => OccupancyGrid::from_solid_mask(
                        GlobalCell::new(0, 0, 0),
                        [1; 3],
                        vec![false],
                        vec![MaterialId::AIR],
                    )
                    .map_err(|error| CreatureRestoreError::Source(error.to_string()))?,
                };
                let applied = transform(portable.frozen_local_pose);
                physical_parts.push(SourcePart {
                    part_id,
                    parent_id: portable.parent_id,
                    volume_id: volume.id(),
                    source_revision: portable.source_revision,
                    grid,
                    cell_m: volume.cell_size().metres() as f32,
                    local_transform: applied,
                });
                let metadata = creature_source_freshness(
                    &volume,
                    portable.parent_id,
                    transform(portable.initial_local_pose),
                )
                .ok_or_else(|| CreatureRestoreError::Source("source provenance".into()))?;
                parts.push((part_id, volume, metadata));
                poses.push((part_id, applied));
            }
            let operation_tag = keys.len() as u64 + 1;
            let state = &record.state;
            let corpse = state.lifecycle == CreatureLifecycleState::Corpse;
            let operation = if !corpse {
                ArticulatedBatchOperation::AliveRestore {
                    operation_tag,
                    representation: Representation::MergedCuboids,
                    restore: AliveArticulatedRestore {
                        owner_key: record.owner_key,
                        source_generation: record.source_generation,
                        topology_token: state.aggregate_topology_revision,
                        pose_token: state.pose_epoch,
                        next_component_serial: record.next_component_serial,
                        root_translation_m: restore_translation(
                            &self.world,
                            state.root_translation_m,
                        )?,
                        root_rotation_xyzw: state.root_rotation_xyzw,
                        root_linear_velocity_m_s: state.linear_velocity_m_s,
                        root_angular_velocity_rad_s: state.angular_velocity_rad_s,
                        part_motion: record
                            .part_velocities
                            .iter()
                            .map(|part| AliveArticulatedPartMotion {
                                part_id: part.part_id,
                                linear_velocity_m_s: part.linear_velocity_m_s,
                                angular_velocity_rad_s: part.angular_velocity_rad_s,
                            })
                            .collect(),
                        parts: physical_parts,
                    },
                }
            } else {
                let mut components = Vec::with_capacity(record.components.len());
                for component in &record.components {
                    let mut cells = Vec::with_capacity(
                        component
                            .source_masks
                            .iter()
                            .map(|mask| mask.cells.len())
                            .sum(),
                    );
                    for mask in &component.source_masks {
                        for cell in &mask.cells {
                            cells.push(SourceCellRef {
                                volume_id: mask.volume_id,
                                part_id: mask.part_id,
                                cell: mask_cell(mask.coord, cell.index)?,
                                material: MaterialId(cell.material_id),
                                source_revision: mask.source_revision,
                            });
                        }
                    }
                    components.push(DynamicCorpseComponentRestore {
                        component_id: component.serial.map(|serial| DerivedFractureComponentId {
                            owner_key: record.owner_key,
                            serial,
                        }),
                        translation_m: restore_translation(&self.world, component.translation_m)?,
                        rotation_xyzw: component.rotation_xyzw,
                        linear_velocity_m_s: component.linear_velocity_m_s,
                        angular_velocity_rad_s: component.angular_velocity_rad_s,
                        cells,
                    });
                }
                // Portable order is canonical serial order; runtime root is first.
                let primary = record
                    .components
                    .iter()
                    .position(|component| component.serial == record.primary_component_serial)
                    .ok_or(CreatureRestoreError::Context)?;
                components.swap(0, primary);
                ArticulatedBatchOperation::ColdRestore {
                    operation_tag,
                    representation: Representation::MergedCuboids,
                    restore: DynamicCorpseRestore {
                        owner_key: record.owner_key,
                        source_generation: record.source_generation,
                        topology_revision: state.aggregate_topology_revision,
                        pose_epoch: state.pose_epoch,
                        next_component_serial: record.next_component_serial,
                        source_parts: physical_parts,
                        components,
                    },
                }
            };
            staged_sources.stage_owner(state.key, parts, corpse.then_some(poses));
            keys.push(state.key);
            operations.push(operation);
        }
        let physics = if operations.is_empty() {
            None
        } else {
            Some(
                self.world
                    .physics()
                    .prepare_articulated_batch(operations, |material| {
                        self.world.density(material)
                    })?,
            )
        };
        Ok(PreparedCreatureRestore {
            fence,
            catalog_hash: hash,
            host,
            sources: staged_sources,
            keys,
            physics,
        })
    }

    /// Revalidates every fence, then publishes without allocating source/host
    /// maps, changing registry counters or appending journal entries.
    pub fn commit_prepared_creature_restore(
        &mut self,
        mut prepared: PreparedCreatureRestore,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureRestoreError> {
        if self.creatures.is_some()
            || self.world.creature_source_counts() != [0; 5]
            || catalog_hash(catalog)? != prepared.catalog_hash
            || !self
                .world
                .validates_geometry_commit_fence(&prepared.fence, self.journal.cursor())
            || prepared
                .sources
                .volume_ids()
                .any(|id| self.world.volume_ref(id).is_some())
        {
            return Err(CreatureRestoreError::Stale);
        }
        let physics = prepared
            .physics
            .take()
            .map(|physics| self.world.physics().validate_articulated_batch(physics))
            .transpose()?;
        // No fallible checks or source/host map allocations after this point.
        if let Some(physics) = physics {
            for outcome in self
                .world
                .physics_mut()
                .publish_validated_articulated_batch(physics)
            {
                prepared.sources.bind_owner(
                    prepared.keys[outcome.operation_tag as usize - 1],
                    outcome.owner_id,
                );
            }
        }
        self.world
            .publish_restored_creature_sources(prepared.sources);
        self.creatures = Some(prepared.host);
        Ok(())
    }
}

#[cfg(test)]
#[path = "creature_restore_tests.rs"]
mod tests;

#[cfg(test)]
pub(super) fn assert_joint_roundtrip(
    snapshot: &CreatureSnapshotV2,
    catalog: &CreatureValidationCatalog<'_>,
) -> Simulation {
    let mut target = tests::target(snapshot);
    let counters = target.world.registry().counters();
    let cursor = target.journal.cursor();
    let decoded =
        CreatureSnapshotV2::decode(&postcard::to_stdvec(snapshot).unwrap(), catalog).unwrap();
    let prepared = target
        .prepare_creature_restore(snapshot.allocator.world_id, &decoded, catalog)
        .unwrap();
    assert!(target.creatures.is_none());
    assert_eq!(target.world.creature_source_counts(), [0; 5]);
    target
        .commit_prepared_creature_restore(prepared, catalog)
        .unwrap();
    let captured = target.capture_creature_snapshot(catalog).unwrap().unwrap();
    assert_eq!(captured, *snapshot);
    assert_eq!(captured.canonical_hash(), snapshot.canonical_hash());
    assert_eq!(target.world.registry().counters(), counters);
    assert_eq!(target.journal.cursor(), cursor);
    target
}
