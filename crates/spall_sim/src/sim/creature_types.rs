//! Creature lifecycle input/output types and source-fence helpers.

use super::*;

/// One source part of an authoritative creature birth.
pub struct CreatureBirthPartInput {
    pub part_id: u32,
    pub parent_id: Option<u32>,
    pub local_transform: spall_physics::PartTransform,
    pub volume: spall_voxel::Volume,
}

pub(super) fn creature_source_freshness(
    volume: &spall_voxel::Volume,
    parent_id: Option<u32>,
    local_transform: spall_physics::PartTransform,
) -> Option<crate::world::CreatureSourceMetadata> {
    let mut brick_revisions = Vec::new();
    for coord in volume.resident_brick_coords() {
        let revision = volume.brick_revision(coord).ok().flatten()?;
        brick_revisions.push((coord, revision.0));
    }
    Some(crate::world::CreatureSourceMetadata {
        parent_id,
        initial_local_transform: local_transform,
        state_stamp: volume.state_stamp(),
        content_hash: crate::world::volume_topology_hash_for(
            volume,
            spall_protocol::CanonicalOwner::Terrain,
        ),
        brick_revisions,
    })
}

/// Server-owned input to the ordered actor lifecycle transaction. These
/// inputs are deliberately crate-private: network callers can never supply a
/// physics death witness or register a live creature directly.
/// Server-owned lifecycle request applied atomically at a tick boundary.
pub enum AuthoritativeCreatureIntent {
    Birth {
        state: Box<spall_protocol::CreatureStateV1>,
        parts: Vec<CreatureBirthPartInput>,
        representation: spall_physics::Representation,
    },
    Death {
        key: spall_core::CreatureKey,
        expected_state_revision: u64,
        expected_topology_token: u64,
        expected_pose_token: u64,
    },
}

/// Result of a committed lifecycle transaction, for ordered publication.
pub struct CommittedCreatureLifecycle {
    pub transaction_id: spall_core::TransactionId,
    pub journal_seq: spall_core::JournalSeq,
    pub attachments: Option<spall_protocol::TransactionAttachmentSetV1>,
    pub topology: spall_protocol::TopologyTransaction,
    pub witnesses: Vec<spall_protocol::CreatureDeathWitnessV1>,
}

/// Caller-owned IDs already burned by the common server transfer allocator.
/// Unused reservations stay burned; Simulation never invents transfer IDs.
#[derive(Default)]
pub(crate) struct CreatureTransactionTransfers {
    pub geometry: Option<spall_protocol::TransferId>,
    pub lifecycle: Vec<spall_protocol::TransferId>,
}

/// Owned mixed transaction candidate. It carries bounded deltas and the
/// geometry/physics candidates, never a clone of the simulation world.
pub(crate) struct PreparedSimulationTransaction {
    pub(super) world_identity: std::sync::Arc<()>,
    pub(super) registry_counters_before: (u64, u64, u64, u64),
    pub(super) physics_origin: PhysicsOrigin,
    pub(super) topology_epoch: spall_jobs::TopologyEpoch,
    pub(super) journal_cursor: u64,
    pub(super) actor_only_physics_fence: Option<spall_physics::BodyCommitFence>,
    pub(super) allocator_before: CreatureAllocatorSnapshotV1,
    pub(super) host_revision_before: u64,
    pub(super) catalog_hash: spall_protocol::Hash32,
    pub(super) candidate_host: crate::creatures::CreatureHost,
    pub(super) geometry: Option<Box<crate::commit::PreparedGeometryCommit>>,
    pub(super) actor_only_registry: Option<crate::registry::IdRegistry>,
    pub(super) physics: spall_physics::PreparedArticulatedBatch,
    pub(super) source_births: Vec<CreatureSourceBirth>,
    pub(super) death_source_fences: Vec<CreatureDeathSourceFence>,
    pub(super) frozen_pose_updates: Vec<(
        spall_core::CreatureKey,
        Vec<(u32, spall_physics::PartTransform)>,
    )>,
    pub(super) transaction_id: spall_core::TransactionId,
    pub(super) journal_seq: spall_core::JournalSeq,
    pub(super) attachments: Option<spall_protocol::TransactionAttachmentSetV1>,
    pub(super) topology: spall_protocol::TopologyTransaction,
    pub(super) participants: Vec<spall_protocol::MotionSnapshot>,
    pub(super) death_witnesses: Vec<spall_protocol::CreatureDeathWitnessV1>,
}

