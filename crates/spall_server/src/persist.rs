//! Save integration (T16): the authoritative-state ⇄ `spall_store` save-record
//! conversion, plus recovery that rebuilds a [`spall_sim::Simulation`].
//!
//! This lives in the integrator, not `spall_sim`, so the vendored SQLite build
//! that `spall_store` carries never enters the pure simulation crate
//! (`docs/architecture.md` "Persistence and recovery").
//!
//! What is journalled: committed [`spall_protocol::TopologyTransaction`]s only,
//! keyed by the simulation's own `JournalSeq`, each carrying its participant
//! body snapshots. Periodic 20 Hz pose-batch journaling is deferred; a crash
//! rewinds body motion to the last checkpoint / last topology-transaction
//! participant state, which `docs/protocol.md` permits ("A crash can lose the
//! unflushed suffix and rewind motion to the latest durable pose batch").

use glam::DQuat;
use spall_core::{
    BrickCoord, CELLS_PER_BRICK, CellSizeCode, EntityId, LocalCell, MaterialId, MaterialManifest,
    Revision, Tick, VolumeId,
};
use spall_physics::PhysicsConfig;
use spall_protocol::{TopologyOp, content_manifest_hash};
use spall_sim::{
    Body, BodyPose, JournalEntry, RestoredBody, SimWorld, Simulation, WorldSetup, solid_cells,
};
use spall_store::{
    BrickCodecError, Checkpoint, DtoError, JournalPayload, JournalRecord, Recovery,
    STORE_SCHEMA_VERSION, StoreError, StoredBody, StoredBodyKind, StoredBrick, StoredPose,
    StoredWorldMeta, decode_cells, encode_cells,
};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, Volume};

/// Algorithm versions stamped into saved world metadata (mirrors
/// `spall_sim::commit::ALGORITHM_VERSION` and the T01/T07 versions).
pub const INTEGER_BRUSH_VERSION: u32 = 1;
pub const STRUCTURE_GRAPH_VERSION: u32 = 1;
pub const TOPOLOGY_HASH_VERSION: u32 = 1;

/// Identity a world save is stamped with. `anchor` and `physics` are redeployment
/// config, re-supplied on restart exactly like `spall_sim::WorldSetup` treats
/// them.
#[derive(Debug, Clone)]
pub struct PersistConfig {
    pub world_id: u128,
    pub seed: u64,
    pub generator_version: u32,
}

/// Anything that can go wrong converting to/from save records.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("save DTO: {0}")]
    Dto(#[from] DtoError),
    #[error("brick codec: {0}")]
    Brick(#[from] BrickCodecError),
    #[error("world rebuild: {0}")]
    World(#[from] spall_sim::WorldError),
    #[error("id space: {0}")]
    Ids(#[from] spall_core::IdError),
    #[error("checkpoint has no terrain body")]
    NoTerrain,
    #[error("checkpoint field invalid: {0}")]
    BadField(&'static str),
    #[error("unknown cell-size code {0}")]
    BadCellSize(u8),
    #[error("material manifest hash mismatch: checkpoint {checkpoint:.16}, runtime {runtime:.16}")]
    ManifestMismatch { checkpoint: String, runtime: String },
    #[error(
        "checkpoint world-hash mismatch: rebuilt {rebuilt:.16}, saved canonical {checkpoint:.16} \
         (the checkpoint rows decoded but do not reproduce the state they claim — refusing to \
         publish it as authoritative)"
    )]
    CheckpointHashMismatch { checkpoint: String, rebuilt: String },
    #[error(
        "save schema version mismatch: checkpoint {checkpoint}, runtime {runtime} \
         (a schema migration is required — do not replay this database in place)"
    )]
    SchemaMismatch { checkpoint: u32, runtime: u32 },
    #[error(
        "world identity mismatch: checkpoint world_id {checkpoint:#034x}, configured {runtime:#034x} \
         (this database belongs to a different world — refusing to replay it as the current one)"
    )]
    WorldIdMismatch { checkpoint: u128, runtime: u128 },
    #[error(
        "world config mismatch on {field}: checkpoint {checkpoint}, configured {runtime} \
         (an expected configuration migration — perform it explicitly on a separate, \
         backed-up and verified copy, never as an in-place recovery)"
    )]
    ConfigMismatch {
        field: &'static str,
        checkpoint: u64,
        runtime: u64,
    },
    #[error(
        "algorithm version mismatch on {field}: checkpoint {checkpoint}, runtime {runtime} \
         (this build cannot interpret the saved checkpoint/journal — upgrade or migrate explicitly)"
    )]
    AlgorithmVersionMismatch {
        field: &'static str,
        checkpoint: u32,
        runtime: u32,
    },
}

