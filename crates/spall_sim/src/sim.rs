//! [`Simulation`] — the authoritative loop that ties staging, commit, and
//! physics together behind a single [`Simulation::tick`].
//!
//! One `tick` runs the in-scope steps of the `docs/architecture.md` tick order:
//! drain accepted intents, validate and commit prepared transactions in
//! server-assigned order with matching collision / ownership updates, advance
//! physics one fixed step, refresh the extracted body poses, and advance the
//! player capsules (T19). Contact-to-intent conversion (T21), replication (T10)
//! and persistence (T16) are handled by the integrator, not here.

use std::collections::{HashSet, VecDeque};

use glam::DVec3;
use spall_core::{
    BRUSH_UNIT, BrushPoint, CreatureAllocatorSnapshotV1, EntityId, GlobalCell, IdError, MaterialId,
    PlayerInput, SphereBrush, Tick, WorldId,
};
use spall_physics::PhysicsOrigin;
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{ActionStatus, InputSeq, RequestId};

use crate::commit::CommitError;
use crate::contact_damage::{ContactDamagePlan, ContactDamagePolicy, ContactEvent};
use crate::dormancy::{ActiveRegion, BodyDormancyInput, DormancyPlan, DormancyPolicy};
use crate::intent::{EditIntent, EditKind, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::player::transaction_world_box;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, TerrainColliderMode, WorldSetup};
use spall_voxel::Sample;

/// One source part of an authoritative creature birth.
pub struct CreatureBirthPartInput {
    pub part_id: u32,
    pub parent_id: Option<u32>,
    pub local_transform: spall_physics::PartTransform,
    pub volume: spall_voxel::Volume,
}

fn creature_source_freshness(
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
    world_identity: std::sync::Arc<()>,
    registry_counters_before: (u64, u64, u64, u64),
    physics_origin: PhysicsOrigin,
    topology_epoch: spall_jobs::TopologyEpoch,
    journal_cursor: u64,
    actor_only_physics_fence: Option<spall_physics::BodyCommitFence>,
    allocator_before: CreatureAllocatorSnapshotV1,
    host_revision_before: u64,
    catalog_hash: spall_protocol::Hash32,
    candidate_host: crate::creatures::CreatureHost,
    geometry: Option<Box<crate::commit::PreparedGeometryCommit>>,
    actor_only_registry: Option<crate::registry::IdRegistry>,
    physics: spall_physics::PreparedArticulatedBatch,
    source_births: Vec<CreatureSourceBirth>,
    death_source_fences: Vec<CreatureDeathSourceFence>,
    frozen_pose_updates: Vec<(
        spall_core::CreatureKey,
        Vec<(u32, spall_physics::PartTransform)>,
    )>,
    transaction_id: spall_core::TransactionId,
    journal_seq: spall_core::JournalSeq,
    attachments: Option<spall_protocol::TransactionAttachmentSetV1>,
    topology: spall_protocol::TopologyTransaction,
    participants: Vec<spall_protocol::MotionSnapshot>,
    death_witnesses: Vec<spall_protocol::CreatureDeathWitnessV1>,
}

#[derive(Clone)]
struct CreatureDeathSourcePartFence {
    part_id: u32,
    volume_id: spall_core::VolumeId,
    state_stamp: u64,
    content_hash: spall_protocol::Hash32,
    brick_revisions: Vec<(spall_core::BrickCoord, u64)>,
    metadata: crate::world::CreatureSourceMetadata,
}

#[derive(Clone)]
struct CreatureDeathSourceFence {
    key: spall_core::CreatureKey,
    physics_owner: spall_physics::ArticulatedBodyId,
    parts: Vec<CreatureDeathSourcePartFence>,
}

fn creature_catalog_fingerprint(
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
    geometry: Option<crate::commit::ValidatedGeometryCommit>,
    physics: spall_physics::ValidatedArticulatedBatch,
    candidate_host: crate::creatures::CreatureHost,
    actor_only_registry: Option<crate::registry::IdRegistry>,
    source_births: Vec<CreatureSourceBirth>,
    frozen_pose_updates: Vec<(
        spall_core::CreatureKey,
        Vec<(u32, spall_physics::PartTransform)>,
    )>,
    transaction_id: spall_core::TransactionId,
    journal_seq: spall_core::JournalSeq,
    attachments: Option<spall_protocol::TransactionAttachmentSetV1>,
    topology: spall_protocol::TopologyTransaction,
    participants: Vec<spall_protocol::MotionSnapshot>,
    death_witnesses: Vec<spall_protocol::CreatureDeathWitnessV1>,
}

type CreatureSourceBirth = (
    u64,
    spall_core::CreatureKey,
    Vec<(
        u32,
        spall_voxel::Volume,
        crate::world::CreatureSourceMetadata,
    )>,
);

fn stable_creature_owner_key(key: spall_core::CreatureKey) -> u128 {
    let mut writer = spall_protocol::CanonicalWriter::with_domain(b"spall.creature-owner-key.v1");
    writer
        .blob(&key.world_id.to_u128().to_le_bytes())
        .u64(key.world_epoch.get())
        .u64(key.creature_id.get())
        .u32(key.generation.get());
    let digest = writer.hash();
    u128::from_le_bytes(digest.0[..16].try_into().expect("hash prefix length"))
}