#[derive(Clone)]
pub(super) struct CreatureDeathSourcePartFence {
    pub(super) part_id: u32,
    pub(super) volume_id: spall_core::VolumeId,
    pub(super) state_stamp: u64,
    pub(super) content_hash: spall_protocol::Hash32,
    pub(super) brick_revisions: Vec<(spall_core::BrickCoord, u64)>,
    pub(super) metadata: crate::world::CreatureSourceMetadata,
}

#[derive(Clone)]
pub(super) struct CreatureDeathSourceFence {
    pub(super) key: spall_core::CreatureKey,
    pub(super) physics_owner: spall_physics::ArticulatedBodyId,
    pub(super) parts: Vec<CreatureDeathSourcePartFence>,
}

pub(super) fn creature_catalog_fingerprint(
    catalog: &spall_protocol::CreatureValidationCatalog<'_>,
) -> spall_protocol::Hash32 {
    let mut writer =
        spall_protocol::CanonicalWriter::with_domain(b"spall.sim-creature-catalog-fence.v1");
    writer
        .blob(&spall_protocol::content_manifest_hash(catalog.materials).0)
        .u32(catalog.material_mapping_version)
        .blob(&catalog.material_mapping_hash.0)
        .seq(catalog.assets.len());
    for asset in catalog.assets {
        writer
            .blob(&asset.uuid)
            .blob(&asset.logical_hash.0)
            .seq(asset.part_ids.len());
        for part_id in asset.part_ids {
            writer.u32(*part_id);
        }
        writer.seq(asset.clip_names.len());
        for name in asset.clip_names {
            writer.blob(name.as_bytes());
        }
        writer.seq(asset.portable_material_keys.len());
        for key in asset.portable_material_keys {
            writer.blob(key.as_bytes());
        }
    }
    writer.seq(catalog.species.len());
    for species in catalog.species {
        writer
            .blob(species.key.as_bytes())
            .u32(species.version)
            .u8(u8::from(species.enabled))
            .blob(&species.asset.uuid)
            .blob(&species.asset.logical_hash.0);
    }
    let mut mapping = catalog.material_mapping.iter().collect::<Vec<_>>();
    mapping.sort_unstable_by(|left, right| {
        left.portable_key
            .as_bytes()
            .cmp(right.portable_key.as_bytes())
    });
    writer.seq(mapping.len());
    for entry in mapping {
        writer
            .blob(entry.portable_key.as_bytes())
            .u16(entry.material_id.0);
    }
    writer.hash()
}

pub(crate) struct ValidatedSimulationTransaction {
    pub(super) geometry: Option<crate::commit::ValidatedGeometryCommit>,
    pub(super) physics: spall_physics::ValidatedArticulatedBatch,
    pub(super) candidate_host: crate::creatures::CreatureHost,
    pub(super) actor_only_registry: Option<crate::registry::IdRegistry>,
    pub(super) source_births: Vec<CreatureSourceBirth>,
    pub(super) frozen_pose_updates: Vec<(
        spall_core::CreatureKey,
        Vec<(u32, spall_physics::PartTransform)>,
    )>,
    pub(super) transaction_id: spall_core::TransactionId,
    pub(super) journal_seq: spall_core::JournalSeq,
    pub(super) attachments: Option<spall_protocol::TransactionAttachmentSetV1>,
    pub(super) topology: spall_protocol::TopologyTransaction,
    pub(super) participants: Vec<spall_protocol::MotionSnapshot>,
    pub(super) death_witnesses: Vec<spall_protocol::CreatureDeathWitnessV1>,
}

pub(super) type CreatureSourceBirth = (
    u64,
    spall_core::CreatureKey,
    Vec<(
        u32,
        spall_voxel::Volume,
        crate::world::CreatureSourceMetadata,
    )>,
);

pub(super) fn stable_creature_owner_key(key: spall_core::CreatureKey) -> u128 {
    let mut writer = spall_protocol::CanonicalWriter::with_domain(b"spall.creature-owner-key.v1");
    writer
        .blob(&key.world_id.to_u128().to_le_bytes())
        .u64(key.world_epoch.get())
        .u64(key.creature_id.get())
        .u32(key.generation.get());
    let digest = writer.hash();
    u128::from_le_bytes(digest.0[..16].try_into().expect("hash prefix length"))
}