// --- capture --------------------------------------------------------------

/// Snapshots the live authoritative world into an immutable [`Checkpoint`]
/// consistent with journal `journal_cursor` (the highest durable `JournalSeq`).
pub fn capture(
    sim: &Simulation,
    cfg: &PersistConfig,
    journal_cursor: u64,
) -> Result<Checkpoint, PersistError> {
    let world = sim.world();
    let (next_entity, next_volume, next_transaction, next_journal_seq) =
        world.registry().counters();

    let mut bodies = Vec::new();
    let mut bricks = Vec::new();
    let mut cell_sizes = std::collections::BTreeSet::new();

    let terrain = world.terrain();
    cell_sizes.insert(terrain.volume.cell_size().to_u8());
    bodies.push(stored_body(world, terrain, StoredBodyKind::Terrain));
    bricks.extend(stored_bricks(&terrain.volume)?);

    for body in world.bodies() {
        cell_sizes.insert(body.volume.cell_size().to_u8());
        bodies.push(stored_body(world, body, StoredBodyKind::Dynamic));
        bricks.extend(stored_bricks(&body.volume)?);
    }

    let meta = StoredWorldMeta {
        store_schema_version: STORE_SCHEMA_VERSION,
        world_id: cfg.world_id,
        seed: cfg.seed,
        generator_version: cfg.generator_version,
        material_manifest_hash: content_manifest_hash(world.materials()).0,
        cell_size_codes: cell_sizes.into_iter().collect(),
        next_entity,
        next_volume,
        next_transaction,
        next_journal_seq,
        integer_brush_version: INTEGER_BRUSH_VERSION,
        structure_graph_version: STRUCTURE_GRAPH_VERSION,
        topology_hash_version: TOPOLOGY_HASH_VERSION,
    };

    Ok(Checkpoint {
        tick: sim.current_tick().get(),
        journal_cursor,
        world_hash: world.world_hash().0,
        meta,
        bodies,
        bricks,
    })
}

fn stored_body(world: &SimWorld, body: &Body, kind: StoredBodyKind) -> StoredBody {
    let cell_m = body.volume.cell_size().metres();
    let density = match kind {
        StoredBodyKind::Terrain => 1.0,
        StoredBodyKind::Dynamic => {
            let (mass_kg, _, _) = world.physics().derived_mass_properties(body.phys);
            let vol_m3 = solid_cells(&body.volume) as f64 * cell_m.powi(3);
            if vol_m3 > 0.0 {
                (f64::from(mass_kg) / vol_m3) as f32
            } else {
                1.0
            }
        }
    };
    let r = body.pose.rotation;
    let (lo, hi) = body.collider_region;
    StoredBody {
        entity_id: body.entity.map(|e| e.get()).unwrap_or(0),
        volume_id: body.volume_id.get(),
        kind,
        cell_size_code: body.volume.cell_size().to_u8(),
        pose: StoredPose {
            translation_m: body.pose.translation_m,
            rotation_xyzw: [r.x, r.y, r.z, r.w],
        },
        linvel_m_s: body.linvel_m_s,
        angvel_rad_s: body.angvel_rad_s,
        sleeping: body.sleeping,
        collider_revision: body.collider_revision,
        coarsen_k: body.coarsen_k,
        collider_region: [[lo.x, lo.y, lo.z], [hi.x, hi.y, hi.z]],
        density_kg_m3: density,
        volume_bounds: body
            .volume
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
    }
}