fn portable_birth_record(
    state: spall_protocol::CreatureStateV1,
    owner_key: u128,
    parts: &[(
        u32,
        &spall_voxel::Volume,
        &crate::world::CreatureSourceMetadata,
    )],
) -> Result<spall_protocol::CreatureOwnershipRecordV4, crate::creatures::CreatureScheduleError> {
    use spall_protocol::{
        BaselineCells, BaselineOwner, CREATURE_OWNER_SCHEMA, CreatureMaskCellV1,
        CreatureOwnedComponentV1, CreatureOwnershipRecordV4, CreatureSourceMaskBrickV1,
        CreatureSourcePartV2, Hash32, baseline::BaselineBrick,
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
        schema: CREATURE_OWNER_SCHEMA,
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
struct CreatureSourceWork {
    bricks: usize,
    grid_cells: u64,
    solid_cells: u64,
    mask_bricks: usize,
}

fn creature_source_work(volume: &spall_voxel::Volume) -> Result<CreatureSourceWork, String> {
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

#[cfg(test)]
#[path = "creature_bulk_publication_tests.rs"]
mod creature_bulk_publication_tests;

#[cfg(test)]
#[path = "creature_lifecycle_review.rs"]
mod creature_lifecycle_review;

#[cfg(test)]
#[path = "creature_owner_bridge.rs"]
mod creature_owner_bridge;

#[path = "creature_capture.rs"]
mod creature_capture;
pub use creature_capture::CreatureCaptureError;

#[path = "creature_restore.rs"]
mod creature_restore;
pub use creature_restore::{CreatureRestoreError, PreparedCreatureRestore};

/// Reserved high bit for a server-authored [`RequestId`]. Contact-damage cuts
/// (T21) are minted by the server, not a client, so their request ids sit in a
/// band a wire `RequestId` never reaches — a client action claiming a value at
/// or above this is rejected at ingress.
pub const SERVER_REQUEST_ID_BAND: u64 = 1 << 62;

/// Journal provenance for a server-authored contact-damage cut. Authority is the
/// server's; `actor` is only journal provenance. Sits above the per-session
/// `actor_for` band and below the reserved player band (`1 << 48`).
pub const CONTACT_DAMAGE_ACTOR_ID: u64 = 1 << 40;

/// Journal provenance for a server-authored dam-gate toggle (an admin command,
/// not a player edit). Its own reserved id beside [`CONTACT_DAMAGE_ACTOR_ID`],
/// both below the reserved player band (`1 << 48`).
pub const DAM_GATE_ACTOR_ID: u64 = (1 << 40) + 1;

/// Largest single covering sphere [`Simulation::set_dam_gate`] will cut or
/// fill, in cells. A dam gate is typically authored as a flat, wide notch —
/// far from spherical — so covering it with clusters this size (rather than
/// one sphere sized to the whole shape) keeps the cut close to the authored
/// outline instead of ballooning out to the shape's own diagonal.
const DAM_GATE_MAX_SPHERE_RADIUS_CELLS: i64 = 5;

/// The fixed server timestep: 60 Hz (`docs/architecture.md`).
pub const TICK_DT_S: f32 = 1.0 / 60.0;

/// Tunables for a [`Simulation`].
pub struct SimulationConfig {
    pub world: WorldSetup,
    /// Per-brick collision is the adopted default. Whole-terrain is retained
    /// only for explicit comparison runs.
    pub terrain_collider_mode: TerrainColliderMode,
    /// Maximum accepted-but-unstaged intents held before backpressure.
    pub max_pending_intents: usize,
    /// Consecutive commit conflicts on one region before it is routed through
    /// the serial queue.
    pub serialize_threshold: u32,
    /// Stacked-column water (ENG-167). `None` preserves non-fluid fixtures.
    pub column_water: Option<crate::column_water::ColumnWaterSetup>,
    /// Step the column water on its own thread (a server's choice; tests and
    /// tools keep the deterministic inline step).
    pub column_water_worker: bool,
    pub vegetation: Option<spall_ecology::living::LivingState>,
    pub weather_seed: u64,
}

#[cfg(test)]
mod creature_host_tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::creatures::{
        CreatureIntentWriter, CreatureRuleError, CreatureScheduleContext, CreatureScheduleSnapshot,
        CreatureSchedulingRules,
    };
    use spall_core::{CellSizeCode, GlobalCell, MaterialId};
    use spall_physics::{ArticulatedPose, PartTransform, Representation};
    use spall_voxel::{EditPlan, Volume};

    struct CountingRules {
        calls: AtomicUsize,
        contexts: Mutex<Vec<CreatureScheduleContext>>,
    }

    fn source_volume(id: spall_core::VolumeId) -> Volume {
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                MaterialId(1),
            ))
            .unwrap();
        volume
    }

    #[test]
    fn lifecycle_catalog_fence_hashes_mapping_entries_not_only_claimed_hash() {
        let fixture = crate::creatures::tests::FixtureCatalog::new();
        let original = fixture.view();
        let changed_mapping = [spall_protocol::CreatureMaterialMappingEntry {
            portable_key: "fur",
            material_id: MaterialId(2),
        }];
        let changed = spall_protocol::CreatureValidationCatalog {
            assets: original.assets,
            species: original.species,
            materials: original.materials,
            material_mapping_version: original.material_mapping_version,
            material_mapping_hash: original.material_mapping_hash,
            material_mapping: &changed_mapping,
        };
        assert_ne!(
            creature_catalog_fingerprint(&original),
            creature_catalog_fingerprint(&changed),
            "a stale claimed mapping hash must not hide changed catalog entries"
        );
    }

    #[test]
    fn dormant_mixed_target_rejects_without_waking_or_publishing() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let entity = simulation
            .world
            .spawn_body(
                crate::fixtures::solid_block(2),
                crate::body::BodyPose::new(glam::DQuat::IDENTITY, [0.0, 3.0, 0.0]),
                [0.0; 3],
                [0.0; 3],
                2600.0,
                0,
            )
            .unwrap();
        let body = simulation.world.body(entity).unwrap();
        let volume_id = body.volume_id;
        let intent = crate::intent::EditIntent::cut(
            RequestId(9),
            entity,
            EditTarget::Body(entity),
            SphereBrush::new(
                BrushPoint::from_units(BRUSH_UNIT / 2, BRUSH_UNIT / 2, BRUSH_UNIT / 2),
                BRUSH_UNIT,
            )
            .unwrap(),
        );
        let staged = crate::stage::stage_edit(&crate::stage::StageInput::new(
            &intent,
            volume_id,
            body.volume.clone(),
            spall_voxel::EvictedBricks::new(),
            simulation.world.anchor(),
            simulation.world.generation(),
            simulation.world.topology_epoch(),
        ))
        .unwrap();
        assert!(simulation.world.deactivate_body(entity));
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let source_id = simulation
            .world
            .registry()
            .clone()
            .allocate_volume()
            .unwrap();
        let state = catalog.state(1, source_id.get());
        let before = (
            simulation.world.world_hash(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.journal.cursor(),
            simulation.creatures.as_ref().unwrap().revision(),
        );
        let result = simulation.prepare_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(state),
                parts: vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source_volume(source_id),
                }],
                representation: Representation::MergedCuboids,
            }],
            Some(&staged),
            &catalog.view(),
        );
        assert!(
            matches!(result, Err(reason) if reason.contains("staged dormant-body reactivation"))
        );
        assert!(simulation.world.body_is_dormant(entity));
        assert_eq!(
            (
                simulation.world.world_hash(),
                simulation.world.registry().counters(),
                simulation.world.physics().body_count(),
                simulation.journal.cursor(),
                simulation.creatures.as_ref().unwrap().revision(),
            ),
            before
        );
    }

    #[test]
    fn ordered_single_part_birth_publishes_one_transaction_and_oversized_birth_rejects_atomically()
    {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let mut candidate_registry = simulation.world.registry().clone();
        let volume_id = candidate_registry.allocate_volume().unwrap();
        let state = catalog.state(1, volume_id.get());
        let input = || CreatureBirthPartInput {
            part_id: 1,
            parent_id: None,
            local_transform: PartTransform::IDENTITY,
            volume: source_volume(volume_id),
        };
        let counters = simulation.world.registry().counters();
        let cursor = simulation.journal.cursor();
        let bodies = simulation.world.physics().body_count();
        let committed = simulation
            .commit_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![AuthoritativeCreatureIntent::Birth {
                    state: Box::new(state.clone()),
                    parts: vec![input()],
                    representation: Representation::MergedCuboids,
                }],
                None,
                &catalog.view(),
            )
            .expect("one-part lifecycle record fits the existing inline frame");
        let inline_bytes = spall_protocol::encode_control(&committed.topology).unwrap();
        eprintln!(
            "one-part lifecycle control record: {} bytes",
            inline_bytes.len()
        );
        assert!(inline_bytes.len() <= spall_protocol::limits::MAX_CONTROL_RECORD);
        assert_eq!(committed.topology.ops.len(), 1);
        assert!(matches!(
            committed.topology.ops[0],
            spall_protocol::TopologyOp::CreatureLifecycle { .. }
        ));
        assert_eq!(simulation.journal.cursor(), cursor + 1);
        assert_eq!(simulation.journal.entries().len(), 1);
        assert_eq!(simulation.world.registry().counters().1, counters.1 + 1);
        assert_eq!(simulation.world.physics().body_count(), bodies + 1);
        assert!(
            simulation
                .world
                .creature_source_part(state.key, 1)
                .is_some()
        );

        let mut multi_simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        multi_simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let materials = multi_simulation.world.materials().clone();
        let multi_catalog = HierarchyCatalog {
            materials: materials.clone(),
            asset: [spall_protocol::CreatureAssetCatalogEntry {
                uuid: [8; 16],
                logical_hash: spall_protocol::Hash32([9; 32]),
                part_ids: &[1, 2, 3],
                clip_names: &["idle"],
                portable_material_keys: &["fur", "hide"],
            }],
            species: [spall_protocol::CreatureSpeciesCatalogEntry {
                key: "deer",
                version: 1,
                enabled: true,
                asset: spall_protocol::CreatureAssetIdentity {
                    uuid: [8; 16],
                    logical_hash: spall_protocol::Hash32([9; 32]),
                },
            }],
            mapping: [
                spall_protocol::CreatureMaterialMappingEntry {
                    portable_key: "fur",
                    material_id: MaterialId(1),
                },
                spall_protocol::CreatureMaterialMappingEntry {
                    portable_key: "hide",
                    material_id: MaterialId(2),
                },
            ],
        };
        let mut multi_registry = multi_simulation.world.registry().clone();
        let volumes = [
            multi_registry.allocate_volume().unwrap(),
            multi_registry.allocate_volume().unwrap(),
            multi_registry.allocate_volume().unwrap(),
        ];
        let multi_state = multi_catalog.state(volumes);
        let inputs = volumes
            .into_iter()
            .enumerate()
            .map(|(index, volume)| CreatureBirthPartInput {
                part_id: index as u32 + 1,
                parent_id: match index {
                    0 => None,
                    1 => Some(1),
                    _ => Some(2),
                },
                local_transform: PartTransform::IDENTITY,
                volume: crate::fixtures::noisy_source_volume(
                    volume,
                    if index == 1 {
                        MaterialId(2)
                    } else {
                        MaterialId(1)
                    },
                ),
            })
            .collect();
        let before = (
            multi_simulation.world.registry().counters(),
            multi_simulation.journal.cursor(),
            multi_simulation.world.physics().body_count(),
            multi_simulation.creatures.as_ref().unwrap().revision(),
        );
        let rejected = multi_simulation.prepare_creature_lifecycle_transaction(
            Tick(1),
            spall_protocol::ControlSeq(1),
            vec![AuthoritativeCreatureIntent::Birth {
                state: Box::new(multi_state.clone()),
                parts: inputs,
                representation: Representation::MergedCuboids,
            }],
            None,
            &multi_catalog.view(),
        );
        assert!(rejected.is_err(), "three-part source exceeds inline cap");
        assert_eq!(
            (
                multi_simulation.world.registry().counters(),
                multi_simulation.journal.cursor(),
                multi_simulation.world.physics().body_count(),
                multi_simulation.creatures.as_ref().unwrap().revision(),
            ),
            before,
            "oversized record rejection must not publish any candidate state"
        );
        assert!(
            multi_simulation
                .creatures
                .as_ref()
                .unwrap()
                .record(multi_state.key.creature_id)
                .is_none()
        );
    }

    #[test]
    fn ordered_birth_applied_pose_and_checked_death_use_one_journal_entry_each() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let mut candidate_registry = simulation.world.registry().clone();
        let volume_id = candidate_registry.allocate_volume().unwrap();
        let state = catalog.state(1, volume_id.get());
        let birth = simulation
            .commit_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![AuthoritativeCreatureIntent::Birth {
                    state: Box::new(state.clone()),
                    parts: vec![CreatureBirthPartInput {
                        part_id: 1,
                        parent_id: None,
                        local_transform: PartTransform::IDENTITY,
                        volume: source_volume(volume_id),
                    }],
                    representation: Representation::MergedCuboids,
                }],
                None,
                &catalog.view(),
            )
            .unwrap();
        assert_eq!(birth.journal_seq.0, 1);
        let owner = simulation.world.creature_physics_owner(state.key).unwrap();
        let moved = PartTransform {
            translation_m: [0.5, 0.0, 0.0],
            ..PartTransform::IDENTITY
        };
        simulation
            .world
            .physics_mut()
            .update_articulated_pose(
                owner,
                ArticulatedPose {
                    topology_token: state.aggregate_topology_revision,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(1, moved)],
                },
            )
            .unwrap();
        simulation.world.step_physics();

        let before_stale_death = (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.creatures.as_ref().unwrap().revision(),
        );
        assert!(
            simulation
                .commit_creature_lifecycle_transaction(
                    Tick(2),
                    spall_protocol::ControlSeq(2),
                    vec![AuthoritativeCreatureIntent::Death {
                        key: state.key,
                        expected_state_revision: state.state_revision,
                        expected_topology_token: state.aggregate_topology_revision,
                        expected_pose_token: 0,
                    }],
                    None,
                    &catalog.view(),
                )
                .is_err()
        );
        assert_eq!(
            (
                simulation.journal.cursor(),
                simulation.world.registry().counters(),
                simulation.world.physics().body_count(),
                simulation.creatures.as_ref().unwrap().revision(),
            ),
            before_stale_death
        );

        let measured = simulation
            .world
            .physics()
            .articulated_part_motion_snapshot(owner)
            .unwrap();
        let death = simulation
            .commit_creature_lifecycle_transaction(
                Tick(2),
                spall_protocol::ControlSeq(3),
                vec![AuthoritativeCreatureIntent::Death {
                    key: state.key,
                    expected_state_revision: state.state_revision,
                    expected_topology_token: state.aggregate_topology_revision,
                    expected_pose_token: 1,
                }],
                None,
                &catalog.view(),
            )
            .unwrap();
        assert_eq!(death.journal_seq.0, 2);
        let spall_protocol::TopologyOp::CreatureLifecycle { commit } = &death.topology.ops[0]
        else {
            panic!("typed death missing");
        };
        let overlay = commit.pre_motion.as_ref().unwrap();
        assert_eq!(overlay.part_velocities.len(), measured.len());
        for (serialized, physical) in overlay.part_velocities.iter().zip(&measured) {
            assert_eq!(serialized.part_id, physical.part_id);
            assert_eq!(serialized.linear_velocity_m_s, physical.linear_velocity_m_s);
            assert_eq!(
                serialized.angular_velocity_rad_s,
                physical.angular_velocity_rad_s
            );
        }
        let spall_protocol::CreatureLifecycleOperationV4::Death { after } =
            &commit.change.operation
        else {
            panic!("wrong lifecycle");
        };
        assert!(after.part_velocities.is_empty());
        assert_eq!(simulation.journal.len(), 2);
        assert_eq!(death.witnesses.len(), 1);
        assert!(matches!(
            death.topology.ops[0],
            spall_protocol::TopologyOp::CreatureLifecycle { .. }
        ));
        assert_eq!(
            simulation.world.creature_source_owner(volume_id),
            Some((state.key, 1))
        );
        assert_eq!(
            simulation.world.creature_frozen_poses(state.key).unwrap(),
            &[(1, moved)]
        );
        let physics = simulation
            .world
            .physics()
            .articulated_owner_snapshot(owner)
            .unwrap();
        let witness = &death.witnesses[0];
        let installed_root = simulation
            .world
            .physics_origin()
            .to_world_f64(physics.root.translation_m);
        for (actual, expected) in witness.root_translation_m.iter().zip(installed_root) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
        assert_eq!(
            witness.installed_linear_velocity_m_s,
            physics.root.linvel_m_s
        );
        assert_eq!(
            witness.installed_angular_velocity_rad_s,
            physics.root.angvel_rad_s
        );
        let corpse = simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .unwrap();
        assert_eq!(
            corpse.lifecycle,
            spall_protocol::CreatureLifecycleState::Corpse
        );
        assert_eq!(corpse.state_revision, state.state_revision + 1);
    }

    #[test]
    fn two_part_birth_refuses_without_mutation_until_typed_bulk_publication_exists() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let mut candidate_registry = simulation.world.registry().clone();
        let volume_id = candidate_registry.allocate_volume().unwrap();
        let child_volume_id = candidate_registry.allocate_volume().unwrap();
        let mut state = catalog.state(1, volume_id.get());
        state
            .source_bindings
            .push(spall_protocol::CreaturePartBinding {
                part_id: 2,
                source_volume_id: child_volume_id,
                source_revision: 1,
            });
        let mut view = catalog.view();
        let assets = [spall_protocol::CreatureAssetCatalogEntry {
            uuid: view.assets[0].uuid,
            logical_hash: view.assets[0].logical_hash,
            part_ids: &[1, 2],
            clip_names: view.assets[0].clip_names,
            portable_material_keys: view.assets[0].portable_material_keys,
        }];
        view.assets = &assets;
        let before = (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.creatures.as_ref().unwrap().revision(),
        );
        let birth = simulation
            .commit_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![AuthoritativeCreatureIntent::Birth {
                    state: Box::new(state.clone()),
                    parts: vec![
                        CreatureBirthPartInput {
                            part_id: 1,
                            parent_id: None,
                            local_transform: PartTransform::IDENTITY,
                            volume: crate::fixtures::noisy_source_volume(volume_id, MaterialId(1)),
                        },
                        CreatureBirthPartInput {
                            part_id: 2,
                            parent_id: Some(1),
                            local_transform: PartTransform {
                                translation_m: [2.0, 0.0, 0.0],
                                ..PartTransform::IDENTITY
                            },
                            volume: source_volume(child_volume_id),
                        },
                    ],
                    representation: Representation::MergedCuboids,
                }],
                None,
                &view,
            )
            .err()
            .expect("two-part inline birth must refuse");
        assert!(birth.contains("inline control-record ceiling"), "{birth}");
        assert_eq!(
            before,
            (
                simulation.journal.cursor(),
                simulation.world.registry().counters(),
                simulation.world.physics().body_count(),
                simulation.creatures.as_ref().unwrap().revision()
            )
        );
    }

    #[test]
    fn mixed_staged_geometry_and_birth_publish_atomically_under_one_transaction() {
        use crate::stage::StageInput;
        use spall_jobs::{Generation, TopologyEpoch};
        use spall_structure::AnchorPlane;
        use spall_voxel::EvictedBricks;

        let setup = crate::fixtures::bridged_terrain_setup();
        let mut simulation = Simulation::new(SimulationConfig::new(setup)).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let terrain_id = simulation.world.terrain_volume_id();
        let terrain = simulation.world.volume_ref(terrain_id).unwrap().clone();
        let half = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                10 * BRUSH_UNIT + half,
                4 * BRUSH_UNIT + half,
                BRUSH_UNIT + half,
            ),
            2 * BRUSH_UNIT,
        )
        .unwrap();
        let intent = crate::intent::EditIntent::cut(
            RequestId(1),
            EntityId::new(1).unwrap(),
            crate::intent::EditTarget::Terrain,
            brush,
        );
        let staged = crate::stage::stage_edit(&StageInput::new(
            &intent,
            terrain_id,
            terrain,
            EvictedBricks::new(),
            AnchorPlane::at(0),
            Generation::START,
            TopologyEpoch::START,
        ))
        .unwrap();
        assert!(staged.splits());
        let geometry = match crate::commit::prepare_geometry(
            &simulation.world,
            &simulation.journal,
            &staged,
            Tick(1),
            spall_protocol::ControlSeq(1),
        )
        .unwrap()
        {
            crate::commit::PrepareGeometryOutcome::Ready(prepared) => prepared,
            crate::commit::PrepareGeometryOutcome::Stale(_) => {
                panic!("fresh mixed geometry fixture is stale")
            }
        };
        let mut candidate_registry = geometry.registry.clone();
        let source_id = candidate_registry.allocate_volume().unwrap();
        let state = catalog.state(1, source_id.get());
        let invalid = AuthoritativeCreatureIntent::Birth {
            state: Box::new(state.clone()),
            parts: vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform {
                    rotation_xyzw: [0.0; 4],
                    ..PartTransform::IDENTITY
                },
                volume: source_volume(source_id),
            }],
            representation: Representation::MergedCuboids,
        };
        let before = (
            simulation.world.volume_hash(terrain_id).unwrap(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.journal.cursor(),
            simulation.creatures.as_ref().unwrap().revision(),
        );
        assert!(
            simulation
                .prepare_creature_lifecycle_transaction(
                    Tick(1),
                    spall_protocol::ControlSeq(1),
                    vec![invalid],
                    Some(&staged),
                    &catalog.view(),
                )
                .is_err()
        );
        assert_eq!(
            (
                simulation.world.volume_hash(terrain_id).unwrap(),
                simulation.world.registry().counters(),
                simulation.world.physics().body_count(),
                simulation.journal.cursor(),
                simulation.creatures.as_ref().unwrap().revision(),
            ),
            before,
            "late actor rejection must leave staged geometry untouched"
        );

        let committed = simulation
            .commit_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![AuthoritativeCreatureIntent::Birth {
                    state: Box::new(state.clone()),
                    parts: vec![CreatureBirthPartInput {
                        part_id: 1,
                        parent_id: None,
                        local_transform: PartTransform::IDENTITY,
                        volume: source_volume(source_id),
                    }],
                    representation: Representation::MergedCuboids,
                }],
                Some(&staged),
                &catalog.view(),
            )
            .unwrap();
        assert_eq!(committed.transaction_id.get(), 1);
        assert_eq!(committed.journal_seq.0, 1);
        assert_eq!(simulation.journal.len(), 1);
        assert_eq!(
            simulation.journal.entries()[0].transaction,
            committed.topology
        );
        assert!(committed.topology.ops.len() > 1);
        assert!(matches!(
            committed.topology.ops.last().unwrap(),
            spall_protocol::TopologyOp::CreatureLifecycle { .. }
        ));
        assert_eq!(
            simulation.world.creature_source_owner(source_id),
            Some((state.key, 1))
        );
        assert_eq!(
            simulation.world.registry().counters().1,
            candidate_registry.counters().1
        );
        assert_ne!(simulation.world.volume_hash(terrain_id).unwrap(), before.0);
    }

    fn source_volume_material(id: spall_core::VolumeId, material: MaterialId) -> Volume {
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(0, 0, 0),
                material,
            ))
            .unwrap();
        volume
    }

    struct HierarchyCatalog {
        materials: spall_core::MaterialManifest,
        asset: [spall_protocol::CreatureAssetCatalogEntry<'static>; 1],
        species: [spall_protocol::CreatureSpeciesCatalogEntry<'static>; 1],
        mapping: [spall_protocol::CreatureMaterialMappingEntry<'static>; 2],
    }

    impl HierarchyCatalog {
        fn view(&self) -> spall_protocol::CreatureValidationCatalog<'_> {
            spall_protocol::CreatureValidationCatalog {
                assets: &self.asset,
                species: &self.species,
                materials: &self.materials,
                material_mapping_version: 1,
                material_mapping_hash: spall_protocol::creature_material_mapping_hash(
                    &self.mapping,
                )
                .unwrap(),
                material_mapping: &self.mapping,
            }
        }

        fn state(&self, volumes: [spall_core::VolumeId; 3]) -> spall_protocol::CreatureStateV1 {
            let mapping_hash =
                spall_protocol::creature_material_mapping_hash(&self.mapping).unwrap();
            spall_protocol::CreatureStateV1 {
                schema: spall_protocol::CREATURE_STATE_SCHEMA,
                key: spall_core::CreatureKey::new(
                    WorldId::from_u128(10),
                    spall_core::WorldEpoch::INITIAL,
                    spall_core::CreatureId::new(1).unwrap(),
                    spall_core::CreatureGeneration::INITIAL,
                )
                .unwrap(),
                state_revision: 1,
                lifecycle: spall_protocol::CreatureLifecycleState::Alive,
                species_key: "deer".into(),
                species_version: 1,
                asset: spall_protocol::CreatureAssetIdentity {
                    uuid: [8; 16],
                    logical_hash: spall_protocol::Hash32([9; 32]),
                },
                material_manifest_hash: spall_protocol::content_manifest_hash(&self.materials),
                material_mapping_version: 1,
                material_mapping_hash: mapping_hash,
                clip_name: "idle".into(),
                clip_mode: spall_protocol::CreatureClipMode::Playing,
                clip_phase_tick: 0,
                clip_subtick_remainder: 0,
                clip_epoch: 1,
                source_bindings: volumes
                    .into_iter()
                    .enumerate()
                    .map(
                        |(index, source_volume_id)| spall_protocol::CreaturePartBinding {
                            part_id: index as u32 + 1,
                            source_volume_id,
                            source_revision: 1,
                        },
                    )
                    .collect(),
                aggregate_topology_revision: 2,
                pose_epoch: 1,
                root_translation_m: [10.0, 5.0, 10.0],
                root_rotation_xyzw: [
                    0.0,
                    0.0,
                    std::f32::consts::FRAC_1_SQRT_2,
                    std::f32::consts::FRAC_1_SQRT_2,
                ],
                linear_velocity_m_s: [0.0; 3],
                angular_velocity_rad_s: [0.0; 3],
                locomotion_tag: 0,
            }
        }
    }

    #[test]
    fn test_only_creature_birth_publishes_source_physics_and_ids_atomically() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let allocated_volume = simulation
            .world
            .registry()
            .clone()
            .allocate_volume()
            .unwrap();
        let source = source_volume(allocated_volume);
        let state = catalog.state(1, allocated_volume.get());

        let counters_before = simulation.world.registry().counters();
        let host_before = simulation.creature_allocator_snapshot().unwrap();
        let physics_before = simulation.world.physics().body_count();
        let foreign_volume = spall_core::VolumeId::new(999).unwrap();
        assert!(
            simulation
                .commit_creature_birth_for_test(
                    catalog.state(1, foreign_volume.get()),
                    &catalog.view(),
                    vec![CreatureBirthPartInput {
                        part_id: 1,
                        parent_id: None,
                        local_transform: PartTransform::IDENTITY,
                        volume: source_volume(foreign_volume),
                    }],
                    Representation::MergedCuboids,
                )
                .is_err()
        );
        let invalid = simulation.commit_creature_birth_for_test(
            state.clone(),
            &catalog.view(),
            vec![CreatureBirthPartInput {
                part_id: 1,
                parent_id: None,
                local_transform: PartTransform {
                    rotation_xyzw: [0.0; 4],
                    ..PartTransform::IDENTITY
                },
                volume: source.clone(),
            }],
            Representation::MergedCuboids,
        );
        assert!(invalid.is_err());
        assert_eq!(simulation.world.registry().counters(), counters_before);
        assert_eq!(
            simulation.creature_allocator_snapshot().unwrap(),
            host_before
        );
        assert_eq!(simulation.world.physics().body_count(), physics_before);
        assert!(
            simulation
                .creatures
                .as_ref()
                .unwrap()
                .record(state.key.creature_id)
                .is_none()
        );

        let owner = simulation
            .commit_creature_birth_for_test(
                state.clone(),
                &catalog.view(),
                vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source,
                }],
                Representation::MergedCuboids,
            )
            .unwrap();
        assert_eq!(
            simulation.world.registry().counters().1,
            counters_before.1 + 1
        );
        assert_eq!(
            simulation
                .creature_allocator_snapshot()
                .unwrap()
                .next_creature_id,
            2
        );
        assert_eq!(
            simulation.world.creature_physics_owner(state.key),
            Some(owner)
        );
        assert_eq!(
            simulation.world.creature_source_owner(allocated_volume),
            Some((state.key, 1))
        );
        assert_eq!(
            simulation.world.physics().articulated_collider_count(owner),
            1
        );
        assert!(simulation.world.volume_ref(allocated_volume).is_none());
        assert!(simulation.world.body_by_volume(allocated_volume).is_none());

        let moved_part_pose = PartTransform {
            translation_m: [0.75, 0.0, 0.0],
            ..PartTransform::IDENTITY
        };
        simulation
            .world
            .physics_mut()
            .update_articulated_pose(
                owner,
                ArticulatedPose {
                    topology_token: state.aggregate_topology_revision,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: vec![(1, moved_part_pose)],
                },
            )
            .unwrap();

        let source_hash = simulation.world.creature_source_hash_for_test();
        let body_count = simulation.world.physics().body_count();
        assert!(
            simulation
                .commit_creature_death_for_test(
                    state.key,
                    state.state_revision,
                    state.aggregate_topology_revision,
                    99,
                    &catalog.view(),
                )
                .is_err()
        );
        assert_eq!(simulation.world.physics().body_count(), body_count);
        assert_eq!(
            simulation.world.creature_source_hash_for_test(),
            source_hash
        );
        assert_eq!(
            simulation
                .creatures
                .as_ref()
                .unwrap()
                .record(state.key.creature_id)
                .unwrap()
                .lifecycle,
            spall_protocol::CreatureLifecycleState::Alive
        );
        assert!(
            simulation
                .commit_creature_death_for_test(
                    state.key,
                    state.state_revision,
                    state.aggregate_topology_revision,
                    1,
                    &catalog.view(),
                )
                .is_err()
        );
        simulation.world.step_physics();

        let transfer = simulation
            .commit_creature_death_for_test(
                state.key,
                state.state_revision,
                state.aggregate_topology_revision,
                1,
                &catalog.view(),
            )
            .unwrap();
        assert!(transfer.mass_kg > 0.0);
        assert_eq!(
            simulation.world.creature_source_hash_for_test(),
            source_hash
        );
        let corpse = simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .unwrap();
        assert_eq!(
            corpse.lifecycle,
            spall_protocol::CreatureLifecycleState::Corpse
        );
        assert_eq!(corpse.clip_mode, spall_protocol::CreatureClipMode::Frozen);
        assert_eq!(corpse.state_revision, state.state_revision + 1);
        assert_eq!(
            simulation.world.creature_frozen_poses(state.key).unwrap(),
            &[(1, moved_part_pose)]
        );
        assert!(
            simulation
                .world
                .physics()
                .articulated_owner_snapshot(owner)
                .unwrap()
                .root
                .mass_kg
                > 0.0
        );
    }

    #[test]
    fn death_rejects_replaced_source_content_with_same_max_revision() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let volume_id = simulation
            .world
            .registry()
            .clone()
            .allocate_volume()
            .unwrap();
        let sparse_brick = |solid_local_cell: usize, revision: u64| {
            let mut cells = vec![MaterialId::AIR; 32 * 32 * 32];
            cells[solid_local_cell] = crate::fixtures::STONE;
            spall_voxel::Brick::restored(&cells, spall_core::Revision(revision), true)
        };
        let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
        source
            .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse_brick(0, 1))
            .unwrap();
        source
            .insert_brick(spall_core::BrickCoord::new(1, 0, 0), sparse_brick(0, 5))
            .unwrap();
        let mut state = catalog.state(1, volume_id.get());
        state.source_bindings[0].source_revision = 5;
        let owner = simulation
            .commit_creature_birth_for_test(
                state.clone(),
                &catalog.view(),
                vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source,
                }],
                Representation::MergedCuboids,
            )
            .unwrap();
        let captured_stamp = simulation
            .world
            .creature_source_metadata(state.key, 1)
            .unwrap()
            .state_stamp;
        let max_revision = |volume: &Volume| {
            volume
                .resident_brick_coords()
                .into_iter()
                .filter_map(|coord| volume.brick_revision(coord).ok().flatten())
                .map(|revision| revision.0)
                .max()
                .unwrap()
        };
        let before_max_revision =
            max_revision(simulation.world.creature_source_part(state.key, 1).unwrap());
        assert_eq!(before_max_revision, 5);
        let mut changed_source = Volume::new(volume_id, CellSizeCode::Quarter);
        changed_source
            .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse_brick(1, 1))
            .unwrap();
        changed_source
            .insert_brick(spall_core::BrickCoord::new(1, 0, 0), sparse_brick(0, 5))
            .unwrap();
        simulation
            .world
            .replace_creature_source(state.key, 1, changed_source)
            .unwrap();
        assert_ne!(
            simulation
                .world
                .creature_source_part(state.key, 1)
                .unwrap()
                .state_stamp(),
            captured_stamp
        );
        assert_eq!(
            max_revision(simulation.world.creature_source_part(state.key, 1).unwrap()),
            before_max_revision,
            "replacement changes low-revision content while the maximum stays 5"
        );
        let before_hash = simulation.world.creature_source_hash_for_test();
        let before_counters = simulation.world.registry().counters();
        let before_allocator = simulation.creature_allocator_snapshot().unwrap();
        let before_body_count = simulation.world.physics().body_count();
        assert!(
            simulation
                .commit_creature_death_for_test(
                    state.key,
                    state.state_revision,
                    state.aggregate_topology_revision,
                    0,
                    &catalog.view(),
                )
                .is_err()
        );
        assert_eq!(
            simulation.world.creature_source_hash_for_test(),
            before_hash
        );
        assert_eq!(simulation.world.registry().counters(), before_counters);
        assert_eq!(
            simulation.creature_allocator_snapshot().unwrap(),
            before_allocator
        );
        assert_eq!(simulation.world.physics().body_count(), before_body_count);
        assert_eq!(
            simulation.world.creature_physics_owner(state.key),
            Some(owner)
        );
        assert_eq!(
            simulation
                .creatures
                .as_ref()
                .unwrap()
                .record(state.key.creature_id),
            Some(&state)
        );
    }

    #[test]
    fn prepared_death_rejects_source_replacement_before_publication() {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::with_materials(
            simulation.world.materials().clone(),
        );
        let volume_id = simulation
            .world
            .registry()
            .clone()
            .allocate_volume()
            .unwrap();
        let sparse = |cell: usize, revision: u64| {
            let mut cells = vec![MaterialId::AIR; 32 * 32 * 32];
            cells[cell] = crate::fixtures::STONE;
            spall_voxel::Brick::restored(&cells, spall_core::Revision(revision), true)
        };
        let mut source = Volume::new(volume_id, CellSizeCode::Quarter);
        source
            .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse(0, 1))
            .unwrap();
        source
            .insert_brick(
                spall_core::BrickCoord::new(1, 0, 0),
                spall_voxel::Brick::uniform(MaterialId::AIR, spall_core::Revision(5)),
            )
            .unwrap();
        let mut state = catalog.state(1, volume_id.get());
        state.source_bindings[0].source_revision = 5;
        simulation
            .commit_creature_birth_for_test(
                state.clone(),
                &catalog.view(),
                vec![CreatureBirthPartInput {
                    part_id: 1,
                    parent_id: None,
                    local_transform: PartTransform::IDENTITY,
                    volume: source,
                }],
                Representation::MergedCuboids,
            )
            .unwrap();
        let owner = simulation.world.creature_physics_owner(state.key).unwrap();
        let prepared = simulation
            .prepare_creature_lifecycle_transaction(
                Tick(1),
                spall_protocol::ControlSeq(1),
                vec![AuthoritativeCreatureIntent::Death {
                    key: state.key,
                    expected_state_revision: state.state_revision,
                    expected_topology_token: state.aggregate_topology_revision,
                    expected_pose_token: 0,
                }],
                None,
                &catalog.view(),
            )
            .unwrap();

        let mut replacement = Volume::new(volume_id, CellSizeCode::Quarter);
        replacement
            .insert_brick(spall_core::BrickCoord::new(0, 0, 0), sparse(1, 1))
            .unwrap();
        replacement
            .insert_brick(
                spall_core::BrickCoord::new(1, 0, 0),
                spall_voxel::Brick::uniform(MaterialId::AIR, spall_core::Revision(5)),
            )
            .unwrap();
        simulation
            .world
            .replace_creature_source(state.key, 1, replacement)
            .unwrap();
        let before = (
            simulation.journal.cursor(),
            simulation.world.registry().counters(),
            simulation.world.physics().body_count(),
            simulation.world.creature_source_hash_for_test(),
            simulation.creatures.as_ref().unwrap().revision(),
        );
        assert!(
            simulation
                .validate_prepared_creature_lifecycle(prepared, &catalog.view())
                .is_err()
        );
        assert_eq!(
            (
                simulation.journal.cursor(),
                simulation.world.registry().counters(),
                simulation.world.physics().body_count(),
                simulation.world.creature_source_hash_for_test(),
                simulation.creatures.as_ref().unwrap().revision(),
            ),
            before
        );
        assert_eq!(
            simulation.world.creature_physics_owner(state.key),
            Some(owner)
        );
    }

    #[test]
    fn nested_mixed_material_sources_keep_hierarchy_queries_and_mass_on_transfer() {
        use spall_physics::{ArticulatedPose, PartTransform, Representation};
        use spall_protocol::{
            CreatureAssetCatalogEntry, CreatureAssetIdentity, CreatureMaterialMappingEntry,
            CreatureSpeciesCatalogEntry, content_manifest_hash,
        };

        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let materials = simulation.world.materials().clone();
        let catalog = HierarchyCatalog {
            materials: materials.clone(),
            asset: [CreatureAssetCatalogEntry {
                uuid: [8; 16],
                logical_hash: spall_protocol::Hash32([9; 32]),
                part_ids: &[1, 2, 3],
                clip_names: &["idle"],
                portable_material_keys: &["fur", "hide"],
            }],
            species: [CreatureSpeciesCatalogEntry {
                key: "deer",
                version: 1,
                enabled: true,
                asset: CreatureAssetIdentity {
                    uuid: [8; 16],
                    logical_hash: spall_protocol::Hash32([9; 32]),
                },
            }],
            mapping: [
                CreatureMaterialMappingEntry {
                    portable_key: "fur",
                    material_id: MaterialId(1),
                },
                CreatureMaterialMappingEntry {
                    portable_key: "hide",
                    material_id: MaterialId(2),
                },
            ],
        };
        assert_eq!(
            content_manifest_hash(&materials),
            content_manifest_hash(simulation.world.materials())
        );
        let mut candidate_registry = simulation.world.registry().clone();
        let volumes = [
            candidate_registry.allocate_volume().unwrap(),
            candidate_registry.allocate_volume().unwrap(),
            candidate_registry.allocate_volume().unwrap(),
        ];
        let state = catalog.state(volumes);
        let q90 = [
            0.0,
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        ];
        let authored = [
            (None, PartTransform::IDENTITY),
            (
                Some(1),
                PartTransform {
                    translation_m: [1.0, 0.0, 0.0],
                    rotation_xyzw: q90,
                },
            ),
            (
                Some(2),
                PartTransform {
                    translation_m: [1.0, 0.0, 0.0],
                    rotation_xyzw: q90,
                },
            ),
        ];
        let sources = [
            source_volume_material(volumes[0], MaterialId(1)),
            source_volume_material(volumes[1], MaterialId(2)),
            source_volume_material(volumes[2], MaterialId(1)),
        ];
        let owner = simulation
            .commit_creature_birth_for_test(
                state.clone(),
                &catalog.view(),
                (0..3)
                    .map(|index| CreatureBirthPartInput {
                        part_id: index as u32 + 1,
                        parent_id: authored[index].0,
                        local_transform: authored[index].1,
                        volume: sources[index].clone(),
                    })
                    .collect(),
                Representation::MergedCuboids,
            )
            .unwrap();
        assert_eq!(
            simulation.world.registry().counters().1,
            candidate_registry.counters().1
        );

        let moving = [
            (1, PartTransform::IDENTITY),
            (
                2,
                PartTransform {
                    translation_m: [1.25, 0.0, 0.0],
                    rotation_xyzw: q90,
                },
            ),
            (
                3,
                PartTransform {
                    translation_m: [0.75, 0.5, 0.0],
                    rotation_xyzw: q90,
                },
            ),
        ];
        simulation
            .world
            .physics_mut()
            .update_articulated_pose(
                owner,
                ArticulatedPose {
                    topology_token: state.aggregate_topology_revision,
                    base_pose_token: 0,
                    pose_token: 1,
                    parts: moving.to_vec(),
                },
            )
            .unwrap();
        simulation.world.step_physics();

        let root_q = glam::Quat::from_xyzw(q90[0], q90[1], q90[2], q90[3]);
        let root_t = glam::Vec3::new(10.0, 5.0, 10.0);
        let compose = |parent: (glam::Vec3, glam::Quat), local: PartTransform| {
            let q = glam::Quat::from_xyzw(
                local.rotation_xyzw[0],
                local.rotation_xyzw[1],
                local.rotation_xyzw[2],
                local.rotation_xyzw[3],
            );
            (
                parent.0 + parent.1 * glam::Vec3::from_array(local.translation_m),
                parent.1 * q,
            )
        };
        let part1 = (root_t, root_q);
        let part2 = compose(part1, moving[1].1);
        let part3 = compose(part2, moving[2].1);
        let rays: Vec<_> = [part1, part2, part3]
            .into_iter()
            .map(|(translation, rotation)| {
                let center = translation + rotation * glam::Vec3::splat(0.125);
                let origin = center - glam::Vec3::Z;
                (
                    [origin.x, origin.y, origin.z],
                    [center.x, center.y, center.z],
                )
            })
            .collect();
        let mut baseline = Vec::new();
        for (origin, _) in &rays {
            let hit = simulation
                .world
                .physics_mut()
                .raycast_articulated(owner, *origin, [0.0, 0.0, 1.0], 2.0)
                .unwrap();
            baseline.push((hit.part_id, hit.volume_id, hit.cell));
        }
        assert_eq!(
            baseline.iter().map(|hit| hit.0).collect::<Vec<_>>(),
            [1, 2, 3]
        );

        let source_hash = simulation.world.creature_source_hash_for_test();
        let transfer = simulation
            .commit_creature_death_for_test(
                state.key,
                state.state_revision,
                state.aggregate_topology_revision,
                1,
                &catalog.view(),
            )
            .unwrap();
        let cell_volume = 0.25_f64.powi(3);
        let expected_mass = cell_volume
            * (simulation.world.density(MaterialId(1)) * 2.0
                + simulation.world.density(MaterialId(2)));
        assert!((transfer.mass_kg - expected_mass).abs() < 1.0e-5);
        assert_eq!(
            simulation.world.creature_source_hash_for_test(),
            source_hash
        );
        let hierarchy = simulation.world.creature_hierarchy(state.key).unwrap();
        assert_eq!(hierarchy.len(), 3);
        for (index, part) in hierarchy.iter().enumerate() {
            assert_eq!(part.part_id, index as u32 + 1);
            assert_eq!(part.parent_id, authored[index].0);
            assert_eq!(
                part.initial_local_transform.translation_m,
                authored[index].1.translation_m
            );
            assert_eq!(
                part.frozen_local_transform.translation_m,
                moving[index].1.translation_m
            );
            for (actual, expected) in part
                .initial_local_transform
                .rotation_xyzw
                .iter()
                .zip(authored[index].1.rotation_xyzw)
                .chain(
                    part.frozen_local_transform
                        .rotation_xyzw
                        .iter()
                        .zip(moving[index].1.rotation_xyzw),
                )
            {
                assert!((actual - expected).abs() < 1.0e-6);
            }
            assert_eq!(
                simulation.world.creature_source_owner(volumes[index]),
                Some((state.key, index as u32 + 1))
            );
            assert_eq!(
                simulation
                    .world
                    .creature_source_part(state.key, index as u32 + 1)
                    .unwrap()
                    .id(),
                volumes[index]
            );
        }
        for ((origin, _), expected) in rays.iter().zip(baseline) {
            let hit = simulation
                .world
                .physics_mut()
                .raycast_articulated(owner, *origin, [0.0, 0.0, 1.0], 2.0)
                .unwrap();
            assert_eq!((hit.part_id, hit.volume_id, hit.cell), expected);
        }
        let corpse = simulation
            .creatures
            .as_ref()
            .unwrap()
            .record(state.key.creature_id)
            .unwrap();
        assert_eq!(
            corpse.material_manifest_hash,
            content_manifest_hash(simulation.world.materials())
        );
        assert_eq!(
            spall_protocol::creature_material_mapping_hash(&catalog.mapping).unwrap(),
            corpse.material_mapping_hash
        );
        assert_eq!(
            simulation.world.physics().articulated_collider_count(owner),
            3
        );
    }

    impl CreatureSchedulingRules for CountingRules {
        fn propose(
            &self,
            context: CreatureScheduleContext,
            _snapshots: &[CreatureScheduleSnapshot],
            _output: &mut CreatureIntentWriter,
        ) -> Result<(), CreatureRuleError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.contexts.lock().unwrap().push(context);
            Ok(())
        }
    }

    #[test]
    fn simulation_host_starts_empty_and_world_replacement_advances_epoch_without_resetting_counter()
    {
        let mut simulation =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation
            .initialize_creature_host(WorldId::from_u128(10))
            .unwrap();
        let catalog = crate::creatures::tests::FixtureCatalog::new();
        simulation
            .creatures
            .as_mut()
            .unwrap()
            .register_validated(catalog.state(1, 1001), &catalog.view())
            .unwrap();
        let before = simulation.creature_allocator_snapshot().unwrap();
        assert_eq!(before.world_epoch, 1);
        assert_eq!(before.next_creature_id, 2);

        let rules = CountingRules {
            calls: AtomicUsize::new(0),
            contexts: Mutex::new(Vec::new()),
        };
        let report = simulation.tick_with_creature_rules(&rules).unwrap();
        let schedule = report.creature_schedule.unwrap();
        assert_eq!(schedule.snapshot_count, 1);
        assert!(schedule.accepted_intents.is_empty());
        assert_eq!(rules.calls.load(Ordering::SeqCst), 1);
        let context = rules.contexts.lock().unwrap()[0];
        assert_eq!(context.tick, 1);
        assert!(context.terrain_revision > 0);
        assert_eq!(context.water_frame_revision, None);

        let fresh =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        simulation.replace_world(fresh).unwrap();
        let after = simulation.creature_allocator_snapshot().unwrap();
        assert_eq!(after.world_epoch, 2);
        assert_eq!(after.next_creature_id, before.next_creature_id);
        let report = simulation.tick_with_creature_rules(&rules).unwrap();
        assert_eq!(report.creature_schedule.unwrap().snapshot_count, 0);
        assert_eq!(rules.calls.load(Ordering::SeqCst), 1);
    }
}

