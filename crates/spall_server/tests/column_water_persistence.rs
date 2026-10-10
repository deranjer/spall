//! ENG-167: stacked-column water survives checkpoint, journal and restart.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, SphereBrush, VolumeId,
};
use spall_physics::PhysicsConfig;
use spall_protocol::RequestId;
use spall_server::persist::RecoveryChoice;
use spall_server::{PersistConfig, persist};
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::{EditIntent, EditKind, EditTarget, Simulation, SimulationConfig};
use spall_store::{CrashPoint, FaultPlan, JournalPayload, JournalRecord, Writer};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Volume};

const STONE: MaterialId = MaterialId(1);
const DAM_X: i64 = 20;

fn fixture() -> Simulation {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    for bx in 0..2 {
        terrain
            .insert_brick(
                BrickCoord::new(bx, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
    }
    let mut plan = EditPlan::new(terrain.id());
    for z in 0..16 {
        for y in 0..32 {
            for x in 0..64 {
                let wall = x == 0 || x == 63 || z == 0 || z == 15;
                if y < 2 || (wall && y < 20) || (x == DAM_X && y < 10) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    let mut cells = Vec::new();
    for z in 1..15 {
        for y in 2..9 {
            for x in 1..DAM_X {
                cells.push(GlobalCell::new(x, y, z));
            }
        }
    }
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, cells));
    Simulation::new(config).unwrap()
}

fn brush_at(cell: GlobalCell, radius_cells: i64) -> SphereBrush {
    let half = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            cell.x * BRUSH_UNIT + half,
            cell.y * BRUSH_UNIT + half,
            cell.z * BRUSH_UNIT + half,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

/// The journal batch one server tick owes: committed topology entries, then a
/// column-water delta if one is due (what `serve` appends).
fn batch(sim: &mut Simulation, through: &mut u64, tick: u64) -> Vec<JournalRecord> {
    let mut batch = persist::journal_records(sim.journal().entries_after(*through)).unwrap();
    if let Some(last) = batch.last() {
        *through = last.seq;
    }
    if let Some(delta) = sim.take_column_water_journal() {
        let seq = sim.reserve_journal_seq().unwrap();
        batch.push(JournalRecord {
            seq: seq.0,
            tick,
            payload: JournalPayload::ColumnWaterDelta(delta),
        });
        *through = seq.0;
    }
    batch
}

#[test]
fn column_water_checkpoint_and_journal_survive_restart_and_failed_commit() {
    let dir = std::env::temp_dir().join(format!(
        "spall-column-water-save-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.db");
    let cfg = PersistConfig {
        world_id: 167,
        seed: 1,
        generator_version: 1,
    };
    let mut sim = fixture();
    let cp = persist::capture(&sim, &cfg, 0).unwrap();
    let baseline = cp.column_water.clone().expect("checkpoint carries water");
    let mut writer = Writer::open(&path).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let recovered = writer.recover().unwrap();
    assert_eq!(recovered.checkpoint.column_water, Some(baseline.clone()));
    let materials = sim.world().materials().clone();
    let restore = |recovery: &spall_store::Recovery| {
        persist::restore(
            recovery,
            &cfg,
            RecoveryChoice::RequireClean,
            materials.clone(),
            AnchorPlane::at(0),
            PhysicsConfig::default(),
        )
    };
    let (restored, _) = restore(&recovered).unwrap();
    assert_eq!(
        restored.column_water().unwrap().canonical_state(),
        baseline,
        "checkpoint restores canonical water exactly"
    );
    assert_eq!(
        restored
            .column_water()
            .unwrap()
            .world()
            .active_region_count(),
        0,
        "recovery is at rest"
    );

    // Breach the dam, journal every tick as the server does, until at rest.
    sim.submit(EditIntent {
        request_id: RequestId(2),
        actor: EntityId::new(1).unwrap(),
        target: EditTarget::Terrain,
        kind: EditKind::Cut,
        brush: brush_at(GlobalCell::new(DAM_X, 5, 7), 3),
        explosion: None,
    })
    .unwrap();
    let mut through = 0;
    let mut appended = 0;
    let mut quiet_after = 0;
    for _ in 0..(60 * 400) {
        let report = sim.tick().unwrap();
        let tick = sim.current_tick().get();
        let records = batch(&mut sim, &mut through, tick);
        appended += records.len();
        if !records.is_empty() {
            writer.append_journal(&records).unwrap();
        }
        let m = report.column_water.unwrap();
        if m.active_regions == 0 && tick > 120 {
            quiet_after += 1;
            if quiet_after > 40 {
                break; // at rest and the last periodic delta has flushed
            }
        }
    }
    assert!(appended > 2, "topology and water records were journaled");
    let live = sim.column_water().unwrap().canonical_state();

    // A commit that fails before it is durable is not recovered.
    writer.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
    assert!(
        writer
            .append_journal(&[JournalRecord {
                seq: through + 1,
                tick: sim.current_tick().get() + 1,
                payload: JournalPayload::ColumnWaterDelta(spall_protocol::ColumnWaterDelta {
                    frame_seq: live.frame_seq + 1,
                    fluid_time_fixed: live.fluid_time_fixed + 1,
                    spring_added_fixed: live.spring_added_fixed,
                    drain_removed_fixed: live.drain_removed_fixed,
                    unplaced_fixed: live.unplaced_fixed,
                    gated_rate: 0,
                    regions: Vec::new(),
                }),
            }])
            .is_err()
    );
    drop(writer);
    let recovery = spall_store::recover(&path).unwrap();
    let (restored, seq) = restore(&recovery).unwrap();
    assert_eq!(seq, through);
    let recovered_state = restored.column_water().unwrap().canonical_state();
    assert_eq!(
        recovered_state.regions, live.regions,
        "journaled water equals the live water at rest"
    );
    assert_eq!(recovered_state.frame_seq, live.frame_seq);
    assert_eq!(restored.world().world_hash(), sim.world().world_hash());
    assert!(
        (restored.column_water().unwrap().frame().volume_m3
            - sim.column_water().unwrap().frame().volume_m3)
            .abs()
            < 1e-9
    );
    drop(restored);
    std::fs::remove_dir_all(dir).unwrap();
}