fn stored_bricks(v: &Volume) -> Result<Vec<StoredBrick>, PersistError> {
    let mut out = Vec::new();
    for coord in v.resident_brick_coords() {
        let snap = v
            .snapshot_brick(coord)
            .ok()
            .flatten()
            .expect("coord came from the resident set");
        let mut cells = vec![0u16; CELLS_PER_BRICK];
        for (i, slot) in cells.iter_mut().enumerate() {
            let local = LocalCell::from_linear_index(i as u16).expect("i < CELLS_PER_BRICK");
            *slot = snap.get(local).raw();
        }
        out.push(StoredBrick {
            volume_id: v.id().get(),
            coord: [coord.x, coord.y, coord.z],
            revision: snap.revision().get(),
            edited: snap.is_edited(),
            payload: encode_cells(&cells)?,
        });
    }
    Ok(out)
}

// --- journal records ----------------------------------------------------

/// Converts simulation journal entries to durable [`JournalRecord`]s (topology
/// only; seq is the simulation's `JournalSeq`).
pub fn journal_records(entries: &[JournalEntry]) -> Result<Vec<JournalRecord>, PersistError> {
    entries
        .iter()
        .map(|e| {
            Ok(JournalRecord {
                seq: e.seq.0,
                tick: e.transaction.server_tick.get(),
                payload: JournalPayload::topology(&e.transaction, &e.participants)?,
            })
        })
        .collect()
}

// --- restore ----------------------------------------------------------

