//! Actual simulation -> SQLite checkpoint/journal -> reconstructed simulation.
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, PlayerInput, Revision, SphereBrush,
    VolumeId, player_entity_for,
};
use spall_physics::PhysicsConfig;
use spall_protocol::{InputSeq, RequestId, WeatherKind};
use spall_server::{PersistConfig, persist};
use spall_sim::{EditIntent, EditKind, EditTarget, Simulation, SimulationConfig};
use spall_store::{JournalPayload, JournalRecord, Writer};
use spall_structure::AnchorPlane;
use spall_voxel::{Brick, BrickBounds, EditPlan, Volume};

fn fixture() -> Simulation {
    let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(1, 0, 1)).unwrap();
    let mut terrain = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
    for z in 0..2 {
        for x in 0..2 {
            terrain
                .insert_brick(
                    BrickCoord::new(x, 0, z),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )
                .unwrap();
        }
    }
    let mut floor = EditPlan::new(terrain.id());
    for z in 0..64 {
        for x in 0..64 {
            for y in 0..2 {
                floor.set(GlobalCell::new(x, y, z), MaterialId(1));
            }
        }
    }
    terrain.apply_edit(&floor).unwrap();
    let mut world = spall_sim::fixtures::flat_terrain_setup();
    world.terrain = terrain;
    world.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(63, 31, 63));
    let mut sim = Simulation::new(SimulationConfig::new(world)).unwrap();
    profile(&mut sim, WeatherKind::Snow, 3);
    sim.add_player(player_entity_for(0), [2.0, 0.52, 2.0]);
    sim
}

fn profile(sim: &mut Simulation, kind: WeatherKind, season: u8) {
    let mut weather = sim.weather_state().clone();
    weather.start_season = season;
    sim.restore_weather(weather).unwrap();
    sim.set_weather_override(Some(kind));
}

fn journal_tick(sim: &mut Simulation, through: &mut u64) -> Vec<JournalRecord> {
    let tick = sim.current_tick().get();
    let mut records = persist::journal_records(sim.journal().entries_after(*through)).unwrap();
    if let Some(last) = records.last() {
        *through = last.seq;
    }
    if let Some(state) = sim.take_weather_journal() {
        let seq = sim.reserve_journal_seq().unwrap().0;
        records.push(JournalRecord {
            seq,
            tick,
            payload: JournalPayload::WeatherState(state),
        });
        *through = seq;
    }
    for (key, state) in sim.take_snow_journal_tiles().unwrap() {
        let seq = sim.reserve_journal_seq().unwrap().0;
        records.push(JournalRecord {
            seq,
            tick,
            payload: JournalPayload::SnowTileState {
                world_epoch: sim.weather_state().world_epoch,
                tile: [key.0, key.1, key.2],
                state,
            },
        });
        *through = seq;
    }
    records
}

fn exact_tiles(sim: &Simulation, keys: &[(i32, i32, i32)]) -> Vec<Vec<u8>> {
    let mut state = sim.snow_state().clone();
    keys.iter()
        .map(|&key| state.tile_canonical_bytes(key).unwrap())
        .collect()
}