impl SimulationConfig {
    /// Default accepted-but-unstaged intent backlog.
    pub const DEFAULT_MAX_PENDING_INTENTS: usize = 256;
    /// Default consecutive-conflict count before a region is serialized.
    pub const DEFAULT_SERIALIZE_THRESHOLD: u32 = 3;

    pub fn new(world: WorldSetup) -> Self {
        Self {
            world,
            terrain_collider_mode: TerrainColliderMode::PerBrick,
            max_pending_intents: Self::DEFAULT_MAX_PENDING_INTENTS,
            serialize_threshold: Self::DEFAULT_SERIALIZE_THRESHOLD,
            column_water: None,
            column_water_worker: false,
            vegetation: None,
            weather_seed: 0,
        }
    }
}

/// Error advancing the simulation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TickError {
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error("tick counter exhausted")]
    TickExhausted,
    #[error("authoritative water tick failed: {0}")]
    Water(String),
    #[error("authoritative snow tick failed: {0}")]
    Snow(String),
}

/// What one [`Simulation::apply_contact_damage`] pass did: the pure
/// [`ContactDamagePlan`] plus how its cuts fared at admission.
#[derive(Debug, Clone, Default)]
pub struct ContactDamageReport {
    /// The policy's decision for this tick.
    pub plan: ContactDamagePlan,
    /// Planned cuts accepted into the edit pipeline.
    pub submitted: usize,
    /// Planned cuts refused admission (pipeline queue full) — bounded
    /// backpressure, not an error.
    pub rejected: usize,
}

/// The authoritative simulation.
pub struct Simulation {
    pub(crate) world: SimWorld,
    pipeline: EditPipeline,
    journal: JournalSink,
    pub(crate) tick: Tick,
    next_control_seq: u64,
    next_damage_seq: u64,
    column_water: Option<crate::column_water::AuthoritativeColumnWater>,
    /// Whether the column water steps on its own thread.
    column_water_worker: bool,
    /// Sample points of each body for buoyancy, rebuilt when a body changes.
    buoyancy_cache: crate::world::BuoyancyCache,
    pub(crate) vegetation: Option<crate::vegetation::AuthoritativeVegetation>,
    pub(crate) weather: spall_protocol::WeatherState,
    pub(crate) weather_dirty: bool,
    weather_seed: u64,
    pub(crate) snow: crate::snow::SnowState,
    snow_priority_tiles: Vec<(i32, i32)>,
    snow_priority_players: Vec<(i32, i32)>,
    snow_priority_refreshed_tick: u64,
    /// Per-player stride credit; proposals are validated by the snow owner.
    motion_footprints: crate::footprints::MotionFootprints,
    footfall_candidates: Vec<crate::footprints::FootfallCandidate>,
    /// The world a reset replaced, being freed on another thread (see [`Retiring`]).
    retiring: Retiring,
    /// Empty unless the owning application explicitly installs an actor host.
    /// No public actor-registration or startup-spawn path exists in this slice.
    creatures: Option<crate::creatures::CreatureHost>,
}

/// Frees a large value on a background thread. Dropping a full-size world (hundreds of thousands
/// of bricks and colliders) took about nine seconds on the tick thread during a world reset, with
/// every client waiting. A panic while dropping is not lost: it is re-raised on the owning thread
/// the next time the value is retired or the handle is finished.
#[derive(Default)]
struct Retiring(Option<std::thread::JoinHandle<()>>);

impl Retiring {
    /// Waits for the previous retirement, re-raising its panic, if any.
    fn finish(&mut self) {
        if let Some(handle) = self.0.take()
            && let Err(panic) = handle.join()
        {
            std::panic::resume_unwind(panic);
        }
    }

    fn retire<T: Send + 'static>(&mut self, old: T) {
        self.finish();
        // Moved into the thread through a slot so a failed spawn still frees it, inline.
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(old)));
        let thread_slot = std::sync::Arc::clone(&slot);
        match std::thread::Builder::new()
            .name("spall-retire-world".into())
            .spawn(move || {
                drop(
                    thread_slot
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take(),
                );
            }) {
            Ok(handle) => self.0 = Some(handle),
            Err(_) => drop(slot.lock().unwrap_or_else(|p| p.into_inner()).take()),
        }
    }
}