/// Rebuilds a [`Simulation`] from a [`Recovery`]: the checkpoint world, then the
/// durable journal suffix replayed onto it. Returns the simulation and the
/// highest durable `JournalSeq` (the resume point for further journalling).
///
/// `cfg` is the configured world identity the host is resuming; `materials`,
/// `anchor`, and `physics` are redeployment config. Before any state is rebuilt
/// the saved [`StoredWorldMeta`] is validated against `cfg` and this build:
/// schema version, world identity, seed/generator, every structural algorithm
/// version, the material-manifest hash and the cell-size codes must all match.
/// A mismatch is reported and nothing is rebuilt or written — an intentional
/// migration is a deliberate, separate, backed-up operation, not an in-place
/// recovery ([`PersistError::ConfigMismatch`] / [`PersistError::WorldIdMismatch`]
/// / [`PersistError::AlgorithmVersionMismatch`]).
pub fn restore(
    recovery: &Recovery,
    cfg: &PersistConfig,
    materials: MaterialManifest,
    anchor: AnchorPlane,
    physics: PhysicsConfig,
) -> Result<(Simulation, u64), PersistError> {
    let cp = &recovery.checkpoint;

    validate_world_meta(&cp.meta, cfg)?;

    let runtime_hash = content_manifest_hash(&materials).0;
    if runtime_hash != cp.meta.material_manifest_hash {
        return Err(PersistError::ManifestMismatch {
            checkpoint: hex32(&cp.meta.material_manifest_hash),
            runtime: hex32(&runtime_hash),
        });
    }

    let terrain_sb = cp
        .bodies
        .iter()
        .find(|b| matches!(b.kind, StoredBodyKind::Terrain))
        .ok_or(PersistError::NoTerrain)?;
    let terrain_cs = CellSizeCode::from_u8(terrain_sb.cell_size_code)
        .ok_or(PersistError::BadCellSize(terrain_sb.cell_size_code))?;
    let terrain_vol = rebuild_volume(
        terrain_sb.volume_id,
        terrain_cs,
        terrain_sb.volume_bounds,
        &cp.bricks,
    )?;

    let mut world = SimWorld::new(WorldSetup {
        terrain: terrain_vol,
        terrain_collider_region: region_from(terrain_sb.collider_region),
        materials,
        anchor,
        physics,
    })?;

    for sb in cp
        .bodies
        .iter()
        .filter(|b| matches!(b.kind, StoredBodyKind::Dynamic))
    {
        let cs = CellSizeCode::from_u8(sb.cell_size_code)
            .ok_or(PersistError::BadCellSize(sb.cell_size_code))?;
        let volume = rebuild_volume(sb.volume_id, cs, sb.volume_bounds, &cp.bricks)?;
        let entity =
            EntityId::new(sb.entity_id).map_err(|_| PersistError::BadField("body entity_id"))?;
        world.insert_restored_body(RestoredBody {
            entity,
            volume,
            pose: pose_from_stored(&sb.pose),
            linvel_m_s: sb.linvel_m_s,
            angvel_rad_s: sb.angvel_rad_s,
            sleeping: sb.sleeping,
            collider_revision: sb.collider_revision,
            collider_region: region_from(sb.collider_region),
            density_kg_m3: sb.density_kg_m3,
        })?;
    }

    // Ids resume at the checkpoint counters; the suffix replay then consumes
    // forward from there.
    world.resume_registry(
        cp.meta.next_entity,
        cp.meta.next_volume,
        cp.meta.next_transaction,
        cp.meta.next_journal_seq,
    )?;

    // The rebuilt checkpoint world — every body, brick, revision and owner — must
    // reproduce the canonical hash the checkpoint was published with before any
    // journal record is replayed onto it. A decodable row corruption (a flipped
    // tombstone bit, a swapped revision) is caught here.
    let rebuilt = world.world_hash().0;
    if rebuilt != cp.world_hash {
        return Err(PersistError::CheckpointHashMismatch {
            checkpoint: hex32(&cp.world_hash),
            rebuilt: hex32(&rebuilt),
        });
    }

    let mut max_tx = cp.meta.next_transaction.saturating_sub(1);
    let mut max_entity = cp.meta.next_entity.saturating_sub(1);
    let mut max_volume = cp.meta.next_volume.saturating_sub(1);
    let mut last_seq = cp.journal_cursor;

    for record in &recovery.journal {
        match &record.payload {
            JournalPayload::Topology { .. } => {
                let (tx, participants) =
                    record.payload.as_topology().expect("payload is Topology")?;
                if tx.algorithm_version != INTEGER_BRUSH_VERSION {
                    return Err(PersistError::AlgorithmVersionMismatch {
                        field: "journal transaction.algorithm_version",
                        checkpoint: tx.algorithm_version,
                        runtime: INTEGER_BRUSH_VERSION,
                    });
                }
                world.replay_transaction(&tx, &participants)?;
                max_tx = max_tx.max(tx.transaction_id.get());
                for op in &tx.ops {
                    if let TopologyOp::SplitOff {
                        child,
                        child_entity,
                        ..
                    } = op
                    {
                        max_entity = max_entity.max(child_entity.get());
                        max_volume = max_volume.max(child.get());
                    }
                }
            }
            JournalPayload::PoseBatch { .. } => {
                let snaps = record
                    .payload
                    .as_pose_batch()
                    .expect("payload is PoseBatch")?;
                world.apply_pose_batch(&snaps);
            }
        }
        last_seq = record.seq;
    }

    world.resume_registry(max_entity + 1, max_volume + 1, max_tx + 1, last_seq + 1)?;

    Ok((Simulation::from_restored(world, Tick(cp.tick)), last_seq))
}

