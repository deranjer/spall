//! ENG-268: winter ice is terrain, so it survives the journal and a restart.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_physics::PhysicsConfig;
use spall_server::persist::RecoveryChoice;
use spall_server::{PersistConfig, persist};
use spall_sim::column_water::ColumnWaterSetup;
use spall_sim::{IceSettings, Simulation, SimulationConfig};
use spall_store::{JournalPayload, JournalRecord, Writer};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

const STONE: MaterialId = MaterialId(1);
const ICE: MaterialId = MaterialId(2);
const SURFACE: i64 = 7;

/// A walled 32 x 16 pool, 0.75 m deep, surface at the top of cell 6.
fn fixture() -> Simulation {
    let mut terrain = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    terrain
        .insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
    let mut plan = EditPlan::new(terrain.id());
    let mut water = Vec::new();
    for z in 0..16 {
        for x in 0..32 {
            let wall = x == 0 || x == 31 || z == 0 || z == 15;
            for y in 0..32 {
                if y < 4 || (wall && y < 12) {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                } else if !wall && y < SURFACE {
                    water.push(GlobalCell::new(x, y, z));
                }
            }
        }
    }
    terrain.apply_edit(&plan).unwrap();
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(31, 31, 15));
    world.anchor = AnchorPlane::at(0);
    let mut config = SimulationConfig::new(world);
    config.column_water = Some(ColumnWaterSetup::new(0, 32, water));
    let mut sim = Simulation::new(config).unwrap();
    let mut ice = IceSettings::new(ICE, ICE);
    ice.settle_s = 0.2;
    sim.set_ice(ice);
    sim
}

fn ice_cells(sim: &Simulation) -> usize {
    let volume = &sim.world().terrain().volume;
    (1..31)
        .flat_map(|x| (1..15).map(move |z| GlobalCell::new(x, SURFACE, z)))
        .filter(|&c| volume.sample(c) == Ok(Sample::Filled(ICE)))
        .count()
}

#[test]
fn ice_is_journaled_and_recovered_like_any_terrain() {
    let dir = std::env::temp_dir().join(format!(
        "spall-ice-save-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.db");
    let cfg = PersistConfig {
        world_id: 268,
        seed: 1,
        generator_version: 1,
    };
    let mut sim = fixture();
    let mut writer = Writer::open(&path).unwrap();
    writer
        .publish_checkpoint(&persist::capture(&sim, &cfg, 0).unwrap())
        .unwrap();
    sim.set_season(3).unwrap();
    let mut through = 0;
    for _ in 0..600 {
        sim.tick().unwrap();
        let tick = sim.current_tick().get();
        let mut records = persist::journal_records(sim.journal().entries_after(through)).unwrap();
        if let Some(last) = records.last() {
            through = last.seq;
        }
        if let Some(delta) = sim.take_column_water_journal() {
            let seq = sim.reserve_journal_seq().unwrap();
            records.push(JournalRecord {
                seq: seq.0,
                tick,
                payload: JournalPayload::ColumnWaterDelta(delta),
            });
            through = seq.0;
        }
        if !records.is_empty() {
            writer.append_journal(&records).unwrap();
        }
        if ice_cells(&sim) == 30 * 14 && sim.is_idle() {
            break;
        }
    }
    assert_eq!(ice_cells(&sim), 30 * 14, "the pool froze");
    drop(writer);
    let recovery = spall_store::recover(&path).unwrap();
    let (restored, _) = persist::restore(
        &recovery,
        &cfg,
        RecoveryChoice::RequireClean,
        sim.world().materials().clone(),
        AnchorPlane::at(0),
        PhysicsConfig::default(),
    )
    .unwrap();
    assert_eq!(ice_cells(&restored), 30 * 14, "the journal replays the ice");
    assert_eq!(restored.world().world_hash(), sim.world().world_hash());
    drop(restored);
    std::fs::remove_dir_all(dir).unwrap();
}