impl Simulation {
    /// Prepares one live server-owned lifecycle transaction. A staged geometry
    /// edit, when present, shares its already reserved transaction/journal IDs
    /// and candidate registry with the actor operations.
    pub(crate) fn prepare_creature_lifecycle_transaction(
        &self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<PreparedSimulationTransaction, String> {
        self.prepare_creature_lifecycle_transaction_with_transfers(
            tick,
            control_seq,
            intents,
            staged,
            catalog,
            CreatureTransactionTransfers::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_creature_lifecycle_transaction_with_transfers(
        &self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
        transfers: CreatureTransactionTransfers,
    ) -> Result<PreparedSimulationTransaction, String> {
        use spall_physics::{ArticulatedBatchOperation, OccupancyGrid, SourcePart};
        use spall_protocol::{
            CREATURE_CHANGE_SCHEMA, CREATURE_LIFECYCLE_COMMIT_SCHEMA, CreatureClipMode,
            CreatureComponentMotionV1, CreatureDeathWitnessV1, CreatureLifecycleChangeV4,
            CreatureLifecycleCommitV4, CreatureLifecycleContextV1, CreatureLifecycleOperationV4,
            CreatureLifecycleState, CreatureMotionOverlayV2, CreaturePartMotionV1,
            CreatureReductionOutcome, Record, TopologyOp, TopologyTransaction,
        };

        if intents.is_empty() {
            return Err("lifecycle transaction has no actor operations".into());
        }
        if intents.len() > spall_physics::MAX_CORPSE_RESTORE_BATCH_OWNERS {
            return Err("too many lifecycle owners in one transaction".into());
        }
        let kind_bits = spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT
            | spall_protocol::WORLD_RESET_TRANSFER_ID_BIT
            | spall_protocol::CREATURE_LIFECYCLE_TRANSFER_ID_BIT;
        let valid_id = |id: spall_protocol::TransferId, kind: u64| {
            id.0 & kind_bits == kind && id.0 & !kind_bits != 0
        };
        let mut seen = std::collections::BTreeSet::new();
        if transfers.lifecycle.len() > intents.len()
            || transfers.lifecycle.iter().any(|id| {
                !valid_id(*id, spall_protocol::CREATURE_LIFECYCLE_TRANSFER_ID_BIT)
                    || !seen.insert(id.0)
            })
            || transfers.geometry.is_some_and(|id| {
                staged.is_none() || !valid_id(id, spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT)
            })
        {
            return Err("invalid reserved lifecycle or geometry transfer IDs".into());
        }
        if staged.is_some_and(|edit| {
            matches!(edit.target, EditTarget::Body(entity) if self.world.body_is_dormant(entity))
        }) {
            return Err("mixed lifecycle commit requires staged dormant-body reactivation".into());
        }
        let live_host = self
            .creatures
            .as_ref()
            .ok_or_else(|| "creature host is not initialized".to_owned())?;
        if spall_protocol::content_manifest_hash(catalog.materials)
            != spall_protocol::content_manifest_hash(self.world.materials())
        {
            return Err("creature catalog material manifest differs from simulation".into());
        }
        let mut volumes_by_intent = Vec::with_capacity(intents.len());
        let mut all_source_volumes = Vec::new();
        for intent in &intents {
            let mut ids = Vec::new();
            match intent {
                AuthoritativeCreatureIntent::Birth { state, parts, .. } => {
                    if parts.len() > spall_protocol::MAX_CREATURE_OWNER_PARTS
                        || parts.len() != state.source_bindings.len()
                    {
                        return Err("birth source part count exceeds the owner bound".into());
                    }
                    for part in parts {
                        if ids.contains(&part.volume.id()) {
                            return Err("duplicate source volume in lifecycle transaction".into());
                        }
                        ids.push(part.volume.id());
                        all_source_volumes.push(&part.volume);
                    }
                }
                AuthoritativeCreatureIntent::Death { key, .. } => {
                    let state = live_host
                        .record(key.creature_id)
                        .filter(|state| state.key == *key)
                        .ok_or_else(|| "death owner is absent".to_owned())?;
                    if state.source_bindings.len() > spall_protocol::MAX_CREATURE_OWNER_PARTS {
                        return Err("death source part count exceeds the owner bound".into());
                    }
                    for binding in &state.source_bindings {
                        if ids.contains(&binding.source_volume_id) {
                            return Err("duplicate source volume in lifecycle transaction".into());
                        }
                        ids.push(binding.source_volume_id);
                        all_source_volumes.push(
                            self.world
                                .creature_source_part(*key, binding.part_id)
                                .ok_or_else(|| "death source part is absent".to_owned())?,
                        );
                    }
                }
            }
            volumes_by_intent.push(ids);
        }
        let mut source_walk_cells = 0_u64;
        for volume in &all_source_volumes {
            let slots = u64::try_from(volume.resident_brick_count())
                .map_err(|_| "source brick count overflow")?
                .checked_mul(32 * 32 * 32)
                .ok_or_else(|| "source cell work overflow".to_owned())?;
            source_walk_cells = source_walk_cells
                .checked_add(slots)
                .ok_or_else(|| "source cell work overflow".to_owned())?;
            if source_walk_cells
                > spall_protocol::creature_owner::MAX_CREATURE_OWNER_SET_DENSE_CELLS as u64
            {
                return Err("lifecycle source traversal exceeds the aggregate work budget".into());
            }
        }
        let mut source_work_by_volume = std::collections::BTreeMap::new();
        let mut aggregate_grid_cells = 0_u64;
        let mut aggregate_solid_cells = 0_u64;
        let mut aggregate_source_bytes = 0_u64;
        for volume in &all_source_volumes {
            let work = creature_source_work(volume)?;
            if source_work_by_volume.insert(volume.id(), work).is_some() {
                return Err("duplicate source volume in lifecycle transaction".into());
            }
            aggregate_grid_cells = aggregate_grid_cells
                .checked_add(work.grid_cells)
                .ok_or_else(|| "lifecycle grid work overflow".to_owned())?;
            aggregate_solid_cells = aggregate_solid_cells
                .checked_add(work.solid_cells)
                .ok_or_else(|| "lifecycle solid-cell work overflow".to_owned())?;
            aggregate_source_bytes = aggregate_source_bytes
                .checked_add(work.grid_cells.saturating_mul(2))
                .and_then(|bytes| bytes.checked_add(work.solid_cells.saturating_mul(4)))
                .ok_or_else(|| "lifecycle source byte estimate overflow".to_owned())?;
            if aggregate_grid_cells > spall_physics::MAX_CORPSE_RESTORE_BATCH_DENSE_CELLS
                || aggregate_solid_cells > spall_physics::MAX_CORPSE_RESTORE_BATCH_SOLID_CELLS
                || aggregate_source_bytes
                    > spall_protocol::creature_owner::MAX_CREATURE_OWNER_SET_BYTES as u64
            {
                return Err("lifecycle source payload exceeds aggregate work limits".into());
            }
        }
        for ids in &volumes_by_intent {
            let mut record_grid_cells = 0_u64;
            let mut record_solid_cells = 0_u64;
            let mut record_bytes = 0_u64;
            let mut record_bricks = 0_usize;
            let mut record_masks = 0_usize;
            for id in ids {
                let work = source_work_by_volume
                    .get(id)
                    .ok_or_else(|| "source work preflight lost a volume".to_owned())?;
                record_grid_cells = record_grid_cells.saturating_add(work.grid_cells);
                record_solid_cells = record_solid_cells.saturating_add(work.solid_cells);
                record_bytes = record_bytes
                    .saturating_add(work.grid_cells.saturating_mul(2))
                    .saturating_add(work.solid_cells.saturating_mul(4));
                record_bricks = record_bricks.saturating_add(work.bricks);
                record_masks = record_masks.saturating_add(work.mask_bricks);
            }
            if record_grid_cells > spall_protocol::MAX_CREATURE_OWNER_DENSE_CELLS as u64
                || record_solid_cells > spall_protocol::MAX_CREATURE_OWNER_SOLID_CELLS as u64
                || record_solid_cells > spall_protocol::MAX_CREATURE_OWNER_MASK_CELLS as u64
                || record_bricks > spall_protocol::MAX_CREATURE_OWNER_BRICKS
                || record_masks > spall_protocol::MAX_CREATURE_OWNER_MASK_BRICKS
                || record_bytes > spall_protocol::MAX_CREATURE_OWNER_RECORD_BYTES as u64
            {
                return Err("lifecycle owner source payload exceeds record limits".into());
            }
        }
        let allocator_before = live_host.allocator_snapshot();
        let host_revision_before = live_host.revision();
        let registry_counters_before = self.world.registry().counters();
        let journal_cursor = self.journal.cursor();
        let mut geometry = if let Some(staged) = staged {
            match crate::commit::prepare_geometry_with_transfer_ids(
                &self.world,
                &self.journal,
                staged,
                tick,
                control_seq,
                &mut transfers.geometry.into_iter().collect(),
            )
            .map_err(|error| error.to_string())?
            {
                crate::commit::PrepareGeometryOutcome::Ready(geometry) => Some(geometry),
                crate::commit::PrepareGeometryOutcome::Stale(_) => {
                    return Err("staged geometry candidate is stale".into());
                }
            }
        } else {
            None
        };
        let mut candidate_registry = geometry
            .as_ref()
            .map(|geometry| geometry.registry.clone())
            .unwrap_or_else(|| self.world.registry().clone());
        let (transaction_id, journal_seq) = if let Some(geometry) = geometry.as_ref() {
            (geometry.transaction_id, geometry.journal_seq)
        } else {
            (
                candidate_registry
                    .allocate_transaction()
                    .map_err(|error| error.to_string())?,
                candidate_registry
                    .allocate_journal_seq()
                    .map_err(|error| error.to_string())?,
            )
        };
        let mut topology = geometry
            .as_ref()
            .map(|geometry| geometry.topology.clone())
            .unwrap_or(TopologyTransaction {
                transaction_id,
                server_tick: tick,
                control_seq,
                algorithm_version: crate::commit::ALGORITHM_VERSION,
                dependencies: Vec::new(),
                before: Vec::new(),
                after: Vec::new(),
                ops: Vec::new(),
                result_hashes: Vec::new(),
            });
        if topology.transaction_id != transaction_id
            || topology.server_tick != tick
            || topology.control_seq != control_seq
        {
            return Err("geometry candidate transaction framing differs".into());
        }

        enum PendingLifecycle {
            Birth {
                tag: u64,
                state: spall_protocol::CreatureStateV1,
                after: spall_protocol::CreatureOwnershipRecordV4,
                allocator_before: CreatureAllocatorSnapshotV1,
                next_volume_id: u64,
                next_volume_after: u64,
                parts: Vec<(
                    u32,
                    spall_voxel::Volume,
                    crate::world::CreatureSourceMetadata,
                )>,
            },
            Death {
                tag: u64,
                key: spall_core::CreatureKey,
                before: spall_protocol::CreatureOwnershipRecordV4,
                pre_motion: CreatureMotionOverlayV2,
                frozen_poses: Vec<(u32, spall_physics::PartTransform)>,
                witness_root_translation_m: [f64; 3],
                witness_root_rotation_xyzw: [f32; 4],
            },
        }

        let mut pending = Vec::with_capacity(intents.len());
        let mut planned_host = live_host.clone();
        let mut physics_operations = Vec::with_capacity(intents.len());
        let mut source_births = Vec::new();
        let mut death_source_fences = Vec::new();
        let mut source_ids_in_batch = std::collections::BTreeSet::new();
        let mut owner_keys_in_batch = std::collections::BTreeSet::new();
        for (index, intent) in intents.into_iter().enumerate() {
            let tag = u64::try_from(index).map_err(|_| "too many lifecycle operations")?;
            match intent {
                AuthoritativeCreatureIntent::Birth {
                    state,
                    parts,
                    representation,
                } => {
                    let state = *state;
                    state.validate(catalog).map_err(|error| error.to_string())?;
                    if state.lifecycle != CreatureLifecycleState::Alive
                        || state.clip_mode != CreatureClipMode::Playing
                        || state.linear_velocity_m_s != [0.0; 3]
                        || state.angular_velocity_rad_s != [0.0; 3]
                        || parts.len() != state.source_bindings.len()
                        || parts
                            .windows(2)
                            .any(|pair| pair[0].part_id >= pair[1].part_id)
                    {
                        return Err("birth source hierarchy or initial state is invalid".into());
                    }
                    let allocator_before = planned_host.allocator_snapshot();
                    let next_volume_id = candidate_registry.counters().1;
                    let owner_key = stable_creature_owner_key(state.key);
                    if !owner_keys_in_batch.insert(owner_key) {
                        return Err("creature owner-key hash collision in batch".into());
                    }
                    let asset = catalog
                        .assets
                        .iter()
                        .find(|asset| {
                            asset.uuid == state.asset.uuid
                                && asset.logical_hash == state.asset.logical_hash
                        })
                        .ok_or_else(|| {
                            "birth asset is absent from validation catalog".to_owned()
                        })?;
                    let portable_keys = asset
                        .portable_material_keys
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>();
                    let mapped_materials = catalog
                        .material_mapping
                        .iter()
                        .filter(|entry| portable_keys.contains(entry.portable_key))
                        .map(|entry| entry.material_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut owned_parts = Vec::with_capacity(parts.len());
                    let mut source_parts = Vec::with_capacity(parts.len());
                    for (input, binding) in parts.into_iter().zip(&state.source_bindings) {
                        let allocated = candidate_registry
                            .allocate_volume()
                            .map_err(|error| error.to_string())?;
                        if input.part_id != binding.part_id
                            || input.volume.id() != binding.source_volume_id
                            || allocated != binding.source_volume_id
                            || !source_ids_in_batch.insert(binding.source_volume_id)
                        {
                            return Err(
                                "birth source IDs differ from shared registry allocation".into()
                            );
                        }
                        let grid = OccupancyGrid::from_volume(&input.volume)
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "birth source has no solid occupancy".to_owned())?;
                        let mut materials_valid = true;
                        grid.for_each_solid(|_, _, _, material| {
                            materials_valid &= self.world.materials().contains(material)
                                && mapped_materials.contains(&material);
                        });
                        if !materials_valid {
                            return Err(
                                "birth source contains a material outside its asset map".into()
                            );
                        }
                        let metadata = creature_source_freshness(
                            &input.volume,
                            input.parent_id,
                            input.local_transform,
                        )
                        .ok_or_else(|| {
                            "birth source revision snapshot is unavailable".to_owned()
                        })?;
                        let cell_m = input.volume.cell_size().metres() as f32;
                        source_parts.push(SourcePart {
                            part_id: input.part_id,
                            parent_id: input.parent_id,
                            volume_id: binding.source_volume_id,
                            source_revision: binding.source_revision,
                            grid,
                            cell_m,
                            local_transform: input.local_transform,
                        });
                        owned_parts.push((input.part_id, input.volume, metadata));
                    }
                    let validation_parts = owned_parts
                        .iter()
                        .map(|(part_id, volume, _)| (*part_id, volume))
                        .collect::<Vec<_>>();
                    self.world
                        .validate_creature_sources(state.key, &validation_parts)
                        .map_err(str::to_owned)?;
                    let record_parts = owned_parts
                        .iter()
                        .map(|(part_id, volume, metadata)| (*part_id, volume, metadata))
                        .collect::<Vec<_>>();
                    let after = portable_birth_record(state.clone(), owner_key, &record_parts)
                        .map_err(|error| format!("birth record: {error:?}"))?;
                    after.validate(catalog).map_err(|error| error.to_string())?;
                    let translation = self
                        .world
                        .physics_translation(state.root_translation_m)
                        .map_err(|error| error.to_string())?;
                    physics_operations.push(ArticulatedBatchOperation::AliveBirth {
                        operation_tag: tag,
                        translation_m: translation,
                        rotation_xyzw: state.root_rotation_xyzw,
                        topology_token: state.aggregate_topology_revision,
                        parts: source_parts,
                        representation,
                    });
                    let next_volume_after = candidate_registry.counters().1;
                    planned_host
                        .register_validated(state.clone(), catalog)
                        .map_err(|error| format!("creature host registration: {error:?}"))?;
                    pending.push(PendingLifecycle::Birth {
                        tag,
                        state,
                        after,
                        allocator_before,
                        next_volume_id,
                        next_volume_after,
                        parts: owned_parts,
                    });
                }
                AuthoritativeCreatureIntent::Death {
                    key,
                    expected_state_revision,
                    expected_topology_token,
                    expected_pose_token,
                } => {
                    let state = self
                        .creatures
                        .as_ref()
                        .and_then(|host| host.record(key.creature_id))
                        .filter(|state| state.key == key)
                        .ok_or_else(|| "death owner is absent".to_owned())?;
                    if state.lifecycle != CreatureLifecycleState::Alive
                        || state.state_revision != expected_state_revision
                        || state.aggregate_topology_revision != expected_topology_token
                    {
                        return Err("death state or topology token is stale".into());
                    }
                    let owner_id = self
                        .world
                        .creature_physics_owner(key)
                        .ok_or_else(|| "death owner has no articulated physics body".to_owned())?;
                    let owner = self
                        .world
                        .physics()
                        .articulated_owner_snapshot(owner_id)
                        .ok_or_else(|| "death articulated owner snapshot is absent".to_owned())?;
                    let part_motion = self
                        .world
                        .physics()
                        .articulated_part_motion_snapshot(owner_id)
                        .ok_or_else(|| {
                            "death has pending or unavailable installed part motion".to_owned()
                        })?;
                    let poses = self
                        .world
                        .physics()
                        .articulated_pose_snapshot(owner_id)
                        .ok_or_else(|| "death articulated pose snapshot is absent".to_owned())?;
                    if owner.pending_pose
                        || owner.topology_token != expected_topology_token
                        || owner.pose_token != expected_pose_token
                        || poses.topology_token != expected_topology_token
                        || poses.pose_token != expected_pose_token
                        || poses
                            .parts
                            .iter()
                            .map(|(part_id, _)| *part_id)
                            .ne(state.source_bindings.iter().map(|binding| binding.part_id))
                    {
                        return Err("death has pending or stale articulated motion".into());
                    }
                    let asset = catalog
                        .assets
                        .iter()
                        .find(|asset| {
                            asset.uuid == state.asset.uuid
                                && asset.logical_hash == state.asset.logical_hash
                        })
                        .ok_or_else(|| {
                            "death asset is absent from validation catalog".to_owned()
                        })?;
                    let portable_keys = asset
                        .portable_material_keys
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>();
                    let mapped_materials = catalog
                        .material_mapping
                        .iter()
                        .filter(|entry| portable_keys.contains(entry.portable_key))
                        .map(|entry| entry.material_id)
                        .collect::<std::collections::BTreeSet<_>>();
                    let mut source_parts = Vec::with_capacity(state.source_bindings.len());
                    let mut source_refs = Vec::with_capacity(state.source_bindings.len());
                    let mut fenced_parts = Vec::with_capacity(state.source_bindings.len());
                    for binding in &state.source_bindings {
                        let volume = self
                            .world
                            .creature_source_part(key, binding.part_id)
                            .ok_or_else(|| "death source part is absent".to_owned())?;
                        let metadata = self
                            .world
                            .creature_source_metadata(key, binding.part_id)
                            .ok_or_else(|| "death source metadata is absent".to_owned())?;
                        if self.world.creature_source_owner(binding.source_volume_id)
                            != Some((key, binding.part_id))
                            || volume.id() != binding.source_volume_id
                            || metadata.state_stamp != volume.state_stamp()
                            || metadata.content_hash
                                != crate::world::volume_topology_hash_for(
                                    volume,
                                    spall_protocol::CanonicalOwner::Terrain,
                                )
                        {
                            return Err("death source ownership or content fence changed".into());
                        }
                        let grid = OccupancyGrid::from_volume(volume)
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "death source part has no solid occupancy".to_owned())?;
                        let current_revisions = volume
                            .resident_brick_coords()
                            .into_iter()
                            .map(|coord| {
                                volume
                                    .brick_revision(coord)
                                    .ok()
                                    .flatten()
                                    .map(|revision| (coord, revision.0))
                            })
                            .collect::<Option<Vec<_>>>()
                            .ok_or_else(|| {
                                "death source brick revision is unavailable".to_owned()
                            })?;
                        let mut materials_valid = true;
                        grid.for_each_solid(|_, _, _, material| {
                            materials_valid &= self.world.materials().contains(material)
                                && mapped_materials.contains(&material);
                        });
                        if metadata.brick_revisions != current_revisions || !materials_valid {
                            return Err(
                                "death source revision or asset material fence changed".into()
                            );
                        }
                        fenced_parts.push(CreatureDeathSourcePartFence {
                            part_id: binding.part_id,
                            volume_id: volume.id(),
                            state_stamp: volume.state_stamp(),
                            content_hash: crate::world::volume_topology_hash_for(
                                volume,
                                spall_protocol::CanonicalOwner::Terrain,
                            ),
                            brick_revisions: current_revisions.clone(),
                            metadata: metadata.clone(),
                        });
                        source_parts.push(SourcePart {
                            part_id: binding.part_id,
                            parent_id: metadata.parent_id,
                            volume_id: binding.source_volume_id,
                            source_revision: binding.source_revision,
                            cell_m: volume.cell_size().metres() as f32,
                            grid,
                            local_transform: metadata.initial_local_transform,
                        });
                        source_refs.push((binding.part_id, volume, metadata));
                    }
                    death_source_fences.push(CreatureDeathSourceFence {
                        key,
                        physics_owner: owner_id,
                        parts: fenced_parts,
                    });
                    let owner_key = stable_creature_owner_key(key);
                    let mut before = portable_birth_record(state.clone(), owner_key, &source_refs)
                        .map_err(|error| format!("death source record: {error:?}"))?;
                    let part_poses = poses
                        .parts
                        .iter()
                        .map(|(part_id, pose)| {
                            (
                                *part_id,
                                spall_protocol::CreatureLocalPoseV1 {
                                    translation_m: pose.translation_m,
                                    rotation_xyzw: pose.rotation_xyzw,
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    for part in &mut before.parts {
                        part.frozen_local_pose = part_poses
                            .iter()
                            .find(|(part_id, _)| *part_id == part.part_id)
                            .ok_or_else(|| "death pose part key mismatch".to_owned())?
                            .1;
                    }
                    let world_translation = self
                        .world
                        .physics_origin()
                        .to_world_f64(owner.root.translation_m);
                    before.state.root_translation_m = world_translation;
                    before.state.root_rotation_xyzw = owner.root.rotation;
                    before.state.linear_velocity_m_s = owner.root.linvel_m_s;
                    before.state.angular_velocity_rad_s = owner.root.angvel_rad_s;
                    before.components[0].translation_m = world_translation;
                    before.components[0].rotation_xyzw = owner.root.rotation;
                    before.components[0].linear_velocity_m_s = owner.root.linvel_m_s;
                    before.components[0].angular_velocity_rad_s = owner.root.angvel_rad_s;
                    before.part_velocities = part_motion
                        .iter()
                        .map(|motion| spall_protocol::CreaturePartVelocityV2 {
                            part_id: motion.part_id,
                            linear_velocity_m_s: motion.linear_velocity_m_s,
                            angular_velocity_rad_s: motion.angular_velocity_rad_s,
                        })
                        .collect();
                    let pre_motion = CreatureMotionOverlayV2 {
                        part_velocities: before.part_velocities.clone(),
                        state_revision: state.state_revision,
                        clip_mode: state.clip_mode,
                        clip_phase_tick: state.clip_phase_tick,
                        clip_subtick_remainder: state.clip_subtick_remainder,
                        clip_epoch: state.clip_epoch,
                        pose_epoch: state.pose_epoch,
                        root_translation_m: world_translation,
                        root_rotation_xyzw: owner.root.rotation,
                        linear_velocity_m_s: owner.root.linvel_m_s,
                        angular_velocity_rad_s: owner.root.angvel_rad_s,
                        locomotion_tag: state.locomotion_tag,
                        parts: part_poses
                            .iter()
                            .map(|(part_id, pose)| CreaturePartMotionV1 {
                                part_id: *part_id,
                                frozen_local_pose: *pose,
                            })
                            .collect(),
                        components: before
                            .components
                            .iter()
                            .map(|component| CreatureComponentMotionV1 {
                                owner_key: component.owner_key,
                                serial: component.serial,
                                translation_m: component.translation_m,
                                rotation_xyzw: component.rotation_xyzw,
                                linear_velocity_m_s: component.linear_velocity_m_s,
                                angular_velocity_rad_s: component.angular_velocity_rad_s,
                            })
                            .collect(),
                    };
                    physics_operations.push(ArticulatedBatchOperation::CheckedDeathTransfer {
                        operation_tag: tag,
                        id: owner_id,
                        expected_topology_token,
                        expected_pose_token,
                    });
                    pending.push(PendingLifecycle::Death {
                        tag,
                        key,
                        before,
                        pre_motion,
                        frozen_poses: poses.parts,
                        witness_root_translation_m: world_translation,
                        witness_root_rotation_xyzw: owner.root.rotation,
                    });
                }
            }
        }

        let prepared_physics = self
            .world
            .physics()
            .prepare_articulated_batch(physics_operations, |material| self.world.density(material))
            .map_err(|error| error.to_string())?;
        if prepared_physics.additional_body_slots() > spall_physics::MAX_CORPSE_RESTORE_BATCH_OWNERS
        {
            return Err("actor body reservation exceeds physics batch bound".into());
        }

        let mut candidate_host = live_host.clone();
        let mut frozen_pose_updates = Vec::new();
        let mut death_witnesses = Vec::new();
        for operation in pending {
            let ordinal = u32::try_from(topology.ops.len())
                .map_err(|_| "too many topology operations for lifecycle ordinal")?;
            let (commit, owner_after, birth_parts, birth_allocator, birth_volume_counters) =
                match operation {
                    PendingLifecycle::Birth {
                        tag,
                        state,
                        after,
                        allocator_before,
                        next_volume_id,
                        next_volume_after,
                        parts,
                    } => {
                        let change = CreatureLifecycleChangeV4 {
                            schema: CREATURE_CHANGE_SCHEMA,
                            transaction_id,
                            operation_ordinal: ordinal,
                            expected_before_hash: spall_protocol::Hash32::ZERO,
                            expected_after_hash: after.canonical_hash(),
                            operation: CreatureLifecycleOperationV4::Birth {
                                after: after.clone(),
                            },
                        };
                        (
                            CreatureLifecycleCommitV4 {
                                schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
                                change,
                                expected_before_topology_hash: spall_protocol::Hash32::ZERO,
                                pre_motion: None,
                                death_witness: None,
                            },
                            Some(after),
                            Some((tag, state, parts)),
                            Some(allocator_before),
                            Some((next_volume_id, next_volume_after)),
                        )
                    }
                    PendingLifecycle::Death {
                        tag,
                        key,
                        before,
                        pre_motion,
                        frozen_poses,
                        witness_root_translation_m,
                        witness_root_rotation_xyzw,
                    } => {
                        let transfer = prepared_physics.transfer_outcome(tag).ok_or_else(|| {
                            "prepared death omitted its checked transfer witness".to_owned()
                        })?;
                        let witness = CreatureDeathWitnessV1 {
                            key,
                            source_generation: before.source_generation,
                            before_state_revision: before.state.state_revision,
                            before_topology_revision: before.state.aggregate_topology_revision,
                            before_pose_epoch: before.state.pose_epoch,
                            root_translation_m: witness_root_translation_m,
                            root_rotation_xyzw: witness_root_rotation_xyzw,
                            installed_linear_velocity_m_s: transfer
                                .linear_velocity_m_s
                                .map(|value| value as f32),
                            installed_angular_velocity_rad_s: transfer
                                .angular_velocity_rad_s
                                .map(|value| value as f32),
                            frozen_local_poses: frozen_poses
                                .iter()
                                .map(|(part_id, pose)| {
                                    (
                                        *part_id,
                                        spall_protocol::CreatureLocalPoseV1 {
                                            translation_m: pose.translation_m,
                                            rotation_xyzw: pose.rotation_xyzw,
                                        },
                                    )
                                })
                                .collect(),
                        };
                        let mut after = before.clone();
                        after.state.state_revision = after
                            .state
                            .state_revision
                            .checked_add(1)
                            .ok_or_else(|| "creature state revision exhausted".to_owned())?;
                        after.state.lifecycle = CreatureLifecycleState::Corpse;
                        after.part_velocities.clear();
                        after.state.clip_mode = CreatureClipMode::Frozen;
                        after.state.clip_epoch = after
                            .state
                            .clip_epoch
                            .checked_add(1)
                            .ok_or_else(|| "creature clip epoch exhausted".to_owned())?;
                        after.state.pose_epoch = after
                            .state
                            .pose_epoch
                            .checked_add(1)
                            .ok_or_else(|| "creature pose epoch exhausted".to_owned())?;
                        after.state.linear_velocity_m_s = witness.installed_linear_velocity_m_s;
                        after.state.angular_velocity_rad_s =
                            witness.installed_angular_velocity_rad_s;
                        for component in &mut after.components {
                            component.linear_velocity_m_s = witness.installed_linear_velocity_m_s;
                            component.angular_velocity_rad_s =
                                witness.installed_angular_velocity_rad_s;
                        }
                        let after_hash = after.canonical_hash();
                        let change = CreatureLifecycleChangeV4 {
                            schema: CREATURE_CHANGE_SCHEMA,
                            transaction_id,
                            operation_ordinal: ordinal,
                            expected_before_hash: before.canonical_hash(),
                            expected_after_hash: after_hash,
                            operation: CreatureLifecycleOperationV4::Death { after },
                        };
                        let commit = CreatureLifecycleCommitV4 {
                            schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
                            change,
                            expected_before_topology_hash: before.topology_hash(),
                            pre_motion: Some(pre_motion),
                            death_witness: Some(witness.clone()),
                        };
                        frozen_pose_updates.push((key, frozen_poses));
                        death_witnesses.push(witness);
                        (commit, Some(before), None, None, None)
                    }
                };

            let existing_keys = candidate_host
                .records()
                .map(|state| state.key)
                .collect::<Vec<_>>();
            let mut existing_owner_keys = Vec::with_capacity(existing_keys.len());
            let mut existing_source_volumes = Vec::new();
            for state in candidate_host.records() {
                existing_owner_keys.push(stable_creature_owner_key(state.key));
                existing_source_volumes.extend(
                    state
                        .source_bindings
                        .iter()
                        .map(|binding| binding.source_volume_id),
                );
            }
            let current = match &commit.change.operation {
                CreatureLifecycleOperationV4::Birth { .. } => None,
                CreatureLifecycleOperationV4::Death { .. } => owner_after.as_ref(),
            };
            let allocator = birth_allocator.unwrap_or_else(|| candidate_host.allocator_snapshot());
            let (next_volume_id, next_volume_after) = birth_volume_counters.unwrap_or((
                candidate_registry.counters().1,
                candidate_registry.counters().1,
            ));
            let witness = commit.death_witness.as_ref();
            let reduced = commit
                .validate_and_reduce(
                    current,
                    CreatureLifecycleContextV1 {
                        transaction_id,
                        operation_ordinal: ordinal,
                        replay_receipt: None,
                        allocator,
                        next_volume_id,
                        death_witness: witness,
                        existing_keys: &existing_keys,
                        existing_owner_keys: &existing_owner_keys,
                        existing_source_volumes: &existing_source_volumes,
                    },
                    catalog,
                )
                .map_err(|error| error.to_string())?;
            if reduced.outcome != CreatureReductionOutcome::Applied {
                return Err("live producer unexpectedly reduced as a replay".into());
            }
            if let Some((tag, state, parts)) = birth_parts {
                if reduced.record.state.key != state.key
                    || reduced.next_volume_id != next_volume_after
                {
                    return Err("birth reducer and shared candidate allocators disagree".into());
                }
                candidate_host
                    .register_validated(state, catalog)
                    .map_err(|error| format!("creature host registration: {error:?}"))?;
                source_births.push((tag, reduced.record.state.key, parts));
            } else {
                candidate_host
                    .replace_record_for_lifecycle(reduced.record.state.clone(), catalog)
                    .map_err(|error| format!("creature host lifecycle update: {error:?}"))?;
            }
            topology.ops.push(TopologyOp::CreatureLifecycle {
                commit: Box::new(commit),
            });
        }
        topology.validate().map_err(|error| error.to_string())?;
        let mut attachments = geometry
            .as_mut()
            .and_then(|geometry| geometry.attachments.take());
        if !crate::replication::inline_wire_fits(&topology) {
            let mut ids = transfers.lifecycle.into_iter();
            let set =
                attachments.get_or_insert_with(|| spall_protocol::TransactionAttachmentSetV1 {
                    schema: spall_protocol::TRANSACTION_ATTACHMENT_SET_SCHEMA,
                    transaction_id,
                    attachments: Vec::new(),
                });
            let inline_ops = std::mem::take(&mut topology.ops);
            for op in inline_ops {
                if let TopologyOp::CreatureLifecycle { commit } = op {
                    let transfer_id = ids.next().ok_or_else(|| "lifecycle transaction exceeds the inline control-record ceiling without reserved bulk IDs".to_owned())?;
                    let ordinal = commit.change.operation_ordinal;
                    let payload = spall_protocol::CreatureLifecycleBulkPayloadV4::new(
                        transaction_id,
                        ordinal,
                        *commit,
                    )
                    .map_err(|error| error.to_string())?;
                    topology.ops.push(TopologyOp::CreatureLifecycleBulk {
                        transfer_id: transfer_id.0,
                        operation_ordinal: ordinal,
                        payload_schema: spall_protocol::CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
                        commit_hash: payload.commit_hash,
                    });
                    set.attachments.push(
                        spall_protocol::TransactionAttachmentV1::CreatureLifecycle {
                            transfer_id: transfer_id.0,
                            payload: Box::new(payload),
                        },
                    );
                } else {
                    topology.ops.push(op);
                }
            }
            topology.validate().map_err(|error| error.to_string())?;
            if !crate::replication::inline_wire_fits(&topology) {
                return Err(
                    "bulk lifecycle marker transaction exceeds the control-record ceiling".into(),
                );
            }
        }
        if let Some(set) = &attachments {
            // Complete correlation, cumulative work and actual encoding must succeed
            // before the physics candidate can publish. No payload is patched later.
            set.encode_for_transaction(&topology)
                .map_err(|error| error.to_string())?;
        }
        if let Some(geometry) = geometry.as_mut() {
            geometry.registry = candidate_registry.clone();
            geometry.topology = topology.clone();
        }
        let participants = geometry
            .as_ref()
            .map_or_else(Vec::new, |geometry| geometry.participants.clone());
        let actor_only_physics_fence = if geometry.is_none() {
            Some(
                self.world
                    .physics()
                    .snapshot_body_commit_fence(&[])
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let actor_only_registry = if geometry.is_none() {
            Some(candidate_registry)
        } else {
            None
        };
        Ok(PreparedSimulationTransaction {
            world_identity: self.world.commit_identity.clone(),
            registry_counters_before,
            physics_origin: self.world.physics_origin(),
            topology_epoch: self.world.topology_epoch(),
            journal_cursor,
            actor_only_physics_fence,
            allocator_before,
            host_revision_before,
            catalog_hash: creature_catalog_fingerprint(catalog),
            candidate_host,
            geometry,
            actor_only_registry,
            physics: prepared_physics,
            source_births,
            death_source_fences,
            frozen_pose_updates,
            transaction_id,
            journal_seq,
            topology,
            attachments,
            participants,
            death_witnesses,
        })
    }

    pub(crate) fn validate_prepared_creature_lifecycle(
        &self,
        mut prepared: PreparedSimulationTransaction,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<ValidatedSimulationTransaction, String> {
        if !std::sync::Arc::ptr_eq(&prepared.world_identity, &self.world.commit_identity)
            || prepared.physics_origin != self.world.physics_origin()
            || prepared.topology_epoch != self.world.topology_epoch()
            || prepared.journal_cursor != self.journal.cursor()
            || prepared.catalog_hash != creature_catalog_fingerprint(catalog)
        {
            return Err("prepared lifecycle world, journal, or catalog fence changed".into());
        }
        let host = self
            .creatures
            .as_ref()
            .ok_or_else(|| "creature host disappeared before publication".to_owned())?;
        if host.revision() != prepared.host_revision_before
            || host.allocator_snapshot() != prepared.allocator_before
        {
            return Err("prepared lifecycle creature host changed".into());
        }
        if prepared.geometry.is_none()
            && (self.world.registry().counters() != prepared.registry_counters_before
                || prepared
                    .actor_only_physics_fence
                    .as_ref()
                    .is_none_or(|fence| !self.world.physics().validate_body_commit_fence(fence)))
        {
            return Err("prepared actor-only world fence changed".into());
        }
        for (_, key, parts) in &prepared.source_births {
            let borrowed = parts
                .iter()
                .map(|(part_id, volume, _)| (*part_id, volume))
                .collect::<Vec<_>>();
            self.world
                .validate_creature_sources(*key, &borrowed)
                .map_err(str::to_owned)?;
        }
        for fence in &prepared.death_source_fences {
            if self.world.creature_physics_owner(fence.key) != Some(fence.physics_owner) {
                return Err("prepared death articulated owner binding changed".into());
            }
            for part in &fence.parts {
                let volume = self
                    .world
                    .creature_source_part(fence.key, part.part_id)
                    .ok_or_else(|| "prepared death source part disappeared".to_owned())?;
                let metadata = self
                    .world
                    .creature_source_metadata(fence.key, part.part_id)
                    .ok_or_else(|| "prepared death source metadata disappeared".to_owned())?;
                let revisions = volume
                    .resident_brick_coords()
                    .into_iter()
                    .map(|coord| {
                        volume
                            .brick_revision(coord)
                            .ok()
                            .flatten()
                            .map(|revision| (coord, revision.0))
                    })
                    .collect::<Option<Vec<_>>>();
                if volume.id() != part.volume_id
                    || volume.state_stamp() != part.state_stamp
                    || crate::world::volume_topology_hash_for(
                        volume,
                        spall_protocol::CanonicalOwner::Terrain,
                    ) != part.content_hash
                    || revisions.as_deref() != Some(part.brick_revisions.as_slice())
                    || metadata != &part.metadata
                    || self.world.creature_source_owner(part.volume_id)
                        != Some((fence.key, part.part_id))
                {
                    return Err("prepared death source ownership or content fence changed".into());
                }
            }
        }
        let geometry_slots = prepared
            .geometry
            .as_ref()
            .map_or(0, |geometry| geometry.additional_body_slots());
        let validated_physics = self
            .world
            .physics()
            .validate_articulated_batch_with_reserved_body_slots(prepared.physics, geometry_slots)
            .map_err(|error| error.to_string())?;
        let validated_geometry = prepared
            .geometry
            .take()
            .map(|geometry| {
                crate::commit::validate_prepared_geometry(&self.world, &self.journal, *geometry)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        Ok(ValidatedSimulationTransaction {
            geometry: validated_geometry,
            physics: validated_physics,
            candidate_host: prepared.candidate_host,
            actor_only_registry: prepared.actor_only_registry,
            source_births: prepared.source_births,
            frozen_pose_updates: prepared.frozen_pose_updates,
            transaction_id: prepared.transaction_id,
            journal_seq: prepared.journal_seq,
            topology: prepared.topology,
            attachments: prepared.attachments,
            participants: prepared.participants,
            death_witnesses: prepared.death_witnesses,
        })
    }

    /// Publishes a fully validated mixed candidate. Every operation below is
    /// infallible; the ordered journal is appended once after both owners.
    pub(crate) fn publish_validated_creature_lifecycle(
        &mut self,
        validated: ValidatedSimulationTransaction,
    ) -> CommittedCreatureLifecycle {
        let ValidatedSimulationTransaction {
            geometry,
            physics,
            candidate_host,
            actor_only_registry,
            source_births,
            frozen_pose_updates,
            transaction_id,
            journal_seq,
            topology,
            attachments,
            participants,
            death_witnesses,
        } = validated;
        // Finish journal ownership/copies before the first world publication.
        // The mixed parent owns the complete final set; no markers or payloads
        // are reconstructed from the geometry participant after it publishes.
        let entry = crate::journal::JournalEntry {
            seq: journal_seq,
            transaction: topology.clone(),
            participants,
            attachments: attachments.clone(),
        };
        if let Some(geometry) = geometry {
            crate::commit::publish_validated_geometry_without_journal(&mut self.world, geometry);
        } else {
            *self.world.registry_mut() = actor_only_registry
                .expect("actor-only prepared transaction owns its registry candidate");
        }
        let outcomes = self
            .world
            .physics_mut()
            .publish_validated_articulated_batch(physics);
        for (tag, key, parts) in source_births {
            self.world
                .publish_creature_sources_with_metadata(key, parts);
            let owner = outcomes
                .iter()
                .find(|outcome| outcome.operation_tag == tag)
                .expect("prepared birth has one physics outcome")
                .owner_id;
            self.world.set_creature_physics_owner(key, owner);
        }
        for (key, poses) in frozen_pose_updates {
            self.world.set_creature_frozen_poses(key, poses);
        }
        self.creatures = Some(candidate_host);
        self.journal.append(entry);
        CommittedCreatureLifecycle {
            transaction_id,
            journal_seq,
            topology,
            attachments,
            witnesses: death_witnesses,
        }
    }

    /// Commits lifecycle intents as one ordered transaction at the current
    /// tick, taking the next control sequence from the same counter as ordinary
    /// edits so replicas see a single monotone stream.
    pub fn commit_creature_lifecycle_now(
        &mut self,
        intents: Vec<AuthoritativeCreatureIntent>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<CommittedCreatureLifecycle, String> {
        let control_seq = spall_protocol::ControlSeq(self.next_control_seq);
        let committed = self.commit_creature_lifecycle_transaction(
            self.tick,
            control_seq,
            intents,
            None,
            catalog,
        )?;
        self.next_control_seq += 1;
        Ok(committed)
    }

    /// Next unallocated creature id and shared source-volume id, so a server
    /// director can build a birth batch without mutating the host.
    pub fn creature_birth_ids(&self) -> Option<(u64, u64)> {
        let next_creature = self.creature_allocator_snapshot()?.next_creature_id;
        Some((next_creature, self.world.registry().counters().1))
    }

    pub fn commit_creature_lifecycle_transaction(
        &mut self,
        tick: Tick,
        control_seq: spall_protocol::ControlSeq,
        intents: Vec<AuthoritativeCreatureIntent>,
        staged: Option<&crate::stage::StagedEdit>,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<CommittedCreatureLifecycle, String> {
        let prepared = self.prepare_creature_lifecycle_transaction(
            tick,
            control_seq,
            intents,
            staged,
            catalog,
        )?;
        let validated = self.validate_prepared_creature_lifecycle(prepared, catalog)?;
        Ok(self.publish_validated_creature_lifecycle(validated))
    }

    pub fn new(config: SimulationConfig) -> Result<Self, crate::world::WorldError> {
        Self::new_with_physics_origin(config, PhysicsOrigin::ZERO)
    }

    /// Creates a simulation with an explicit local physics frame. Authoritative
    /// world positions and protocol snapshots remain in global coordinates.
    pub fn new_with_physics_origin(
        config: SimulationConfig,
        physics_origin: PhysicsOrigin,
    ) -> Result<Self, crate::world::WorldError> {
        let column_water_setup = config.column_water;
        let column_water_worker = config.column_water_worker;
        let vegetation_setup = config.vegetation;
        let has_vegetation = vegetation_setup.is_some();
        let weather_start_season = vegetation_setup
            .as_ref()
            .map_or(0, |s| s.start_season as u8);
        let weather_season_ms = vegetation_setup.as_ref().map_or(900_000, |s| s.season_ms);
        let weather_seed = config.weather_seed;
        let world_span = crate::prof::Span::start("startup.world_and_collision");
        let world = SimWorld::new_with_terrain_collider_mode_and_origin(
            config.world,
            config.terrain_collider_mode,
            physics_origin,
        )?;
        drop(world_span);
        let mut simulation = Self {
            world,
            pipeline: EditPipeline::new(config.max_pending_intents, config.serialize_threshold),
            journal: JournalSink::new(),
            retiring: Retiring::default(),
            creatures: None,
            tick: Tick::ZERO,
            next_control_seq: 1,
            next_damage_seq: 0,
            column_water: None,
            column_water_worker: false,
            buoyancy_cache: Default::default(),
            vegetation: None,
            weather: spall_protocol::WeatherState::initial(
                weather_seed,
                1,
                spall_protocol::WeatherClock {
                    time_ms: 0,
                    source: if has_vegetation {
                        spall_protocol::WeatherClockSource::Vegetation
                    } else {
                        spall_protocol::WeatherClockSource::ServerFallback
                    },
                    start_season: weather_start_season,
                    season_ms: weather_season_ms,
                },
            ),
            weather_dirty: true,
            weather_seed,
            snow: crate::snow::SnowState::new(),
            snow_priority_tiles: Vec::new(),
            snow_priority_players: Vec::new(),
            snow_priority_refreshed_tick: 0,
            motion_footprints: crate::footprints::MotionFootprints::new(),
            footfall_candidates: Vec::new(),
        };
        if let Some(setup) = column_water_setup {
            let _span = crate::prof::Span::start("startup.column_water");
            simulation.column_water = Some(
                crate::column_water::AuthoritativeColumnWater::new(
                    &simulation.world.terrain().volume,
                    setup,
                )
                .map_err(|error| {
                    crate::world::WorldError::WaterInitialization(error.to_string())
                })?,
            );
            simulation.set_column_water_worker(column_water_worker);
        }
        simulation.pipeline.warm_labels(&simulation.world);
        if let Some(state) = vegetation_setup {
            let _span = crate::prof::Span::start("startup.vegetation_install");
            simulation
                .install_vegetation(state)
                .map_err(crate::world::WorldError::VegetationInitialization)?;
        }
        Ok(simulation)
    }

    /// Rebuilds a simulation from a recovered [`SimWorld`] (T16). `tick` is the
    /// checkpoint tick the world was restored to (and journal suffix replayed
    /// onto). The in-memory journal starts empty — the durable journal lives in
    /// `spall_store` — and control-stream sequencing restarts at 1 for the fresh
    /// post-restart session.
    pub fn from_restored(world: SimWorld, tick: Tick) -> Self {
        let simulation = Self {
            world,
            pipeline: EditPipeline::new(
                SimulationConfig::DEFAULT_MAX_PENDING_INTENTS,
                SimulationConfig::DEFAULT_SERIALIZE_THRESHOLD,
            ),
            journal: JournalSink::new(),
            retiring: Retiring::default(),
            creatures: None,
            tick,
            next_control_seq: 1,
            next_damage_seq: 0,
            // Column water is installed after recovery by the caller.
            column_water: None,
            column_water_worker: false,
            buoyancy_cache: Default::default(),
            vegetation: None,
            weather: spall_protocol::WeatherState::initial(
                0,
                1,
                spall_protocol::WeatherClock {
                    time_ms: tick.get().saturating_mul(1000) / 60,
                    source: spall_protocol::WeatherClockSource::ServerFallback,
                    start_season: 0,
                    season_ms: 900_000,
                },
            ),
            weather_dirty: false,
            weather_seed: 0,
            snow: crate::snow::SnowState::new(),
            snow_priority_tiles: Vec::new(),
            snow_priority_players: Vec::new(),
            snow_priority_refreshed_tick: 0,
            motion_footprints: crate::footprints::MotionFootprints::new(),
            footfall_candidates: Vec::new(),
        };
        simulation.pipeline.warm_labels(&simulation.world);
        simulation
    }

    pub fn world(&self) -> &SimWorld {
        &self.world
    }

    /// Admin world reset: adopt `fresh`'s world, water, and (empty) edit
    /// pipeline while keeping this simulation's tick, journal, and control
    /// sequencing, so every id a client or journal has seen stays monotonic.
    /// Id allocation resumes from whichever counter is further along, so no
    /// entity, volume, transaction, or journal sequence is handed out twice.
    /// Players are not carried over; the caller re-adds them. Staged edits of
    /// the old world are dropped.
    pub fn replace_world(&mut self, fresh: Simulation) -> Result<(), crate::world::WorldError> {
        if fresh.creatures.is_some() {
            return Err(crate::world::WorldError::CreatureIdentityReplacement);
        }
        if let Some(creatures) = &self.creatures {
            creatures.preview_reset_epoch()?;
        }
        let (entity, volume, transaction, journal_seq) = self.world.registry().counters();
        let Simulation {
            mut world,
            pipeline,
            column_water,
            vegetation,
            mut weather,
            weather_seed,
            snow,
            ..
        } = fresh;
        let (f_entity, f_volume, f_transaction, f_journal_seq) = world.registry().counters();
        world.resume_registry(
            entity.max(f_entity),
            volume.max(f_volume),
            transaction.max(f_transaction),
            journal_seq.max(f_journal_seq),
        )?;
        if let Some(creatures) = &mut self.creatures {
            creatures.reset_epoch()?;
        }
        let off_thread = self.pipeline.stages_off_thread();
        let old_world = std::mem::replace(&mut self.world, world);
        let old_pipeline = std::mem::replace(&mut self.pipeline, pipeline);
        if off_thread {
            self.pipeline.enable_off_thread_staging();
        }
        let old_column_water = std::mem::replace(&mut self.column_water, column_water);
        if self.column_water_worker
            && let Some(water) = &mut self.column_water
        {
            water.start_worker();
        }
        let old_vegetation = std::mem::replace(&mut self.vegetation, vegetation);
        weather.world_epoch = self.weather.world_epoch.saturating_add(1);
        self.weather = weather;
        self.weather_seed = weather_seed;
        self.weather_dirty = true;
        let old_snow = std::mem::replace(&mut self.snow, snow);
        self.motion_footprints.clear();
        self.footfall_candidates.clear();
        // The replaced world goes away off the tick thread.
        self.retiring.retire((
            old_world,
            old_pipeline,
            old_column_water,
            old_vegetation,
            old_snow,
        ));
        Ok(())
    }

    /// Waits until the world a reset replaced has been freed, re-raising a panic from its drop.
    pub fn wait_for_retired(&mut self) {
        self.retiring.finish();
    }

    /// Installs an empty, owner-thread creature host. Registration remains
    /// crate-private until the atomic lifecycle/source-ownership slice.
    pub fn initialize_creature_host(
        &mut self,
        world_id: WorldId,
    ) -> Result<(), crate::creatures::CreatureHostInitError> {
        if self.creatures.is_some() {
            return Err(crate::creatures::CreatureHostInitError::AlreadyInitialized);
        }
        self.creatures = Some(crate::creatures::CreatureHost::new(world_id)?);
        Ok(())
    }

    /// Restores only the in-memory actor ID/epoch counters. Actor records remain
    /// empty; this does not restore or publish creatures.
    pub fn restore_creature_allocator_snapshot(
        &mut self,
        snapshot: CreatureAllocatorSnapshotV1,
    ) -> Result<(), crate::creatures::CreatureHostInitError> {
        if self.creatures.is_some() {
            return Err(crate::creatures::CreatureHostInitError::AlreadyInitialized);
        }
        self.creatures = Some(crate::creatures::CreatureHost::from_allocator_snapshot(
            snapshot,
        )?);
        Ok(())
    }

    /// Captures the in-memory creature allocator only. This is not wired into
    /// any checkpoint or durable recovery schema.
    pub fn creature_allocator_snapshot(&self) -> Option<CreatureAllocatorSnapshotV1> {
        self.creatures
            .as_ref()
            .map(crate::creatures::CreatureHost::allocator_snapshot)
    }

    /// Test-only transaction seam for source ownership. It is intentionally
    /// absent from production builds until lifecycle replay and checkpoint
    /// publication can carry actor state.
    #[cfg(test)]
    fn commit_creature_birth_for_test(
        &mut self,
        state: spall_protocol::CreatureStateV1,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
        parts: Vec<CreatureBirthPartInput>,
        representation: spall_physics::Representation,
    ) -> Result<spall_physics::ArticulatedBodyId, crate::creatures::CreatureScheduleError> {
        use crate::creatures::CreatureScheduleError;
        use spall_physics::{OccupancyGrid, SourcePart};
        use spall_protocol::{CreatureClipMode, CreatureLifecycleState, content_manifest_hash};

        if state.lifecycle != CreatureLifecycleState::Alive
            || state.clip_mode != CreatureClipMode::Playing
            || state.linear_velocity_m_s != [0.0; 3]
            || state.angular_velocity_rad_s != [0.0; 3]
            || content_manifest_hash(catalog.materials)
                != content_manifest_hash(self.world.materials())
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }

        let mut candidate_host = self
            .creatures
            .as_ref()
            .ok_or(CreatureScheduleError::InvalidBatch)?
            .clone();
        // Validate the immutable DTO and consume only a cloned creature counter.
        candidate_host.register_validated(state.clone(), catalog)?;

        if parts.len() != state.source_bindings.len()
            || parts
                .windows(2)
                .any(|pair| pair[0].part_id >= pair[1].part_id)
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let mut candidate_registry = self.world.registry().clone();
        let asset = catalog
            .assets
            .iter()
            .find(|asset| {
                asset.uuid == state.asset.uuid && asset.logical_hash == state.asset.logical_hash
            })
            .ok_or(CreatureScheduleError::InvalidBatch)?;
        let portable_keys: std::collections::BTreeSet<_> =
            asset.portable_material_keys.iter().copied().collect();
        let mapped_materials: std::collections::BTreeSet<_> = catalog
            .material_mapping
            .iter()
            .filter(|entry| portable_keys.contains(entry.portable_key))
            .map(|entry| entry.material_id)
            .collect();
        let mut source_volumes = Vec::with_capacity(parts.len());
        let mut physics_parts = Vec::with_capacity(parts.len());
        for (input, binding) in parts.into_iter().zip(&state.source_bindings) {
            let allocated = candidate_registry
                .allocate_volume()
                .map_err(|_| CreatureScheduleError::CounterExhausted)?;
            if input.part_id != binding.part_id
                || input.volume.id() != binding.source_volume_id
                || allocated != binding.source_volume_id
            {
                return Err(CreatureScheduleError::InvalidBatch);
            }
            let grid = OccupancyGrid::from_volume(&input.volume)
                .map_err(|_| CreatureScheduleError::InvalidBatch)?
                .ok_or(CreatureScheduleError::InvalidBatch)?;
            let mut materials_valid = true;
            grid.for_each_solid(|_, _, _, material| {
                materials_valid &= self.world.materials().contains(material)
                    && mapped_materials.contains(&material);
            });
            if !materials_valid {
                return Err(CreatureScheduleError::InvalidBatch);
            }
            let freshness =
                creature_source_freshness(&input.volume, input.parent_id, input.local_transform)
                    .ok_or(CreatureScheduleError::InvalidBatch)?;
            let cell_m = input.volume.cell_size().metres() as f32;
            physics_parts.push(SourcePart {
                part_id: input.part_id,
                parent_id: input.parent_id,
                volume_id: binding.source_volume_id,
                source_revision: binding.source_revision,
                grid,
                cell_m,
                local_transform: input.local_transform,
            });
            source_volumes.push((input.part_id, input.volume, freshness));
        }
        let source_volumes_for_validation: Vec<_> = source_volumes
            .iter()
            .map(|(part_id, volume, _)| (*part_id, volume.clone()))
            .collect();
        self.world
            .validate_creature_sources(state.key, &source_volumes_for_validation)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;

        let translation = self
            .world
            .physics_translation(state.root_translation_m)
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        // The physics adapter preflights every part, collider and transform
        // before inserting any owner state. No fallible step follows success.
        let physics_owner = self
            .world
            .physics_mut()
            .add_articulated_kinematic_parts(
                translation,
                state.root_rotation_xyzw,
                state.aggregate_topology_revision,
                physics_parts,
                representation,
            )
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;

        self.world
            .publish_creature_sources_with_metadata(state.key, source_volumes);
        self.world
            .set_creature_physics_owner(state.key, physics_owner);
        self.world.registry_mut().clone_from(&candidate_registry);
        self.creatures = Some(candidate_host);
        Ok(physics_owner)
    }

    #[cfg(test)]
    fn commit_creature_death_for_test(
        &mut self,
        key: spall_core::CreatureKey,
        expected_state_revision: u64,
        expected_topology_token: u64,
        expected_pose_token: u64,
        catalog: &spall_protocol::CreatureValidationCatalog<'_>,
    ) -> Result<spall_physics::CorpseTransferOutcome, crate::creatures::CreatureScheduleError> {
        use crate::creatures::CreatureScheduleError;
        use spall_protocol::{CreatureClipMode, CreatureLifecycleState};

        let current = self
            .creatures
            .as_ref()
            .and_then(|host| host.record(key.creature_id))
            .filter(|state| state.key == key)
            .ok_or(CreatureScheduleError::Stale)?;
        if current.lifecycle != CreatureLifecycleState::Alive
            || current.state_revision != expected_state_revision
            || current.aggregate_topology_revision != expected_topology_token
            || spall_protocol::content_manifest_hash(catalog.materials)
                != spall_protocol::content_manifest_hash(self.world.materials())
        {
            return Err(CreatureScheduleError::Stale);
        }
        current
            .validate(catalog)
            .map_err(|_| CreatureScheduleError::Stale)?;
        let asset = catalog
            .assets
            .iter()
            .find(|asset| {
                asset.uuid == current.asset.uuid && asset.logical_hash == current.asset.logical_hash
            })
            .ok_or(CreatureScheduleError::Stale)?;
        let portable_keys: std::collections::BTreeSet<_> =
            asset.portable_material_keys.iter().copied().collect();
        let mapped_materials: std::collections::BTreeSet<_> = catalog
            .material_mapping
            .iter()
            .filter(|entry| portable_keys.contains(entry.portable_key))
            .map(|entry| entry.material_id)
            .collect();
        for binding in &current.source_bindings {
            let source = self
                .world
                .creature_source_part(key, binding.part_id)
                .ok_or(CreatureScheduleError::Stale)?;
            if self.world.creature_source_owner(binding.source_volume_id)
                != Some((key, binding.part_id))
                || source.id() != binding.source_volume_id
            {
                return Err(CreatureScheduleError::Stale);
            }
            let Some(metadata) = self.world.creature_source_metadata(key, binding.part_id) else {
                return Err(CreatureScheduleError::Stale);
            };
            let current_revisions: Vec<_> = source
                .resident_brick_coords()
                .into_iter()
                .map(|coord| {
                    source
                        .brick_revision(coord)
                        .ok()
                        .flatten()
                        .map(|revision| (coord, revision.0))
                })
                .collect::<Option<_>>()
                .ok_or(CreatureScheduleError::Stale)?;
            if metadata.state_stamp != source.state_stamp()
                || metadata.content_hash
                    != crate::world::volume_topology_hash_for(
                        source,
                        spall_protocol::CanonicalOwner::Terrain,
                    )
                || metadata.brick_revisions != current_revisions
            {
                return Err(CreatureScheduleError::Stale);
            }
            let grid = spall_physics::OccupancyGrid::from_volume(source)
                .map_err(|_| CreatureScheduleError::Stale)?
                .ok_or(CreatureScheduleError::Stale)?;
            let mut materials_valid = true;
            grid.for_each_solid(|_, _, _, material| {
                materials_valid &= self.world.materials().contains(material)
                    && mapped_materials.contains(&material);
            });
            if !materials_valid {
                return Err(CreatureScheduleError::Stale);
            }
        }
        let physics_owner = self
            .world
            .creature_physics_owner(key)
            .ok_or(CreatureScheduleError::Stale)?;
        let live = self
            .world
            .physics()
            .articulated_owner_snapshot(physics_owner)
            .ok_or(CreatureScheduleError::Stale)?;
        if live.pending_pose
            || live.topology_token != expected_topology_token
            || live.pose_token != expected_pose_token
        {
            return Err(CreatureScheduleError::Stale);
        }
        let live_parts = self
            .world
            .physics()
            .articulated_pose_snapshot(physics_owner)
            .ok_or(CreatureScheduleError::Stale)?;
        if live_parts.topology_token != expected_topology_token
            || live_parts.pose_token != expected_pose_token
            || live_parts
                .parts
                .iter()
                .map(|(part_id, _)| *part_id)
                .ne(current
                    .source_bindings
                    .iter()
                    .map(|binding| binding.part_id))
        {
            return Err(CreatureScheduleError::Stale);
        }

        let mut corpse = current.clone();
        corpse.state_revision = corpse
            .state_revision
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.lifecycle = CreatureLifecycleState::Corpse;
        corpse.clip_mode = CreatureClipMode::Frozen;
        corpse.clip_epoch = corpse
            .clip_epoch
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.pose_epoch = corpse
            .pose_epoch
            .checked_add(1)
            .ok_or(CreatureScheduleError::CounterExhausted)?;
        corpse.root_translation_m = self
            .world
            .physics_origin()
            .to_world_f64(live.root.translation_m);
        corpse.root_rotation_xyzw = live.root.rotation;
        corpse.linear_velocity_m_s = live.root.linvel_m_s;
        corpse.angular_velocity_rad_s = live.root.angvel_rad_s;

        let mut candidate_host = self.creatures.as_ref().unwrap().clone();

        // Density is read from the Simulation's immutable material manifest,
        // and the exact materials present in source cells are preflighted
        // before the physics owner changes.
        let mut densities = std::collections::BTreeMap::new();
        for binding in &current.source_bindings {
            let volume = self
                .world
                .creature_source_part(key, binding.part_id)
                .ok_or(CreatureScheduleError::Stale)?;
            let grid = spall_physics::OccupancyGrid::from_volume(volume)
                .map_err(|_| CreatureScheduleError::InvalidBatch)?
                .ok_or(CreatureScheduleError::InvalidBatch)?;
            grid.for_each_solid(|_, _, _, material| {
                densities
                    .entry(material)
                    .or_insert_with(|| self.world.density(material));
            });
        }
        if densities
            .values()
            .any(|density| !density.is_finite() || *density <= 0.0)
        {
            return Err(CreatureScheduleError::InvalidBatch);
        }
        let density = |material| densities.get(&material).copied().unwrap_or(0.0);

        // The adapter invokes this closure after exact mass/resultant motion
        // and collider preparation, before changing physics ownership. It
        // validates the final DTO/host candidate so no fallible step follows
        // the physical transfer.
        let outcome = self
            .world
            .physics_mut()
            .transfer_articulated_to_dynamic_corpse_checked(
                physics_owner,
                expected_topology_token,
                expected_pose_token,
                density,
                |outcome| {
                    corpse.linear_velocity_m_s =
                        outcome.linear_velocity_m_s.map(|value| value as f32);
                    corpse.angular_velocity_rad_s =
                        outcome.angular_velocity_rad_s.map(|value| value as f32);
                    candidate_host
                        .replace_record_for_lifecycle_test(corpse, catalog)
                        .is_ok()
                },
            )
            .map_err(|_| CreatureScheduleError::InvalidBatch)?;
        self.world.set_creature_frozen_poses(key, live_parts.parts);
        self.creatures = Some(candidate_host);
        Ok(outcome)
    }

    /// Stages edits on a worker thread instead of inline on the tick thread (see
    /// [`EditPipeline::enable_off_thread_staging`]). Survives [`Self::replace_world`].
    pub fn enable_off_thread_staging(&mut self) {
        self.pipeline.enable_off_thread_staging();
    }

    /// Where a committed request's time went inside the pipeline, taken once.
    pub fn take_edit_timing(
        &mut self,
        request_id: spall_protocol::RequestId,
    ) -> Option<crate::EditTiming> {
        self.pipeline.take_timing(request_id)
    }

    /// Whether edits are staged on a worker thread.
    pub fn stages_off_thread(&self) -> bool {
        self.pipeline.stages_off_thread()
    }

    /// Read-only stacked-column water, when this world installed one.
    pub fn column_water(&self) -> Option<&crate::column_water::AuthoritativeColumnWater> {
        self.column_water.as_ref()
    }

    /// Installs recovered column water (checkpoint recovery). Replaces any
    /// water this simulation started with.
    pub fn restore_column_water(
        &mut self,
        state: &spall_protocol::column_water::ColumnWaterState,
        config: spall_fluid::columns::ColumnConfig,
    ) -> Result<(), crate::column_water::ColumnWaterError> {
        let mut water = crate::column_water::AuthoritativeColumnWater::restore(
            &self.world.terrain().volume,
            state,
            config,
        )?;
        if self.column_water_worker {
            water.start_worker();
        }
        self.column_water = Some(water);
        Ok(())
    }

    /// Runs the column water's fluid step on its own thread (see
    /// [`crate::column_water::AuthoritativeColumnWater::start_worker`]) for
    /// this water and any installed later by a restore or world reset.
    pub fn set_column_water_worker(&mut self, on: bool) {
        self.column_water_worker = on;
        if on && let Some(water) = &mut self.column_water {
            water.start_worker();
        }
    }

    /// The column-water delta owed to this tick's journal batch, if any.
    pub fn take_column_water_journal(
        &mut self,
    ) -> Option<spall_protocol::column_water::ColumnWaterDelta> {
        self.column_water.as_mut()?.take_journal_delta()
    }

    /// Sets this world's scene-authored gated spring(s) to `rate` (an admin
    /// control, e.g. "fill the reservoir"): `0` is off, `1..=3` selects an
    /// increasingly large authored footprint, i.e. a faster fill (clamped
    /// into range). A no-op — not an error — when this world has no water or
    /// the scene authored no gated spring.
    pub fn set_water_spring_rate(&self, rate: u8) {
        if let Some(water) = &self.column_water {
            water.set_gated_sources_rate(rate);
        }
    }

    /// The gated spring's current rate (`0` is off), `None` when this world
    /// has no water.
    pub fn water_spring_rate(&self) -> Option<u8> {
        self.column_water.as_ref().map(|w| w.gated_sources_rate())
    }

    /// Opens (cuts to air) or closes (refills with `closed_material`) a
    /// scene-authored dam gate: `cells` covered by a set of modest spheres
    /// (never one sphere sized to the whole shape's own bounding box — for a
    /// flat, elongated authored shape such as a notch, that would overshoot
    /// to the shape's diagonal and cut a crater far bigger than authored),
    /// each submitted as a server-authored edit exactly like a T21
    /// contact-damage cut — it goes through the normal commit pipeline
    /// (staged, then committed a few ticks later, exactly like a player's
    /// own dig), so it rebuilds the collider, replicates to clients, and
    /// dirties the water boundary on its own. Returns one admitted intent's
    /// status per covering sphere (empty when `cells` is empty — the scene
    /// authored no gate) — the caller decides whether admission alone is
    /// enough to report success, or whether to also inspect the eventual
    /// commit outcome of each.
    pub fn set_dam_gate(
        &mut self,
        cells: &[GlobalCell],
        open: bool,
        closed_material: MaterialId,
    ) -> Result<Vec<ActionStatus>, IntentError> {
        let actor = EntityId::new(DAM_GATE_ACTOR_ID).expect("non-zero reserved actor id");
        let kind = if open {
            EditKind::Cut
        } else {
            EditKind::Place(closed_material)
        };
        let mut statuses = Vec::new();
        for brush in covering_spheres(cells, DAM_GATE_MAX_SPHERE_RADIUS_CELLS) {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let intent = EditIntent {
                request_id,
                actor,
                target: EditTarget::Terrain,
                kind,
                brush,
                explosion: None,
            };
            statuses.push(self.submit(intent)?);
        }
        Ok(statuses)
    }

    /// Mutable world access, for standing up a scenario (spawning a pre-existing
    /// body) before ticking.
    pub fn world_mut(&mut self) -> &mut SimWorld {
        &mut self.world
    }

    pub fn journal(&self) -> &JournalSink {
        &self.journal
    }

    /// The journal cursor for a baseline transfer: the highest sequence the
    /// sink has ever owned, retained across pruning (ENG-50).
    pub fn journal_cursor(&self) -> u64 {
        self.journal.cursor()
    }

    /// Reserves the next contiguous [`spall_core::JournalSeq`] for the
    /// integrator to own — used for the periodic 20 Hz pose batches, which
    /// share one sequence space with the committed topology transactions
    /// (`docs/protocol.md`: "Journal periodic body pose batches at 20 Hz";
    /// ENG-50: "contiguous sequence ownership").
    pub fn reserve_journal_seq(&mut self) -> Result<spall_core::JournalSeq, IdError> {
        self.world.registry_mut().allocate_journal_seq()
    }

    /// Drops in-memory journal entries at or below `through` after the
    /// integrator has flushed them durably and a checkpoint covers them
    /// (ENG-50 bounded retention). Returns how many entries were removed.
    pub fn prune_journal(&mut self, through: u64) -> usize {
        self.journal.prune_through(through)
    }

    pub fn current_tick(&self) -> Tick {
        self.tick
    }

    /// `true` when every accepted intent has committed or been rejected.
    pub fn is_idle(&self) -> bool {
        self.pipeline.is_idle()
    }

    /// Conservative count of allocator IDs the upcoming edit tick may need
    /// for oversized split baselines. This intentionally excludes unrelated
    /// scheduled work such as physics prewarm jobs.
    pub fn transfer_reservation_bound(&self) -> usize {
        self.pipeline.transfer_reservation_bound()
    }

    /// The committed transaction for a request id, if it committed.
    pub fn committed(&self, request_id: RequestId) -> Option<&crate::commit::Committed> {
        self.pipeline.committed(request_id)
    }

    /// T23 / G3 row 7 (ENG-30 row 7 increment 13): the bounded brick footprint
    /// every currently-queued (not yet staged/committed) intent targeting
    /// `volume` will need. A residency pass uses this as a preflight pin set —
    /// see [`crate::schedule::EditPipeline::pending_dependency_bricks`].
    pub fn pending_edit_bricks(
        &self,
        volume: spall_core::VolumeId,
    ) -> std::collections::HashSet<spall_core::BrickCoord> {
        self.pipeline.pending_dependency_bricks(volume)
    }

    /// The current status of an admitted request, if this simulation has seen
    /// it. Hosts use this before re-validating a reliable retry, because the
    /// original action may already have changed the geometry it targeted.
    pub fn action_status(&self, request_id: RequestId) -> Option<&ActionStatus> {
        self.pipeline.action_status(request_id)
    }

    /// Admits an edit intent for staging, or replays the stored status for a
    /// duplicate request id.
    ///
    /// An intent that targets a **dormant** body (T21) reactivates it first, so
    /// the edit always stages and commits against a live body — "sleeping
    /// bodies retain [...] future destructibility" (`docs/architecture.md`).
    pub fn submit(&mut self, intent: EditIntent) -> Result<ActionStatus, IntentError> {
        if let EditTarget::Body(entity) = intent.target
            && self.world.body_is_dormant(entity)
        {
            self.world.reactivate_body(entity);
        }
        self.pipeline.submit_intent(intent, &self.world)
    }

    /// Initializes the deterministic state for a legacy save with no weather checkpoint.
    pub fn initialize_legacy_weather(&mut self, seed: u64) {
        self.weather_seed = seed;
        let source = if self.vegetation.is_some() {
            spall_protocol::WeatherClockSource::Vegetation
        } else {
            spall_protocol::WeatherClockSource::ServerFallback
        };
        let time_ms = self
            .vegetation
            .as_ref()
            .map_or(self.tick.get().saturating_mul(1000) / 60, |v| {
                v.state.time_ms
            });
        self.weather = spall_protocol::WeatherState::for_clock(
            seed,
            self.weather.world_epoch,
            self.tick.get(),
            spall_protocol::WeatherClock {
                time_ms,
                source,
                start_season: self
                    .vegetation
                    .as_ref()
                    .map_or(self.weather.start_season, |v| v.state.start_season as u8),
                season_ms: self
                    .vegetation
                    .as_ref()
                    .map_or(self.weather.season_ms, |v| v.state.season_ms),
            },
            self.weather.world_revision,
        );
        self.weather_dirty = true;
    }

    /// Set the deterministic world seed before the first authoritative tick.
    pub fn set_weather_seed(&mut self, seed: u64) {
        self.weather_seed = seed;
        self.weather = spall_protocol::WeatherState::for_clock(
            seed,
            self.weather.world_epoch,
            self.tick.get(),
            spall_protocol::WeatherClock {
                time_ms: self.weather.clock_ms,
                source: self.weather.clock_source,
                start_season: self.weather.start_season,
                season_ms: self.weather.season_ms,
            },
            self.weather.world_revision,
        );
        self.weather_dirty = true;
    }

    /// Apply or clear a persisted server-authorized weather override at the current tick.
    pub fn set_weather_override(&mut self, kind: Option<spall_protocol::WeatherKind>) {
        self.weather.world_revision = self.weather.world_revision.saturating_add(1);
        self.weather.effective_tick = self.tick.get();
        if let Some(kind) = kind {
            self.weather.force_kind(Some(kind));
        } else {
            self.weather = spall_protocol::WeatherState::for_clock(
                self.weather_seed,
                self.weather.world_epoch,
                self.tick.get(),
                spall_protocol::WeatherClock {
                    time_ms: self.weather.clock_ms,
                    source: self.weather.clock_source,
                    start_season: self.weather.start_season,
                    season_ms: self.weather.season_ms,
                },
                self.weather.world_revision,
            );
        }
        self.weather_dirty = true;
    }

    /// Current authoritative weather state. Clock source is fixed per world.
    pub fn weather_state(&self) -> &spall_protocol::WeatherState {
        &self.weather
    }

    /// Latest weather state owed to durable journal.
    pub fn take_weather_journal(&mut self) -> Option<spall_protocol::WeatherState> {
        if !std::mem::take(&mut self.weather_dirty) {
            return None;
        }
        Some(self.weather.clone())
    }

    pub fn snow_state(&self) -> &crate::snow::SnowState {
        &self.snow
    }

    pub fn snow_state_bytes(&self) -> Result<Vec<u8>, String> {
        self.snow.to_canonical_bytes()
    }

    pub fn restore_snow_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.snow = crate::snow::SnowState::from_canonical_bytes(bytes)?;
        Ok(())
    }

    pub fn take_snow_frames(&mut self) -> Vec<spall_protocol::SnowSurfaceFrame> {
        self.snow.take_changed_frames(
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    pub fn take_snow_journal_tiles(
        &mut self,
    ) -> Result<Vec<crate::snow::SnowTileJournalState>, String> {
        self.snow
            .take_geometry_changed_keys()
            .into_iter()
            .map(|key| {
                Ok(crate::snow::SnowTileJournalState {
                    tile: [key.0, key.1, key.2],
                    state: self.snow.tile_canonical_bytes(key)?,
                })
            })
            .collect()
    }

    pub fn snow_snapshot_frames(
        &mut self,
        tile_bounds: Option<[i32; 4]>,
    ) -> Vec<spall_protocol::SnowSurfaceFrame> {
        let keys = self.snow.tile_keys();
        keys.into_iter()
            .filter(|(x, z, _)| {
                tile_bounds.is_none_or(|[min_x, max_x, min_z, max_z]| {
                    (min_x..=max_x).contains(x) && (min_z..=max_z).contains(z)
                })
            })
            .filter_map(|(x, z, y)| {
                self.snow.frame(
                    x,
                    z,
                    y,
                    self.weather.world_epoch,
                    self.weather.world_revision,
                    self.tick.get(),
                )
            })
            .collect()
    }

    pub fn snow_snapshot_page(
        &mut self,
        tile_bounds: Option<[i32; 4]>,
        after: Option<(i32, i32, i32)>,
        limit: usize,
    ) -> (
        Vec<spall_protocol::SnowSurfaceFrame>,
        Option<(i32, i32, i32)>,
    ) {
        self.snow.snapshot_page(
            tile_bounds,
            after,
            limit,
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    pub fn snow_nearest_snapshot_page(
        &mut self,
        tile_bounds: [i32; 4],
        center: (i32, i32),
        after: Option<crate::snow::SnowSnapshotCursor>,
        limit: usize,
    ) -> (
        Vec<spall_protocol::SnowSurfaceFrame>,
        Option<crate::snow::SnowSnapshotCursor>,
    ) {
        self.snow.nearest_snapshot_page(
            tile_bounds,
            center,
            after,
            limit,
            self.weather.world_epoch,
            self.weather.world_revision,
            self.tick.get(),
        )
    }

    /// Verifies recovered weather still points at the world's frozen clock source.
    pub fn validate_weather_clock_source(&self) -> Result<(), String> {
        if self.weather.clock_source == spall_protocol::WeatherClockSource::Vegetation {
            let Some(vegetation) = &self.vegetation else {
                return Err("weather clock requires recovered vegetation".into());
            };
            if vegetation.state.start_season as u8 != self.weather.start_season
                || vegetation.state.season_ms != self.weather.season_ms
            {
                return Err("weather and vegetation season clocks disagree".into());
            }
        }
        Ok(())
    }

    /// Restores the explicit weather/fallback clock record from a checkpoint or journal.
    pub fn restore_weather(&mut self, state: spall_protocol::WeatherState) -> Result<(), String> {
        state.validate().map_err(|e| e.to_string())?;
        self.weather_seed = state.weather_seed;
        let mut state = state;
        state.schema = spall_protocol::WEATHER_SCHEMA;
        self.weather = state;
        self.weather_dirty = false;
        Ok(())
    }

    fn advance_weather(&mut self) {
        let (time_ms, source, start_season, season_ms) = match self.weather.clock_source {
            spall_protocol::WeatherClockSource::Vegetation => match self.vegetation.as_ref() {
                Some(v) => (
                    v.state.time_ms,
                    spall_protocol::WeatherClockSource::Vegetation,
                    v.state.start_season as u8,
                    v.state.season_ms,
                ),
                None => (
                    self.weather.clock_ms,
                    spall_protocol::WeatherClockSource::Vegetation,
                    self.weather.start_season,
                    self.weather.season_ms,
                ),
            },
            spall_protocol::WeatherClockSource::ServerFallback => (
                self.tick.get().saturating_mul(1000) / 60,
                spall_protocol::WeatherClockSource::ServerFallback,
                self.weather.start_season,
                self.weather.season_ms,
            ),
        };
        let clock = spall_protocol::WeatherClock {
            time_ms,
            source,
            start_season,
            season_ms,
        };
        let next = spall_protocol::WeatherState::for_clock(
            self.weather_seed,
            self.weather.world_epoch,
            self.tick.get(),
            clock,
            self.weather.world_revision,
        );
        let mut next = next;
        if let Some(kind) = self.weather.override_kind {
            next.force_kind(Some(kind));
        }
        let profile_changed = next.transition_id != self.weather.transition_id
            || next.kind != self.weather.kind
            || next.precipitation_q16 != self.weather.precipitation_q16
            || next.gust_q16 != self.weather.gust_q16
            || next.temperature_centi_c != self.weather.temperature_centi_c
            || next.lightning_id != self.weather.lightning_id;
        self.weather = next;
        self.weather_dirty |= profile_changed;
    }

    /// Advances one server tick: run the edit pipeline, then step physics and
    /// refresh extracted body state.
    pub fn tick(&mut self) -> Result<TickReport, TickError> {
        self.tick_with_creature_rules_inner(None, VecDeque::new())
    }

    /// Advances a server tick using transfer IDs reserved by the server's
    /// shared allocator before any tick mutation. Unused values remain burned.
    pub fn tick_with_transfer_ids(
        &mut self,
        transfer_ids: Vec<spall_protocol::TransferId>,
    ) -> Result<TickReport, TickError> {
        let mut seen = std::collections::HashSet::with_capacity(transfer_ids.len());
        if transfer_ids.iter().any(|id| {
            id.0 == 0
                || id.0 & ((1_u64 << 61) - 1) == 0
                || id.0 & spall_protocol::SPLIT_BULK_TRANSFER_ID_BIT == 0
                || id.0 & (spall_protocol::WORLD_RESET_TRANSFER_ID_BIT | (1_u64 << 61)) != 0
                || !seen.insert(id.0)
        }) {
            return Err(CommitError::InvalidBulkTransferIds.into());
        }
        self.tick_with_creature_rules_inner(None, transfer_ids.into())
    }

    /// Runs the optional statically linked game scheduling policy against a
    /// bounded immutable host snapshot. Returned intents are proposals only;
    /// this slice does not apply actor movement or lifecycle changes.
    pub fn tick_with_creature_rules(
        &mut self,
        rules: &dyn crate::creatures::CreatureSchedulingRules,
    ) -> Result<TickReport, TickError> {
        self.tick_with_creature_rules_inner(Some(rules), VecDeque::new())
    }

    fn tick_with_creature_rules_inner(
        &mut self,
        rules: Option<&dyn crate::creatures::CreatureSchedulingRules>,
        mut transfer_ids: VecDeque<spall_protocol::TransferId>,
    ) -> Result<TickReport, TickError> {
        self.tick = self
            .tick
            .checked_next()
            .map_err(|_: IdError| TickError::TickExhausted)?;
        let pipeline_span = crate::prof::Span::start("tick.pipeline");
        let report = self.pipeline.run_tick_with_transfer_ids(
            &mut self.world,
            &mut self.journal,
            self.tick,
            &mut self.next_control_seq,
            &mut transfer_ids,
        )?;
        drop(pipeline_span);
        let mut report = report;
        // Only a committed change to a terrain brick at or next to a region's
        // fluid domain can change that region's boundary.
        let terrain_id = self.world.terrain_volume_id();
        let edited_terrain_bricks: Vec<spall_core::BrickCoord> = report
            .committed
            .iter()
            .flat_map(|(_, committed)| {
                committed
                    .topology
                    .before
                    .iter()
                    .chain(&committed.topology.after)
                    .filter(|b| b.volume == terrain_id)
                    .map(|b| b.coord)
            })
            .collect();
        let water_span = crate::prof::Span::start("tick.water");
        if let Some(water) = &mut self.column_water {
            let players: Vec<[f64; 2]> = self
                .world
                .players()
                .map(|player| {
                    let (lo, hi) = player.capsule_aabb_m();
                    [0.5 * (lo[0] + hi[0]), 0.5 * (lo[2] + hi[2])]
                })
                .collect();
            water.set_focus(&players);
            let reloaded: Vec<spall_core::BrickCoord> = report
                .reloaded_bricks
                .iter()
                .filter(|(volume, _)| *volume == terrain_id)
                .map(|(_, coord)| *coord)
                .collect();
            report.column_water = Some(
                water
                    .tick(
                        &self.world.terrain().volume,
                        &edited_terrain_bricks,
                        &reloaded,
                        f64::from(TICK_DT_S),
                    )
                    .map_err(|error| TickError::Water(error.to_string()))?,
            );
        }
        drop(water_span);
        let vegetation_span = crate::prof::Span::start("tick.vegetation");
        self.advance_vegetation(&report)?;
        self.advance_weather();
        self.snow
            .advance(&self.weather, self.tick.get())
            .map_err(TickError::Snow)?;
        self.refresh_snow_from_report(&report);
        drop(vegetation_span);
        if let (Some(creatures), Some(rules)) = (&mut self.creatures, rules) {
            let terrain_revision = self.world.terrain().volume.next_revision().get();
            let water_frame_revision = self.column_water.as_ref().map(|water| water.frame().seq);
            report.creature_schedule = Some(creatures.schedule(
                self.tick.get(),
                terrain_revision,
                water_frame_revision,
                rules,
            ));
        }
        // Bodies float: buoyancy and drag from the column water, before the
        // physics step that integrates them.
        if let Some(water) = &self.column_water {
            // From the published frame, so a fluid step running on its worker
            // never makes the tick wait.
            let frame = water.frame();
            let sampler = frame.sampler();
            report.buoyancy = Some(self.world.apply_buoyancy(
                &|x, y, z| sampler.sample_at_m(x, y, z).map(|w| w.surface_y),
                &|x, y, z| sampler.sample_at_m(x, y, z).map(|w| w.flow_m_s),
                f64::from(TICK_DT_S),
                &mut self.buoyancy_cache,
            ));
        }
        let physics_span = crate::prof::Span::start("tick.physics");
        let physics_started = std::time::Instant::now();
        self.world.step_physics();
        let physics_duration = physics_started.elapsed();
        report.physics_duration = physics_duration;
        drop(physics_span);
        let players_span = crate::prof::Span::start("tick.players");
        self.advance_players(&report);
        drop(players_span);
        Ok(report)
    }

    /// Converts this tick's solver contacts into bounded terrain-damage cuts and
    /// admits them (T21, increment 1). Call **after** [`Self::tick`], passing the
    /// [`TickReport`] it returned so a body created this tick is excluded from
    /// the recursion guard.
    ///
    /// This is deliberately *not* run by [`Self::tick`]: the integrator opts in,
    /// owns the [`ContactDamagePolicy`] (its cooldown state), and decides the
    /// cadence. Admitted cuts stage off-tick and commit on a later tick, exactly
    /// like a client edit — so a contact can never mutate world state inside the
    /// physics step, and a damage cut cannot cascade into another cut the same
    /// tick.
    ///
    /// Increment 1 damaged **terrain only**. Increment 3 adds **body-on-body**
    /// fracture: a `(dynamic, dynamic)` contact carves a bounded cut into the
    /// body being struck — the slower of the pair, tie-broken to the lower mass
    /// (`ContactDamageConfig::still_speed_m_s`) — through the same threshold,
    /// per-region cooldown, and per-tick cap. The contact point is resolved into
    /// the struck body's local cell frame here, so the pure policy stays in cell
    /// coordinates and a moving body's cooldown spot does not drift.
    pub fn apply_contact_damage(
        &mut self,
        policy: &mut ContactDamagePolicy,
        report: &TickReport,
    ) -> ContactDamageReport {
        let mut out = ContactDamageReport::default();
        if self.world.body_count() == 0 {
            return out;
        }

        let terrain_volume = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let g = {
            let a = self.world.physics().gravity_m_s2();
            (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
        };
        let dt = TICK_DT_S;
        let still_speed = policy.config().still_speed_m_s;

        let born_this_tick: HashSet<u64> = report
            .committed
            .iter()
            .flat_map(|(_, c)| c.children.iter().map(|e| e.get()))
            .collect();

        let mut events: Vec<ContactEvent> = Vec::new();
        for contact in self.world.physics().contact_impulses() {
            if !contact.point_m.iter().all(|v| v.is_finite()) {
                continue;
            }
            let world_point =
                DVec3::from_array(self.world.physics_origin().to_world_f64(contact.point_m));
            match (contact.dynamic[0], contact.dynamic[1]) {
                // Exactly one side dynamic, the other terrain: a body striking
                // the world grid (increment 1).
                (true, false) | (false, true) => {
                    let striker_idx = if contact.dynamic[0] { 0 } else { 1 };
                    if !self
                        .world
                        .is_terrain_physics_body(contact.bodies[1 - striker_idx])
                    {
                        continue;
                    }
                    let striker_phys = contact.bodies[striker_idx];
                    let Some(striker) =
                        self.world.body_by_phys(striker_phys).and_then(|b| b.entity)
                    else {
                        continue;
                    };
                    let mass = self.world.physics().body_state(striker_phys).mass_kg;
                    let point_cell = (world_point / cell_m).to_array();
                    // Contact normals point from body 0 to body 1. Sampling
                    // steps against target-to-striker, so orient from target
                    // (terrain) toward the selected striker.
                    let normal =
                        target_to_striker_normal(contact.normal.map(f64::from), 1 - striker_idx);
                    let Some(target_material) =
                        sample_contact_material(&self.world, terrain_volume, point_cell, normal)
                    else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Terrain,
                        target_volume: terrain_volume,
                        target_material,
                        point_cell,
                        normal,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass * g * dt,
                        striker_born_this_tick: born_this_tick.contains(&striker.get()),
                    });
                }
                // Both sides dynamic: debris-on-debris (increment 3). Damage the
                // body being struck.
                (true, true) => {
                    let (pa, pb) = (contact.bodies[0], contact.bodies[1]);
                    let (Some(ba), Some(bb)) =
                        (self.world.body_by_phys(pa), self.world.body_by_phys(pb))
                    else {
                        continue;
                    };
                    let (Some(ea), Some(eb)) = (ba.entity, bb.entity) else {
                        continue;
                    };
                    debug_assert!(
                        !ba.dormant && !bb.dormant,
                        "a dormant body has no physics body, so it cannot appear in a live contact"
                    );
                    let sa = self.world.physics().body_state(pa);
                    let sb = self.world.physics().body_state(pb);
                    let (speed_a, speed_b) = (f64::from(sa.speed_m_s()), f64::from(sb.speed_m_s()));
                    // Struck = slower body; near-equal speeds -> lower mass;
                    // still equal -> lower entity id (fully deterministic).
                    let strike_a = if (speed_a - speed_b).abs() <= still_speed {
                        if (sa.mass_kg - sb.mass_kg).abs() <= f32::EPSILON {
                            ea.get() <= eb.get()
                        } else {
                            sa.mass_kg <= sb.mass_kg
                        }
                    } else {
                        speed_a < speed_b
                    };
                    let (struck_body, struck_entity, mass_struck) = if strike_a {
                        (ba, ea, sa.mass_kg)
                    } else {
                        (bb, eb, sb.mass_kg)
                    };
                    // World contact point -> the struck body's local cell frame,
                    // so the brush and the cooldown key travel with the body.
                    let point_cell = struck_body
                        .pose
                        .xform(struck_body.cell_size())
                        .world_to_local_cell(world_point)
                        .to_array();
                    let normal_target_to_striker = target_to_striker_normal(
                        contact.normal.map(f64::from),
                        if strike_a { 0 } else { 1 },
                    );
                    let normal_world = DVec3::from_array(normal_target_to_striker);
                    let normal_local = struck_body.pose.rotation.inverse() * normal_world;
                    let Some(target_material) = sample_contact_material(
                        &self.world,
                        struck_body.volume_id,
                        point_cell,
                        normal_local.to_array(),
                    ) else {
                        continue;
                    };
                    events.push(ContactEvent {
                        target: EditTarget::Body(struck_entity),
                        target_volume: struck_body.volume_id,
                        target_material,
                        point_cell,
                        normal: normal_target_to_striker,
                        impulse_n_s: contact.normal_impulse_n_s,
                        resting_impulse_n_s: mass_struck * g * dt,
                        // A body split out this tick is at its split instant, not
                        // a real impact — guard on either side of the pair.
                        striker_born_this_tick: born_this_tick.contains(&ea.get())
                            || born_this_tick.contains(&eb.get()),
                    });
                }
                // Only terrain is Fixed, so a fixed/fixed pair cannot occur.
                (false, false) => continue,
            }
        }

        let plan = policy.plan(self.tick.get(), &events);
        let actor = EntityId::new(CONTACT_DAMAGE_ACTOR_ID).expect("non-zero reserved actor id");
        for cut in &plan.damage {
            let request_id = RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq);
            self.next_damage_seq += 1;
            let mut intent = EditIntent::cut(request_id, actor, cut.target, cut.brush);
            if let Some(explosion) = cut.explosion {
                intent = intent.with_explosion(explosion);
            }
            // Route through `submit` so the dormancy-wake guard covers a
            // body-targeted cut uniformly with client edits (a struck body is
            // live here, so this is a no-op in practice — but consistent).
            match self.submit(intent) {
                Ok(_) => out.submitted += 1,
                Err(_) => out.rejected += 1,
            }
        }
        out.plan = plan;
        out
    }

    /// Runs the T21 region-dormancy policy for the current tick and applies its
    /// decisions: settled detached bodies with a quiet interaction region are
    /// deactivated (dropped from the physics step, record kept), and dormant
    /// bodies a player or an awake body has approached are reactivated.
    ///
    /// Opt-in, like [`Self::apply_contact_damage`]: the integrator owns the
    /// [`DormancyPolicy`] and the cadence, and it is *not* run by [`Self::tick`].
    /// Dormancy never changes authoritative geometry, ownership, or damage, so
    /// [`SimWorld::world_hash`](crate::world::SimWorld::world_hash),
    /// conservation, and the checkpoint set are unaffected. An edit that targets
    /// a dormant body still wakes it immediately through [`Self::submit`],
    /// independent of this pass.
    ///
    /// Pass the [`TickReport`] from the matching [`Self::tick`]: a **terrain**
    /// transaction committed this tick that lands within
    /// [`DormancyConfig::wake_margin_m`](crate::dormancy::DormancyConfig) of a
    /// dormant body hard-wakes it — the ground a settled body rests on just
    /// changed, so it must not wait out the hysteresis window
    /// (`docs/architecture.md`: "nearby edits wake affected neighbors").
    pub fn apply_dormancy(
        &mut self,
        policy: &mut DormancyPolicy,
        report: &TickReport,
    ) -> DormancyPlan {
        if self.world.body_count() == 0 {
            return DormancyPlan::default();
        }

        // Active interaction regions: every player capsule, plus every body that
        // is genuinely moving (an approaching / rolling piece — "active body
        // trajectories", docs/architecture.md).
        let still_speed = policy.config().still_speed_m_s;
        let mut regions: Vec<ActiveRegion> = Vec::new();
        for player in self.world.players() {
            let (lo, hi) = player.capsule_aabb_m();
            let centre = [
                0.5 * (lo[0] + hi[0]),
                0.5 * (lo[1] + hi[1]),
                0.5 * (lo[2] + hi[2]),
            ];
            let radius = 0.5
                * ((hi[0] - lo[0]).powi(2) + (hi[1] - lo[1]).powi(2) + (hi[2] - lo[2]).powi(2))
                    .sqrt();
            regions.push(ActiveRegion {
                centre_m: centre,
                radius_m: radius,
            });
        }
        for body in self.world.bodies() {
            if body.dormant {
                continue;
            }
            let speed = {
                let v = body.linvel_m_s;
                (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
            };
            if speed > still_speed {
                let (centre_m, radius_m) = body.world_bounding_sphere();
                regions.push(ActiveRegion { centre_m, radius_m });
            }
        }

        // Terrain transactions committed this tick: their world boxes hard-wake
        // any dormant body resting within `wake_margin_m` of the cut (increment
        // 3). Order-independent — a body is woken iff *any* box is close enough.
        let wake_margin_m = policy.config().wake_margin_m;
        let terrain = self.world.terrain_volume_id();
        let terrain_cell_m = self.world.terrain().cell_size().metres();
        let terrain_edit_boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, c)| transaction_world_box(&c.topology, terrain, terrain_cell_m))
            .collect();

        let inputs: Vec<BodyDormancyInput> = self
            .world
            .bodies()
            .filter_map(|body| {
                let entity = body.entity?;
                let (centre_m, radius_m) = body.world_bounding_sphere();
                let speed = {
                    let v = body.linvel_m_s;
                    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
                };
                // A body-targeted edit wakes its target through `submit` before
                // it is ever staged, so a dormant body never has a pending edit
                // against it here. A *terrain* cut under settled rubble is the
                // remaining case: hard-wake it so the support change is honoured
                // this tick, not after the hysteresis window.
                let hard_wake = body.dormant
                    && terrain_edit_boxes
                        .iter()
                        .any(|b| box_sphere_gap(*b, centre_m, radius_m) <= wake_margin_m);
                Some(BodyDormancyInput {
                    entity,
                    centre_m,
                    radius_m,
                    sleeping: body.sleeping,
                    speed_m_s: speed,
                    dormant: body.dormant,
                    hard_wake,
                })
            })
            .collect();

        let plan = policy.plan(self.tick.get(), &inputs, &regions);
        // A body with a queued or staged edit stays live until that edit commits (the commit
        // rebuilds its collider); it is retried on a later tick.
        let targeted = self.pipeline.targeted_bodies();
        for &entity in &plan.deactivate {
            if targeted.contains(&entity) {
                continue;
            }
            self.world.deactivate_body(entity);
        }
        for &entity in &plan.reactivate {
            self.world.reactivate_body(entity);
        }
        plan
    }

    /// Advances the player capsules after the physics step, so the character
    /// sweep runs against the broad-phase BVH this tick's [`SimWorld::step_physics`]
    /// just refreshed — including any collider a commit rebuilt. A player near a
    /// cell this tick's transactions edited has its prediction epoch bumped and
    /// is depenetrated (`crate::player`).
    fn advance_players(&mut self, report: &TickReport) {
        if self.world.player_count() == 0 {
            return;
        }
        let terrain = self.world.terrain_volume_id();
        let cell_m = self.world.terrain().cell_size().metres();
        let boxes: Vec<([f64; 3], [f64; 3])> = report
            .committed
            .iter()
            .filter_map(|(_, committed)| {
                transaction_world_box(&committed.topology, terrain, cell_m)
            })
            .collect();
        let frame = self.column_water.as_ref().map(|w| w.frame());
        let sampler = frame.as_ref().map(|f| f.sampler());
        self.world.advance_players(TICK_DT_S, &boxes, &|x, y, z| {
            sampler.as_ref().and_then(|s| s.sample_at_m(x, y, z))
        });
        self.motion_footprints
            .retain_players(self.world.players().map(|player| player.entity));
        self.footfall_candidates.clear();
        self.footfall_candidates.extend(
            self.world
                .players()
                .filter_map(|player| self.motion_footprints.observe(player, self.tick.get())),
        );
        self.stamp_snow_candidates();
        if std::env::var_os("SPALL_DEBUG_PLAYER").is_some() {
            for p in self.world.players() {
                eprintln!(
                    "DBGPLAYER tick={} pos={:?} grounded={} vel={:?}",
                    self.tick.0, p.state.position_m, p.state.grounded, p.state.velocity_m_s
                );
            }
        }
    }

    /// Server-authoritative stride proposals produced after capsule movement.
    /// The snow subsystem revalidates each proposal against exact terrain state.
    pub fn take_footfall_candidates(&mut self) -> Vec<crate::footprints::FootfallCandidate> {
        std::mem::take(&mut self.footfall_candidates)
    }

    fn refresh_snow_from_report(&mut self, report: &TickReport) {
        use spall_core::CellSizeCode;
        use std::collections::BTreeSet;

        let terrain_body = self.world.terrain();
        let terrain = &terrain_body.volume;
        if terrain.cell_size() != CellSizeCode::Quarter {
            return;
        }
        // Volume may be intentionally unbounded for sparse natural worlds;
        // collider_region is the durable worldgen extent and therefore the
        // authoritative limit for sky exposure scans.
        let (region_min, region_max) = terrain_body.collider_region;
        let min_tile_x_raw = region_min.x.div_euclid(16);
        let max_tile_x_raw = region_max.x.div_euclid(16);
        let min_tile_z_raw = region_min.z.div_euclid(16);
        let max_tile_z_raw = region_max.z.div_euclid(16);
        let Ok(min_tile_x) = i32::try_from(min_tile_x_raw) else {
            return;
        };
        let Ok(max_tile_x) = i32::try_from(max_tile_x_raw) else {
            return;
        };
        let Ok(min_tile_z) = i32::try_from(min_tile_z_raw) else {
            return;
        };
        let Ok(max_tile_z) = i32::try_from(max_tile_z_raw) else {
            return;
        };
        if self.snow.needs_resident_discovery_seed() {
            self.snow
                .seed_resident_discovery(&terrain.resident_brick_coords());
        }
        let mut player_tiles = Vec::new();
        for player in self.world.players() {
            let tile_x = (player.state.position_m[0] / 4.0).floor();
            let tile_z = (player.state.position_m[2] / 4.0).floor();
            if tile_x < f64::from(i32::MIN)
                || tile_x > f64::from(i32::MAX)
                || tile_z < f64::from(i32::MIN)
                || tile_z > f64::from(i32::MAX)
            {
                continue;
            }
            let (tile_x, tile_z) = (tile_x as i32, tile_z as i32);
            player_tiles.push((tile_x, tile_z));
        }
        player_tiles.retain(|(x, z)| {
            (min_tile_x..=max_tile_x).contains(x) && (min_tile_z..=max_tile_z).contains(z)
        });
        // Cache interleaved player-center/ring order. Rebuild only when a
        // player changes snow tile or once a second, never by allocating a
        // 96 m ring on every authoritative tick.
        if self.snow_priority_players != player_tiles
            || self.tick.get() <= 1
            || self
                .tick
                .get()
                .saturating_sub(self.snow_priority_refreshed_tick)
                >= 60
        {
            self.snow_priority_players.clone_from(&player_tiles);
            self.snow_priority_refreshed_tick = self.tick.get();
            self.snow_priority_tiles.clear();
            for &(x, z) in &player_tiles {
                self.snow.enqueue_scan((x, z), 0);
                self.snow_priority_tiles.push((x, z));
            }
            for radius in 1i32..=24 {
                for &(tile_x, tile_z) in &player_tiles {
                    for dz in -radius..=radius {
                        for dx in -radius..=radius {
                            if dx.abs().max(dz.abs()) != radius {
                                continue;
                            }
                            if let (Some(x), Some(z)) =
                                (tile_x.checked_add(dx), tile_z.checked_add(dz))
                            {
                                if !(min_tile_x..=max_tile_x).contains(&x)
                                    || !(min_tile_z..=max_tile_z).contains(&z)
                                {
                                    continue;
                                }
                                self.snow.enqueue_scan((x, z), 0);
                                self.snow_priority_tiles.push((x, z));
                            }
                        }
                    }
                }
            }
        }
        let mut affected = BTreeSet::new();
        for brick in report.committed.iter().flat_map(|(_, committed)| {
            committed
                .topology
                .before
                .iter()
                .chain(&committed.topology.after)
                .filter(|brick| brick.volume == terrain.id())
                .map(|brick| brick.coord)
        }) {
            for dz in 0..2 {
                for dx in 0..2 {
                    let tx = brick.x.saturating_mul(2).saturating_add(dx);
                    let tz = brick.z.saturating_mul(2).saturating_add(dz);
                    if let (Ok(tx), Ok(tz)) = (i32::try_from(tx), i32::try_from(tz)) {
                        affected.insert((tx, tz));
                        self.snow.enqueue_edited_scan((tx, tz), self.tick.get());
                    }
                }
            }
        }
        let work = self
            .snow
            .take_scan_batch(&self.snow_priority_tiles, 16, self.tick.get());
        let water = self.column_water.as_ref().map(|w| w.frame());
        let water_sampler = water.as_ref().map(|frame| frame.sampler());
        for ((tile_x, tile_z), birth_tick) in work {
            let was_scanned = self.snow.tile_scanned((tile_x, tile_z));
            let edited_tile = affected.contains(&(tile_x, tile_z));
            let mut layers = BTreeSet::new();
            let mut masks = std::collections::BTreeMap::<i32, ([u64; 4], u64)>::new();
            let geometry = match crate::snow_query::query_snow_tile_in_region(
                terrain, tile_x, tile_z, region_min, region_max,
            ) {
                Ok(geometry) => geometry,
                Err(crate::snow_query::SnowTileQueryError::OutsideBounds) => {
                    self.snow.mark_tile_scanned((tile_x, tile_z));
                    continue;
                }
                Err(
                    crate::snow_query::SnowTileQueryError::UnsupportedCellSize
                    | crate::snow_query::SnowTileQueryError::MissingBounds
                    | crate::snow_query::SnowTileQueryError::VerticalSpan
                    | crate::snow_query::SnowTileQueryError::CoordinateRange,
                ) => {
                    self.snow.mark_tile_scanned((tile_x, tile_z));
                    continue;
                }
                Err(_) => {
                    self.snow
                        .retry_scan((tile_x, tile_z), birth_tick, self.tick.get());
                    continue;
                }
            };
            let x0 = i64::from(tile_x) * 16;
            let z0 = i64::from(tile_z) * 16;
            for (y, layer) in geometry.layers {
                let mut mask = layer.exposed_faces;
                for bit in 0..256usize {
                    if mask[bit / 64] & (1u64 << (bit % 64)) == 0 {
                        continue;
                    }
                    let dx = (bit % 16) as i64;
                    let dz = (bit / 16) as i64;
                    let top_m = (f64::from(y) + 1.0) * 0.25;
                    if water_sampler
                        .as_ref()
                        .and_then(|sampler| {
                            sampler.sample_at_m(
                                (x0 + dx) as f64 * 0.25 + 0.125,
                                top_m + 0.01,
                                (z0 + dz) as f64 * 0.25 + 0.125,
                            )
                        })
                        .is_some_and(|surface| surface.surface_y > top_m + 0.02)
                    {
                        mask[bit / 64] &= !(1u64 << (bit % 64));
                    }
                }
                if mask.iter().any(|word| *word != 0) {
                    masks.insert(y, (mask, layer.geometry_revision));
                }
            }
            layers.extend(masks.keys().copied());
            let prior = self.snow.tile_layers(tile_x, tile_z);
            let tick = self.tick.get();
            let birth = if birth_tick == 0 { tick } else { birth_tick };
            for (y, (mask, revision)) in masks {
                if was_scanned || edited_tile || birth_tick > 0 {
                    self.snow
                        .reconcile_at(tile_x, tile_z, y, revision, mask, birth, tick);
                    if edited_tile || birth_tick > 0 {
                        self.snow.mark_geometry_changed((tile_x, tile_z, y));
                    }
                } else {
                    self.snow
                        .discover_initial(tile_x, tile_z, y, revision, mask);
                }
            }
            if was_scanned {
                for (_, _, y) in prior {
                    if !layers.contains(&y) {
                        self.snow.reconcile(tile_x, tile_z, y, 0, [0; 4], tick);
                        self.snow.mark_geometry_changed((tile_x, tile_z, y));
                    }
                }
            }
            self.snow.mark_tile_scanned((tile_x, tile_z));
        }
    }

    fn stamp_snow_candidates(&mut self) {
        let cell_m = 0.25;
        let terrain = &self.world.terrain().volume;
        if terrain.cell_size() != spall_core::CellSizeCode::Quarter {
            return;
        }
        let water = self.column_water.as_ref().map(|w| w.frame());
        let water_sampler = water.as_ref().map(|frame| frame.sampler());
        let candidates = std::mem::take(&mut self.footfall_candidates);
        for candidate in candidates {
            let x = (candidate.position_m[0] / cell_m).floor() as i64;
            let z = (candidate.position_m[2] / cell_m).floor() as i64;
            let y = (candidate.position_m[1] / cell_m).floor() as i64 - 1;
            let (Ok(x), Ok(y), Ok(z)) = (i32::try_from(x), i32::try_from(y), i32::try_from(z))
            else {
                continue;
            };
            let cell = GlobalCell::new(i64::from(x), i64::from(y), i64::from(z));
            if !matches!(terrain.sample(cell), Ok(Sample::Filled(_))) {
                continue;
            }
            let top_m = (f64::from(y) + 1.0) * cell_m;
            if (candidate.position_m[1] - top_m).abs() > 0.045 {
                continue;
            }
            if water_sampler
                .as_ref()
                .and_then(|sampler| {
                    sampler.sample_at_m(
                        candidate.position_m[0],
                        top_m + 0.01,
                        candidate.position_m[2],
                    )
                })
                .is_some_and(|surface| surface.surface_y > top_m + 0.02)
            {
                continue;
            }
            let (brick, _) = cell.split();
            let Some(revision) = terrain
                .brick_revision(brick)
                .ok()
                .flatten()
                .map(|revision| revision.get())
            else {
                continue;
            };
            self.snow
                .stamp(candidate, [x, y, z], revision, self.tick.get());
        }
    }

    /// Registers an authoritative player capsule at `feet_m` (metres). `entity`
    /// is a reserved-band id from [`spall_core::player_entity_for`].
    pub fn add_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> EntityId {
        self.world
            .add_player(entity, feet_m, CharacterParams::DEFAULT)
    }

    /// Feeds one validated input frame to a player (at most one per tick per
    /// player). Returns `false` for an unknown player or a stale / duplicate /
    /// non-finite frame.
    pub fn set_player_input(
        &mut self,
        entity: EntityId,
        input: PlayerInput,
        seq: InputSeq,
    ) -> bool {
        self.world.set_player_input(entity, input, seq)
    }

    /// Moves a player to `feet_m` at rest (see `SimWorld::teleport_player`).
    pub fn teleport_player(&mut self, entity: EntityId, feet_m: [f64; 3]) -> bool {
        self.world.teleport_player(entity, feet_m, 1.0 / 60.0)
    }

    /// The authoritative kinematic state of a player, if it exists.
    pub fn player_state(&self, entity: EntityId) -> Option<CharacterState> {
        self.world.player(entity).map(|p| p.state)
    }

    /// A player's prediction-invalidation epoch (bumped by a nearby commit).
    pub fn player_movement_epoch(&self, entity: EntityId) -> Option<u64> {
        self.world.player(entity).map(|p| p.movement_epoch)
    }

    /// The last input sequence this simulation accepted for a player.
    pub fn player_acked_input(&self, entity: EntityId) -> Option<InputSeq> {
        self.world.player(entity).map(|p| p.last_input_seq)
    }

    pub fn player_count(&self) -> usize {
        self.world.player_count()
    }

    /// Runs ticks until the pipeline is idle or `max_ticks` is reached. Returns
    /// the per-tick reports.
    pub fn run_until_idle(&mut self, max_ticks: u32) -> Result<Vec<TickReport>, TickError> {
        let mut reports = Vec::new();
        for _ in 0..max_ticks {
            let report = self.tick()?;
            let done = self.pipeline.is_idle();
            reports.push(report);
            if done {
                break;
            }
        }
        Ok(reports)
    }

    /// Steps physics only (no edits), for settling / observation.
    pub fn step_physics_only(&mut self) {
        self.world.step_physics();
    }
}

/// Covers `cells` with a set of modest spheres instead of one big bounding
/// sphere: partitions them into clusters no more than `max_radius_cells`
/// from a shared seed cell (first-remaining-cell order — simple, and correct
/// regardless of `cells`' own order), then covers each cluster with its own
/// tight [`bounding_sphere_brush`]. For a compact blob this converges to the
/// same single sphere `bounding_sphere_brush` alone would give; for a flat,
/// elongated shape (a dam gate's straight notch, not remotely spherical) it
/// keeps every individual cut close to cluster-sized instead of one sphere
/// stretched to the whole shape's own diagonal. Empty for an empty slice.
fn covering_spheres(cells: &[GlobalCell], max_radius_cells: i64) -> Vec<SphereBrush> {
    let mut remaining: Vec<GlobalCell> = cells.to_vec();
    let mut spheres = Vec::new();
    let r2 = max_radius_cells * max_radius_cells;
    while let Some(seed) = remaining.first().copied() {
        let mut cluster = Vec::new();
        remaining.retain(|&c| {
            let d2 = (c.x - seed.x).pow(2) + (c.y - seed.y).pow(2) + (c.z - seed.z).pow(2);
            if d2 <= r2 {
                cluster.push(c);
                false
            } else {
                true
            }
        });
        if let Some(brush) = bounding_sphere_brush(&cluster) {
            spheres.push(brush);
        }
    }
    spheres
}

/// The smallest sphere, centred on `cells`' bounding box, that covers every
/// cell in `cells` (plus one cell of slack so a `Place` on close reliably
/// refills every voxel a matching `Cut` opened). Radius is the true farthest
/// marked cell from that centre, not the bounding box's own half-diagonal —
/// for a compact cluster (what [`covering_spheres`] always passes), that
/// keeps the brush close to the cluster's actual size instead of its
/// bounding box's corner-to-corner distance. `None` for an empty slice.
fn bounding_sphere_brush(cells: &[GlobalCell]) -> Option<SphereBrush> {
    let first = *cells.first()?;
    let (mut lo, mut hi) = (first, first);
    for &c in &cells[1..] {
        lo = GlobalCell::new(lo.x.min(c.x), lo.y.min(c.y), lo.z.min(c.z));
        hi = GlobalCell::new(hi.x.max(c.x), hi.y.max(c.y), hi.z.max(c.z));
    }
    // Cell-centred bounding box: [lo, hi + 1) in whole cells.
    let centre = BrushPoint::from_units(
        (lo.x + hi.x + 1) * BRUSH_UNIT / 2,
        (lo.y + hi.y + 1) * BRUSH_UNIT / 2,
        (lo.z + hi.z + 1) * BRUSH_UNIT / 2,
    );
    let centre_cells = [
        (lo.x + hi.x + 1) as f64 / 2.0,
        (lo.y + hi.y + 1) as f64 / 2.0,
        (lo.z + hi.z + 1) as f64 / 2.0,
    ];
    let radius_cells = cells
        .iter()
        .map(|c| {
            let d = [
                c.x as f64 + 0.5 - centre_cells[0],
                c.y as f64 + 0.5 - centre_cells[1],
                c.z as f64 + 0.5 - centre_cells[2],
            ];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .fold(0.0_f64, f64::max)
        + 1.0;
    let radius_units = (radius_cells * BRUSH_UNIT as f64).ceil() as i64;
    SphereBrush::new(centre, radius_units).ok()
}

/// Converts Rapier's body-0-to-body-1 contact normal to a target-to-striker
/// normal. `target_index` is the target's position in the contact pair.
fn target_to_striker_normal(pair_normal: [f64; 3], target_index: usize) -> [f64; 3] {
    if target_index == 0 {
        pair_normal
    } else {
        pair_normal.map(|component| -component)
    }
}

/// Resolves the solid material just inside a contact surface. Solver points can
/// lie on cell boundaries, so step half a cell against the target-to-striker
/// normal and fail closed when the backing data is not resident.
fn sample_contact_material(
    world: &SimWorld,
    volume_id: spall_core::VolumeId,
    point_cell: [f64; 3],
    normal: [f64; 3],
) -> Option<spall_core::MaterialId> {
    let volume = world.volume_ref(volume_id)?;
    for offset in [0.51, 1.01, 1.51] {
        let cell = GlobalCell::new(
            (point_cell[0] - normal[0] * offset).floor() as i64,
            (point_cell[1] - normal[1] * offset).floor() as i64,
            (point_cell[2] - normal[2] * offset).floor() as i64,
        );
        if let Ok(Sample::Filled(material)) = volume.sample(cell) {
            return Some(material);
        }
    }
    None
}

#[cfg(test)]
mod bounding_sphere_brush_tests {
    use super::bounding_sphere_brush;
    use spall_core::{BRUSH_UNIT, GlobalCell};

    #[test]
    fn empty_cells_has_no_brush() {
        assert!(bounding_sphere_brush(&[]).is_none());
    }

    #[test]
    fn covers_every_marked_cell_with_only_a_little_slack() {
        // A compact, roughly spherical blob (as a scene author would place a
        // gate marker) around (10, 5, 2), radius 3 cells.
        let mut cells = Vec::new();
        for x in -3..=3 {
            for y in -3..=3 {
                for z in -3..=3 {
                    if x * x + y * y + z * z <= 9 {
                        cells.push(GlobalCell::new(10 + x, 5 + y, 2 + z));
                    }
                }
            }
        }
        let brush = bounding_sphere_brush(&cells).expect("non-empty cells");
        for cell in &cells {
            let half = BRUSH_UNIT / 2;
            assert!(
                brush.contains_cell_centre(
                    cell.x * BRUSH_UNIT + half,
                    cell.y * BRUSH_UNIT + half,
                    cell.z * BRUSH_UNIT + half,
                ),
                "{cell:?} must be inside its own covering brush"
            );
        }
        // A blob of authored radius 3 should not need to balloon out to the
        // bounding box's own corner-to-corner reach (radius 3 * sqrt(3) ~= 5.2
        // cells): the fix in `bounding_sphere_brush` keeps it near 3 + 1 slack.
        assert!(
            brush.radius_units() <= 5 * BRUSH_UNIT,
            "brush grew past the authored blob: {} units",
            brush.radius_units()
        );
    }
}

#[cfg(test)]
mod covering_spheres_tests {
    use super::covering_spheres;
    use spall_core::{BRUSH_UNIT, GlobalCell};

    #[test]
    fn empty_cells_has_no_spheres() {
        assert!(covering_spheres(&[], 5).is_empty());
    }

    #[test]
    fn a_flat_wide_notch_is_covered_without_one_giant_sphere() {
        // A dam-gate-shaped notch: wide and tall, but only 8 cells thick —
        // nothing like a sphere. One bounding sphere over this would need
        // a radius of roughly the box's own half-diagonal (~14 cells for a
        // 24x14x8 box); no individual covering sphere here should come
        // anywhere near that.
        let mut cells = Vec::new();
        for x in 0..24 {
            for y in 0..14 {
                for z in 0..8 {
                    cells.push(GlobalCell::new(x, y, z));
                }
            }
        }
        let spheres = covering_spheres(&cells, 5);
        assert!(
            spheres.len() > 1,
            "a shape this size needs more than one sphere"
        );
        for brush in &spheres {
            assert!(
                brush.radius_units() <= 7 * BRUSH_UNIT,
                "a single covering sphere ballooned out to the whole notch: {} units",
                brush.radius_units()
            );
        }
        // Every authored cell must still land inside at least one sphere.
        let half = BRUSH_UNIT / 2;
        for cell in &cells {
            let covered = spheres.iter().any(|brush| {
                brush.contains_cell_centre(
                    cell.x * BRUSH_UNIT + half,
                    cell.y * BRUSH_UNIT + half,
                    cell.z * BRUSH_UNIT + half,
                )
            });
            assert!(covered, "{cell:?} is not covered by any sphere");
        }
    }
}

#[cfg(test)]
mod dam_gate_tests {
    use super::{Simulation, SimulationConfig};
    use crate::fixtures::{STONE, flat_terrain_setup};
    use spall_core::GlobalCell;
    use spall_voxel::Sample;

    /// A dam gate is a server-authored edit through the same commit pipeline
    /// as a player's dig: it stages, then commits a few ticks later, and
    /// closing with the original material exactly undoes opening.
    #[test]
    fn opens_and_closes_through_the_normal_edit_pipeline() {
        let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
        let gate = [GlobalCell::new(10, 0, 10)];
        fn sample(sim: &Simulation) -> Sample {
            sim.world()
                .terrain()
                .volume
                .sample(GlobalCell::new(10, 0, 10))
                .unwrap()
        }
        assert!(matches!(sample(&sim), Sample::Filled(_)), "starts solid");

        sim.set_dam_gate(&gate, true, STONE).unwrap();
        for _ in 0..8 {
            sim.tick().unwrap();
        }
        assert!(matches!(sample(&sim), Sample::Empty { .. }), "open cuts it");

        sim.set_dam_gate(&gate, false, STONE).unwrap();
        for _ in 0..8 {
            sim.tick().unwrap();
        }
        assert!(
            matches!(sample(&sim), Sample::Filled(_)),
            "close refills it"
        );
    }

    /// An empty cell list (a scene with no authored gate) is a harmless no-op,
    /// not a rejected intent.
    #[test]
    fn empty_cells_is_a_no_op() {
        let mut sim = Simulation::new(SimulationConfig::new(flat_terrain_setup())).unwrap();
        sim.set_dam_gate(&[], true, STONE).unwrap();
        for _ in 0..4 {
            sim.tick().unwrap();
        }
    }
}

#[cfg(test)]
mod contact_normal_tests {
    use super::target_to_striker_normal;

    #[test]
    fn contact_pair_order_orients_sampling_into_either_target() {
        let body_zero_to_one = [1.0, 0.0, 0.0];
        // Target in slot 0: the struck material lies in +X, toward striker 1.
        assert_eq!(
            target_to_striker_normal(body_zero_to_one, 0),
            body_zero_to_one
        );
        // Target in slot 1: the struck material lies in -X, toward striker 0.
        assert_eq!(
            target_to_striker_normal(body_zero_to_one, 1),
            [-1.0, 0.0, 0.0]
        );
    }
}

/// Nearest-surface gap, metres, between a world-space AABB `(min, max)` and a
/// sphere. Negative when the sphere overlaps the box. Used to decide whether a
/// terrain cut this tick is close enough to a settled body to wake it.
fn box_sphere_gap(bbox: ([f64; 3], [f64; 3]), centre_m: [f64; 3], radius_m: f64) -> f64 {
    let (lo, hi) = bbox;
    let mut d2 = 0.0;
    for axis in 0..3 {
        let (a, b) = (lo[axis].min(hi[axis]), lo[axis].max(hi[axis]));
        let outside = (a - centre_m[axis]).max(centre_m[axis] - b).max(0.0);
        d2 += outside * outside;
    }
    d2.sqrt() - radius_m
}

#[cfg(test)]
mod retiring_tests {
    use super::Retiring;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_retired_value_is_dropped_off_the_calling_thread_and_finish_waits_for_it() {
        let dropped = Arc::new(AtomicBool::new(false));
        let caller = std::thread::current().id();
        struct Where(Arc<AtomicBool>, std::thread::ThreadId, Arc<AtomicBool>);
        impl Drop for Where {
            fn drop(&mut self) {
                self.2
                    .store(std::thread::current().id() != self.1, Ordering::SeqCst);
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let off_thread = Arc::new(AtomicBool::new(false));
        let mut retiring = Retiring::default();
        retiring.retire(Where(Arc::clone(&dropped), caller, Arc::clone(&off_thread)));
        retiring.finish();
        assert!(dropped.load(Ordering::SeqCst), "finish waits for the drop");
        assert!(
            off_thread.load(Ordering::SeqCst),
            "dropped on another thread"
        );
        // A second value retires cleanly after the first finished.
        let second = Arc::new(AtomicBool::new(false));
        retiring.retire(Flag(Arc::clone(&second)));
        retiring.finish();
        assert!(second.load(Ordering::SeqCst));
    }

    #[test]
    #[should_panic(expected = "drop failed")]
    fn a_panic_while_dropping_is_re_raised_on_the_owner() {
        struct Boom;
        impl Drop for Boom {
            fn drop(&mut self) {
                panic!("drop failed");
            }
        }
        let mut retiring = Retiring::default();
        retiring.retire(Boom);
        retiring.finish();
    }
}

#[cfg(test)]
mod weather_clock_tests {
    use super::*;
    #[test]
    fn no_vegetation_world_uses_tick_clock_and_reset_starts_new_epoch() {
        let mut sim =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        assert_eq!(
            sim.weather_state().clock_source,
            spall_protocol::WeatherClockSource::ServerFallback
        );
        sim.tick().unwrap();
        assert_eq!(sim.weather_state().clock_ms, 16);
        assert_eq!(sim.weather_state().effective_tick, 1);
        let prior_epoch = sim.weather_state().world_epoch;
        let fresh =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        sim.replace_world(fresh).unwrap();
        assert_eq!(sim.weather_state().world_epoch, prior_epoch + 1);
        assert_eq!(sim.weather_state().clock_ms, 0);
        sim.wait_for_retired();
    }

    #[test]
    fn weather_override_survives_ticks_and_reset_clears_it() {
        let mut sim =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        sim.set_weather_override(Some(spall_protocol::WeatherKind::Snow));
        assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
        assert_eq!(
            sim.weather_state().override_kind,
            Some(spall_protocol::WeatherKind::Snow)
        );
        sim.tick().unwrap();
        assert_eq!(sim.weather_state().kind, spall_protocol::WeatherKind::Snow);
        let fresh =
            Simulation::new(SimulationConfig::new(crate::fixtures::flat_terrain_setup())).unwrap();
        sim.replace_world(fresh).unwrap();
        assert_eq!(sim.weather_state().override_kind, None);
        sim.wait_for_retired();
    }
}