/// Validates the saved world metadata against the configured world identity and
/// this build's algorithm/schema versions *before* any checkpoint or journal
/// state is interpreted. Every branch returns without touching the database.
fn validate_world_meta(meta: &StoredWorldMeta, cfg: &PersistConfig) -> Result<(), PersistError> {
    if meta.store_schema_version != STORE_SCHEMA_VERSION {
        return Err(PersistError::SchemaMismatch {
            checkpoint: meta.store_schema_version,
            runtime: STORE_SCHEMA_VERSION,
        });
    }
    // Wrong-database: identity can never be "migrated", only pointed at correctly.
    if meta.world_id != cfg.world_id {
        return Err(PersistError::WorldIdMismatch {
            checkpoint: meta.world_id,
            runtime: cfg.world_id,
        });
    }
    // Generation config: a real change is an intentional, separate migration.
    if meta.seed != cfg.seed {
        return Err(PersistError::ConfigMismatch {
            field: "seed",
            checkpoint: meta.seed,
            runtime: cfg.seed,
        });
    }
    if meta.generator_version != cfg.generator_version {
        return Err(PersistError::ConfigMismatch {
            field: "generator_version",
            checkpoint: u64::from(meta.generator_version),
            runtime: u64::from(cfg.generator_version),
        });
    }
    // Every algorithm version needed to interpret the checkpoint/journal.
    for (field, checkpoint, runtime) in [
        (
            "integer_brush_version",
            meta.integer_brush_version,
            INTEGER_BRUSH_VERSION,
        ),
        (
            "structure_graph_version",
            meta.structure_graph_version,
            STRUCTURE_GRAPH_VERSION,
        ),
        (
            "topology_hash_version",
            meta.topology_hash_version,
            TOPOLOGY_HASH_VERSION,
        ),
    ] {
        if checkpoint != runtime {
            return Err(PersistError::AlgorithmVersionMismatch {
                field,
                checkpoint,
                runtime,
            });
        }
    }
    // Every cell-size code the checkpoint/journal references must be known to
    // this build before a volume is rebuilt with it.
    for &code in &meta.cell_size_codes {
        if CellSizeCode::from_u8(code).is_none() {
            return Err(PersistError::BadCellSize(code));
        }
    }
    Ok(())
}

fn rebuild_volume(
    volume_id: u64,
    cell_size: CellSizeCode,
    bounds: Option<[[i64; 3]; 2]>,
    all_bricks: &[StoredBrick],
) -> Result<Volume, PersistError> {
    let vid = VolumeId::new(volume_id).map_err(|_| PersistError::BadField("volume_id"))?;
    let mut volume = match bounds {
        Some([mn, mx]) => {
            let bb = BrickBounds::new(
                BrickCoord::new(mn[0], mn[1], mn[2]),
                BrickCoord::new(mx[0], mx[1], mx[2]),
            )
            .ok_or(PersistError::BadField("volume_bounds"))?;
            Volume::bounded(vid, cell_size, bb)
        }
        None => Volume::new(vid, cell_size),
    };
    for sb in all_bricks.iter().filter(|b| b.volume_id == volume_id) {
        let cells: Vec<MaterialId> = decode_cells(&sb.payload)?
            .into_iter()
            .map(MaterialId)
            .collect();
        let brick = Brick::restored(&cells, Revision(sb.revision), sb.edited);
        volume
            .insert_brick(
                BrickCoord::new(sb.coord[0], sb.coord[1], sb.coord[2]),
                brick,
            )
            .map_err(|_| PersistError::BadField("brick coord outside volume bounds"))?;
    }
    Ok(volume)
}

fn pose_from_stored(p: &StoredPose) -> BodyPose {
    BodyPose::new(
        DQuat::from_xyzw(
            p.rotation_xyzw[0],
            p.rotation_xyzw[1],
            p.rotation_xyzw[2],
            p.rotation_xyzw[3],
        ),
        p.translation_m,
    )
}

fn region_from(r: [[i64; 3]; 2]) -> (spall_core::GlobalCell, spall_core::GlobalCell) {
    (
        spall_core::GlobalCell::new(r[0][0], r[0][1], r[0][2]),
        spall_core::GlobalCell::new(r[1][0], r[1][1], r[1][2]),
    )
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// --- crash suite (`cargo xtask crash-test --suite persistence`) -----------

use serde::Serialize;
use spall_store::{CrashPoint, FaultPlan};

/// One crash / disk-fault scenario outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Machine-readable persistence crash-suite report.
#[derive(Debug, Clone, Serialize)]
pub struct CrashSuiteReport {
    pub version: u32,
    pub result: String,
    pub scenarios: Vec<ScenarioResult>,
    pub workload: Workload,
    pub metrics: Metrics,
}

/// Workload the scenarios actually drove.
#[derive(Debug, Clone, Serialize)]
pub struct Workload {
    pub bodies_after_cut: usize,
    pub checkpoint_bricks: usize,
    pub solid_cells: u64,
    pub journal_records: usize,
}

/// Measured durable-write throughput from the clean baseline writer.
#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub journal_commits: u64,
    pub journal_records: u64,
    pub bytes_per_journal_write: f64,
    pub checkpoint_publishes: u64,
    pub checkpoint_payload_bytes: u64,
    pub commit_bytes_per_sec: f64,
    pub max_wal_bytes: u64,
}