#[test]
fn checkpoint_and_geometry_journal_preserve_q32_snow_but_clear_temporary_tracks() {
    let dir = std::env::temp_dir().join(format!(
        "spall-snow-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = PersistConfig {
        world_id: 206,
        seed: 0,
        generator_version: 1,
    };
    let mut sim = fixture();
    let player = player_entity_for(0);
    for tick in 1..=487 {
        let moving = (400..440).contains(&tick);
        sim.set_player_input(
            player,
            PlayerInput {
                movement: [0.0, 0.0, if moving { 1.0 } else { 0.0 }],
                view_dir: [1.0, 0.0, 0.0],
                buttons: 0,
            },
            InputSeq(tick),
        );
        sim.tick().unwrap();
    }
    assert!(
        sim.snow_snapshot_frames(None)
            .iter()
            .any(|f| !f.footprints.is_empty()),
        "fixture must create actual validated walking tracks before save"
    );
    let checkpoint = persist::capture(&sim, &cfg, 0).unwrap();
    assert!(
        checkpoint
            .snow
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty())
    );
    let mut writer = Writer::open(dir.join("world.db")).unwrap();
    writer.publish_checkpoint(&checkpoint).unwrap();
    let materials = sim.world().materials().clone();
    let restore = |recovery: &spall_store::Recovery| {
        persist::restore(
            recovery,
            &cfg,
            persist::RecoveryChoice::RequireClean,
            materials.clone(),
            AnchorPlane::at(0),
            PhysicsConfig::default(),
        )
    };
    let (mut from_checkpoint, _) = restore(&writer.recover().unwrap()).unwrap();
    let checkpoint_keys = sim.snow_state().tile_keys();
    assert_eq!(
        exact_tiles(&sim, &checkpoint_keys),
        exact_tiles(&from_checkpoint, &checkpoint_keys)
    );
    assert!(
        from_checkpoint
            .snow_snapshot_frames(None)
            .iter()
            .all(|f| f.footprints.is_empty())
    );

    let mut through = 0;
    let mut geometry_records = 0;
    for tick in 488..=600 {
        if tick == 492 {
            let half = BRUSH_UNIT / 2;
            sim.submit(EditIntent {
                request_id: RequestId(1),
                actor: EntityId::new(1).unwrap(),
                target: EditTarget::Terrain,
                kind: EditKind::Cut,
                brush: SphereBrush::new(
                    BrushPoint::from_units(
                        8 * BRUSH_UNIT + half,
                        BRUSH_UNIT + half,
                        8 * BRUSH_UNIT + half,
                    ),
                    BRUSH_UNIT,
                )
                .unwrap(),
                explosion: None,
            })
            .unwrap();
        }
        if tick == 500 {
            profile(&mut sim, WeatherKind::Rain, 1);
        }
        if tick == 530 {
            profile(&mut sim, WeatherKind::Snow, 3);
        }
        // A final accepted preset command proves this exact durable boundary;
        // ticks without journal records are not durable suffix time by themselves.
        if tick == 600 {
            sim.set_weather_override(Some(WeatherKind::Snow));
        }
        sim.tick().unwrap();
        let batch = journal_tick(&mut sim, &mut through);
        geometry_records += batch
            .iter()
            .filter(|record| matches!(record.payload, JournalPayload::SnowTileState { .. }))
            .count();
        if !batch.is_empty() {
            writer.append_journal(&batch).unwrap();
        }
    }
    assert!(
        geometry_records > 0,
        "committed terrain changes must journal exact snow masks/credit"
    );
    let recovered = writer.recover().unwrap();
    let (mut restarted, _) = restore(&recovered).unwrap();
    assert_eq!(restarted.current_tick(), sim.current_tick());
    assert_eq!(restarted.world().world_hash(), sim.world().world_hash());
    let keys = sim.snow_state().tile_keys();
    assert_eq!(
        exact_tiles(&sim, &keys),
        exact_tiles(&restarted, &keys),
        "Q32 depth/cursor must survive warm/cold and geometry replay exactly"
    );
    assert!(
        restarted
            .snow_snapshot_frames(None)
            .iter()
            .all(|f| f.footprints.is_empty())
    );
    for _ in 0..37 {
        sim.tick().unwrap();
        restarted.tick().unwrap();
    }
    assert_eq!(
        exact_tiles(&sim, &keys),
        exact_tiles(&restarted, &keys),
        "resumed fractional growth must stay exact"
    );

    let mut stale = recovered.clone();
    let record = stale
        .journal
        .iter_mut()
        .find(|r| matches!(r.payload, JournalPayload::SnowTileState { .. }))
        .unwrap();
    if let JournalPayload::SnowTileState { world_epoch, .. } = &mut record.payload {
        *world_epoch += 1;
    }
    assert!(
        restore(&stale).is_err(),
        "foreign-epoch snow geometry must never enter a recovered world"
    );
    drop(writer);
    // Keep evidence if an assertion fails; remove only this fixture's own directory on success.
    std::fs::remove_dir_all(dir).unwrap();
}