pub(super) fn portable_birth_record(
    state: spall_protocol::CreatureStateV1,
    owner_key: u128,
    parts: &[(
        u32,
        &spall_voxel::Volume,
        &crate::world::CreatureSourceMetadata,
    )],
) -> Result<spall_protocol::CreatureOwnershipRecordV4, crate::creatures::CreatureScheduleError> {
    use spall_protocol::{
        BaselineCells, BaselineOwner, CreatureMaskCellV1, CreatureOwnedComponentV1,
        CreatureOwnershipRecordV4, CreatureSourceMaskBrickV1, CreatureSourcePartV2, Hash32,
        WIRE_SCHEMA_VERSION, baseline::BaselineBrick,
    };

    let mut portable_parts = Vec::with_capacity(parts.len());
    let mut component_masks = Vec::new();
    for (part_id, volume, metadata) in parts {
        let binding = state
            .source_bindings
            .iter()
            .find(|binding| binding.part_id == *part_id)
            .ok_or(crate::creatures::CreatureScheduleError::InvalidBatch)?;
        let baseline = crate::replication::baseline_volume_of(volume, BaselineOwner::Terrain, None);
        let mut bricks = Vec::with_capacity(baseline.bricks.len());
        for brick in baseline.bricks {
            let cells = match &brick.cells {
                BaselineCells::Uniform(material) => {
                    if *material != 0 {
                        component_masks.push(CreatureSourceMaskBrickV1 {
                            volume_id: binding.source_volume_id,
                            part_id: *part_id,
                            coord: brick.coord,
                            source_revision: binding.source_revision,
                            brick_revision: brick.revision,
                            cells: (0..32 * 32 * 32)
                                .map(|index| CreatureMaskCellV1 {
                                    index: index as u16,
                                    material_id: *material,
                                })
                                .collect(),
                        });
                    }
                    BaselineCells::Uniform(*material)
                }
                BaselineCells::Dense(ids) => {
                    let cells = ids
                        .iter()
                        .enumerate()
                        .filter_map(|(index, material_id)| {
                            (*material_id != 0).then_some(CreatureMaskCellV1 {
                                index: index as u16,
                                material_id: *material_id,
                            })
                        })
                        .collect::<Vec<_>>();
                    if !cells.is_empty() {
                        component_masks.push(CreatureSourceMaskBrickV1 {
                            volume_id: binding.source_volume_id,
                            part_id: *part_id,
                            coord: brick.coord,
                            source_revision: binding.source_revision,
                            brick_revision: brick.revision,
                            cells,
                        });
                    }
                    BaselineCells::Dense(ids.clone())
                }
                other => other.clone(),
            };
            bricks.push(BaselineBrick { cells, ..brick });
        }
        let local_pose = spall_protocol::CreatureLocalPoseV1 {
            translation_m: metadata.initial_local_transform.translation_m,
            rotation_xyzw: metadata.initial_local_transform.rotation_xyzw,
        };
        let mut part = CreatureSourcePartV2 {
            next_brick_revision: volume.next_revision().get(),
            part_id: *part_id,
            source_volume_id: binding.source_volume_id,
            source_generation: 1,
            source_revision: binding.source_revision,
            cell_size_code: volume.cell_size().to_u8(),
            bounds: volume.bounds().map(|bounds| {
                [
                    [bounds.min.x, bounds.min.y, bounds.min.z],
                    [bounds.max.x, bounds.max.y, bounds.max.z],
                ]
            }),
            source_content_hash: Hash32::ZERO,
            parent_id: metadata.parent_id,
            initial_local_pose: local_pose,
            frozen_local_pose: local_pose,
            bricks,
        };
        part.source_content_hash = part.canonical_source_content_hash();
        portable_parts.push(part);
    }
    component_masks.sort_by_key(|mask| {
        (
            mask.volume_id.get(),
            mask.part_id,
            mask.coord[2],
            mask.coord[1],
            mask.coord[0],
        )
    });
    Ok(CreatureOwnershipRecordV4 {
        primary_component_serial: None,
        part_velocities: portable_parts
            .iter()
            .map(|part| spall_protocol::CreaturePartVelocityV2 {
                part_id: part.part_id,
                linear_velocity_m_s: [0.0; 3],
                angular_velocity_rad_s: [0.0; 3],
            })
            .collect(),
        schema: WIRE_SCHEMA_VERSION,
        components: vec![CreatureOwnedComponentV1 {
            owner_key,
            serial: None,
            translation_m: state.root_translation_m,
            rotation_xyzw: state.root_rotation_xyzw,
            linear_velocity_m_s: state.linear_velocity_m_s,
            angular_velocity_rad_s: state.angular_velocity_rad_s,
            source_masks: component_masks,
        }],
        state,
        owner_key,
        source_generation: 1,
        next_component_serial: 1,
        parts: portable_parts,
    })
}