fn scripted_bridge_cut() -> Simulation {
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{EntityId, SphereBrush};
    use spall_sim::{EditIntent, EditTarget, SimulationConfig, fixtures};

    let mut sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup()))
        .expect("bridge scene is valid");
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(10 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
        2 * BRUSH_UNIT,
    )
    .expect("brush is valid");
    sim.submit(EditIntent::cut(
        spall_protocol::RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    ))
    .expect("submit");
    sim.run_until_idle(24).expect("run to idle");
    sim
}

/// Runs the persistence crash-point / disk-fault matrix end-to-end through a
/// real [`Simulation`] (bridge scene, column cut → beam detaches) and asserts
/// the durable prefix after recovery. Writes nothing itself; the caller
/// serialises the returned report to `summary.json`.
pub fn run_crash_suite(scratch_dir: &std::path::Path) -> Result<CrashSuiteReport, PersistError> {
    use spall_sim::{SimulationConfig, fixtures};

    let cfg = PersistConfig {
        world_id: 0x5A11_0000_0000_7016,
        seed: 16,
        generator_version: 1,
    };
    std::fs::create_dir_all(scratch_dir).ok();

    let scripted = scripted_bridge_cut();
    let post_cut_hash = scripted.world().world_hash();
    let post_cut_bodies = scripted.world().body_count();
    let post_cut_solid = scripted.world().total_solid_cells();
    let journal = journal_records(scripted.journal().entries())?;
    let durable_seq = journal.last().map(|r| r.seq).unwrap_or(0);

    let manifest = fixtures::stone_manifest();
    let anchor = AnchorPlane::at(0);

    let fresh = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let checkpoint0 = capture(&fresh, &cfg, 0)?;
    let checkpoint1 = capture(&scripted, &cfg, durable_seq)?;
    let checkpoint_bricks = checkpoint1.bricks.len();
    let pre_cut_bodies = 0usize;

    let mut scenarios = Vec::new();
    let mut idx = 0u64;
    let mut next_db = || {
        idx += 1;
        scratch_dir.join(format!("scenario-{idx}.db"))
    };

    // 1. Clean baseline — also the metrics source.
    let metrics;
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.append_journal(&journal)?;
        w.publish_checkpoint(&checkpoint1)?;
        let m = w.metrics().clone();
        metrics = Metrics {
            journal_commits: m.journal_commits,
            journal_records: m.journal_records,
            bytes_per_journal_write: m.bytes_per_journal_write(),
            checkpoint_publishes: m.checkpoint_publishes,
            checkpoint_payload_bytes: m.checkpoint_payload_bytes,
            commit_bytes_per_sec: m.commit_bytes_per_sec(),
            max_wal_bytes: m.max_wal_bytes,
        };
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, seq) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "clean",
            sim.world().world_hash() == post_cut_hash
                && sim.world().body_count() == post_cut_bodies
                && seq == durable_seq
                && rec.corruption.is_empty(),
            format!(
                "hash_ok={}, bodies={}, durable_seq={seq}",
                sim.world().world_hash() == post_cut_hash,
                sim.world().body_count()
            ),
        ));
    }

    // 2. Crash right after the journal commit — the split is durable anyway.
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.set_faults(FaultPlan::crash(CrashPoint::AfterJournalCommit));
        let crashed = w.append_journal(&journal).is_err();
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, _) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "crash_after_journal_commit",
            crashed
                && rec.journal.len() == journal.len()
                && sim.world().world_hash() == post_cut_hash
                && sim.world().body_count() == post_cut_bodies,
            format!(
                "journal_suffix={}, bodies={}",
                rec.journal.len(),
                sim.world().body_count()
            ),
        ));
    }

    // 3. Crash before the journal commit — the edit is simply not durable.
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
        let crashed = w.append_journal(&journal).is_err();
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, seq) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "crash_before_journal_commit",
            crashed
                && rec.journal.is_empty()
                && rec.corruption.is_empty()
                && sim.world().body_count() == pre_cut_bodies
                && seq == 0,
            format!(
                "journal_suffix={}, bodies={}",
                rec.journal.len(),
                sim.world().body_count()
            ),
        ));
    }

    // 4. Crash before the second checkpoint commits — partial checkpoint stays
    //    invisible, the journal still recovers the split.
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.append_journal(&journal)?;
        w.set_faults(FaultPlan::crash(CrashPoint::BeforeCheckpointCommit));
        let crashed = w.publish_checkpoint(&checkpoint1).is_err();
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, _) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "crash_before_checkpoint_commit",
            crashed
                && rec.checkpoint.tick == checkpoint0.tick
                && rec.journal.len() == journal.len()
                && sim.world().world_hash() == post_cut_hash
                && sim.world().body_count() == post_cut_bodies,
            format!(
                "recovered_cp_tick={}, journal_suffix={}, bodies={}",
                rec.checkpoint.tick,
                rec.journal.len(),
                sim.world().body_count()
            ),
        ));
    }

    // 5. Disk fault on the journal commit — no false success, clean rollback.
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.set_faults(FaultPlan::disk_fail_journal());
        let failed = matches!(
            w.append_journal(&journal),
            Err(spall_store::StoreError::Disk(_))
        );
        let poisoned = w.is_poisoned();
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, _) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "disk_fault_on_journal",
            failed
                && poisoned
                && rec.journal.is_empty()
                && rec.corruption.is_empty()
                && sim.world().body_count() == pre_cut_bodies,
            format!(
                "failed={failed}, poisoned={poisoned}, bodies={}",
                sim.world().body_count()
            ),
        ));
    }

    // 6. Disk fault on the second checkpoint — falls back to cp@0 + journal.
    {
        let db = next_db();
        let mut w = spall_store::Writer::open(&db)?;
        w.publish_checkpoint(&checkpoint0)?;
        w.append_journal(&journal)?;
        w.set_faults(FaultPlan::disk_fail_checkpoint());
        let failed = matches!(
            w.publish_checkpoint(&checkpoint1),
            Err(spall_store::StoreError::Disk(_))
        );
        drop(w);
        let rec = spall_store::recover(&db)?;
        let (sim, _) = restore(
            &rec,
            &cfg,
            manifest.clone(),
            anchor,
            PhysicsConfig::default(),
        )?;
        scenarios.push(check(
            "disk_fault_on_checkpoint",
            failed
                && rec.checkpoint.tick == checkpoint0.tick
                && rec.journal.len() == journal.len()
                && sim.world().world_hash() == post_cut_hash,
            format!(
                "failed={failed}, recovered_cp_tick={}, bodies={}",
                rec.checkpoint.tick,
                sim.world().body_count()
            ),
        ));
    }

    let all_passed = scenarios.iter().all(|s| s.passed);
    Ok(CrashSuiteReport {
        version: 1,
        result: if all_passed { "passed" } else { "failed" }.to_string(),
        scenarios,
        workload: Workload {
            bodies_after_cut: post_cut_bodies,
            checkpoint_bricks,
            solid_cells: post_cut_solid,
            journal_records: journal.len(),
        },
        metrics,
    })
}

fn check(name: &str, passed: bool, detail: String) -> ScenarioResult {
    ScenarioResult {
        name: name.to_string(),
        passed,
        detail,
    }
}
