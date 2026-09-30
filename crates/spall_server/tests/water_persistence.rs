use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::GridReservoirFixture;
use spall_physics::PhysicsConfig;
use spall_server::persist::RecoveryChoice;
use spall_server::{PersistConfig, persist};
use spall_sim::{Simulation, SimulationConfig, WaterSetup};
use spall_store::{CrashPoint, FaultPlan, JournalPayload, JournalRecord, Writer};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

fn fixture() -> Simulation {
    let fixture = GridReservoirFixture::new(1, false).unwrap();
    let spec = fixture.grid().spec();
    let d = spec.dimensions();
    let mut seeds = Vec::new();
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    terrain
        .insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
    let mut solid = EditPlan::new(terrain.id());
    for z in 0..i64::from(d[2]) {
        for y in 0..i64::from(d[1]) {
            for x in 0..i64::from(d[0]) {
                let cell = GlobalCell::new(x, y, z);
                if let Some(f) = fixture.grid().fraction_at(cell)
                    && f > 0.0
                {
                    seeds.push((cell, f));
                }
                if let Sample::Filled(m) = fixture.volume().sample(cell).unwrap() {
                    solid.set(cell, m);
                }
            }
        }
    }
    terrain.apply_edit(&solid).unwrap();
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (
        spec.origin(),
        GlobalCell::new(
            i64::from(d[0]) - 1,
            i64::from(d[1]) - 1,
            i64::from(d[2]) - 1,
        ),
    );
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.water = Some(WaterSetup::new(spec, seeds));
    Simulation::new(config).unwrap()
}

#[test]
fn exact_water_checkpoint_and_journal_survive_restart_and_failed_commit() {
    let dir = std::env::temp_dir().join(format!(
        "spall-water-save-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.db");
    let cfg = PersistConfig {
        world_id: 105,
        seed: 1,
        generator_version: 1,
    };
    let mut sim = fixture();
    let mut saved = sim.water().unwrap().canonical_state();
    saved.trapped[0] = 0.375_f64.to_bits();
    sim.restore_water_regions(std::slice::from_ref(&saved))
        .unwrap();
    let cp = persist::capture(&sim, &cfg, 0).unwrap();
    let mut writer = Writer::open(&path).unwrap();
    writer.publish_checkpoint(&cp).unwrap();
    let recovered = writer.recover().unwrap();
    assert_eq!(recovered.checkpoint.water, vec![saved.clone()]);
    let (restored, _) = persist::restore(
        &recovered,
        &cfg,
        RecoveryChoice::RequireClean,
        sim.world().materials().clone(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    assert_eq!(restored.water().unwrap().canonical_state(), saved);
    assert_eq!(
        restored
            .water()
            .unwrap()
            .grid()
            .unwrap()
            .max_face_component_velocity_m_s(),
        0.0
    );

    sim.tick().unwrap();
    let durable = sim.water().unwrap().canonical_state();
    durable.validate().unwrap();
    writer
        .append_journal(&[JournalRecord {
            seq: 1,
            tick: sim.current_tick().get(),
            payload: JournalPayload::WaterState(vec![durable.clone()]),
        }])
        .unwrap();
    sim.tick().unwrap();
    writer.set_faults(FaultPlan::crash(CrashPoint::BeforeJournalCommit));
    assert!(
        writer
            .append_journal(&[JournalRecord {
                seq: 2,
                tick: sim.current_tick().get(),
                payload: JournalPayload::WaterState(vec![sim.water().unwrap().canonical_state()])
            }])
            .is_err()
    );
    drop(writer);
    let recovery = spall_store::recover(&path).unwrap();
    let (restored, seq) = persist::restore(
        &recovery,
        &cfg,
        RecoveryChoice::RequireClean,
        sim.world().materials().clone(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    assert_eq!(seq, 1);
    assert_eq!(restored.water().unwrap().canonical_state(), durable);
    assert_eq!(restored.world().world_hash(), sim.world().world_hash());
    drop(restored);
    std::fs::remove_dir_all(dir).unwrap();
}