#[derive(Clone, Copy, Default)]
pub(super) struct CreatureSourceWork {
    pub(super) bricks: usize,
    pub(super) grid_cells: u64,
    pub(super) solid_cells: u64,
    pub(super) mask_bricks: usize,
}

pub(super) fn creature_source_work(
    volume: &spall_voxel::Volume,
) -> Result<CreatureSourceWork, String> {
    let bricks = volume.resident_brick_count();
    if bricks > spall_protocol::MAX_CREATURE_OWNER_BRICKS {
        return Err("creature source has too many resident bricks".into());
    }
    let mut min = [i64::MAX; 3];
    let mut max = [i64::MIN; 3];
    let mut solid_cells = 0_u64;
    let mut mask_bricks = 0_usize;
    let mut invalid_coordinates = false;
    volume.for_each_resident_brick(|coord, brick| {
        if invalid_coordinates {
            return;
        }
        let brick_solids = u64::from(brick.solid_cells());
        solid_cells = solid_cells.saturating_add(brick_solids);
        if brick_solids == 0 {
            return;
        }
        mask_bricks = mask_bricks.saturating_add(1);
        let Some(base) = coord
            .x
            .checked_mul(32)
            .zip(coord.y.checked_mul(32))
            .zip(coord.z.checked_mul(32))
            .map(|((x, y), z)| [x, y, z])
        else {
            invalid_coordinates = true;
            return;
        };
        if brick_solids == 32 * 32 * 32 {
            for axis in 0..3 {
                min[axis] = min[axis].min(base[axis]);
                let Some(end) = base[axis].checked_add(31) else {
                    invalid_coordinates = true;
                    return;
                };
                max[axis] = max[axis].max(end);
            }
            return;
        }
        let snapshot = brick.snapshot();
        for index in 0..(32 * 32 * 32) as u16 {
            let cell = spall_core::LocalCell::from_linear_index(index)
                .expect("bounded source brick index");
            if snapshot.get(cell).is_air() {
                continue;
            }
            let Some(global) = base[0]
                .checked_add(i64::from(cell.x()))
                .zip(base[1].checked_add(i64::from(cell.y())))
                .zip(base[2].checked_add(i64::from(cell.z())))
                .map(|((x, y), z)| [x, y, z])
            else {
                invalid_coordinates = true;
                return;
            };
            for axis in 0..3 {
                min[axis] = min[axis].min(global[axis]);
                max[axis] = max[axis].max(global[axis]);
            }
        }
    });
    if invalid_coordinates {
        return Err("creature source brick coordinate overflows cell space".into());
    }
    if solid_cells == 0 {
        return Err("creature source has no solid occupancy".into());
    }
    let extent = |axis: usize| {
        max[axis]
            .checked_sub(min[axis])
            .and_then(|value| value.checked_add(1))
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| "invalid source grid extent".to_owned())
    };
    let dims = [extent(0)?, extent(1)?, extent(2)?];
    let grid_cells = dims[0]
        .checked_mul(dims[1])
        .and_then(|value| value.checked_mul(dims[2]))
        .ok_or_else(|| "source grid work overflow".to_owned())?;
    if grid_cells > spall_physics::articulated::MAX_ARTICULATED_DENSE_CELLS {
        return Err("creature source grid exceeds the articulated owner limit".into());
    }
    Ok(CreatureSourceWork {
        bricks,
        grid_cells,
        solid_cells,
        mask_bricks,
    })
}
