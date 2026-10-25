use sandbox::editor_scene;
use spall_core::{GlobalCell, PlayerInput, player_entity_for};
use spall_protocol::InputSeq;
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;
use std::path::Path;

#[test]
fn cabin_starts_inside_and_player_walks_out_and_back() {
    let scene = editor_scene::load(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/cabin")
            .as_path(),
    )
    .unwrap();
    assert_eq!(scene.player_spawns.len(), 4);
    assert!(scene.column_water_setup().is_none());
    let setup = scene.world_setup();
    for spawn in &scene.player_spawns {
        assert_eq!(spawn[1], 1.0);
        let x = (spawn[0] * 4.0).floor() as i64;
        let z = (spawn[2] * 4.0).floor() as i64;
        for y in 4..13 {
            assert!(matches!(
                setup.terrain.sample(GlobalCell::new(x, y, z)),
                Ok(Sample::Empty { .. })
            ));
        }
    }
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    let player = player_entity_for(0);
    let start = scene.player_spawns[0];
    sim.add_player(player, start);
    for _ in 0..60 {
        sim.tick().unwrap();
    }
    assert!(sim.player_state(player).unwrap().grounded);
    turn_door(&mut sim, 100);
    let mut seq = 0;
    // Cross the doorway and porch onto the step, then return inside.
    for (direction, ticks) in [(-1.0, 80), (1.0, 80)] {
        for _ in 0..ticks {
            seq += 1;
            assert!(sim.set_player_input(
                player,
                PlayerInput {
                    movement: [0.0, 0.0, 1.0],
                    view_dir: [0.0, 0.0, direction],
                    buttons: 0
                },
                InputSeq(seq)
            ));
            sim.tick().unwrap();
        }
        let state = sim.player_state(player).unwrap();
        let floor = if direction < 0.0 { 0.75 } else { 1.0 };
        assert!(
            state.grounded && (state.position_m[1] - floor).abs() < 0.1,
            "{state:?}"
        );
        if direction < 0.0 {
            assert!(
                state.position_m[2] < 6.0,
                "must pass through front wall: {state:?}"
            );
        } else {
            assert!(
                (state.position_m[2] - start[2]).abs() < 0.3,
                "must return inside: {state:?}"
            );
        }
    }
}

#[test]
fn cabin_has_no_floating_structural_fragments() {
    use spall_jobs::{Generation, TopologyEpoch};
    use spall_structure::{CancelToken, ResidencyMode, StructureIndex};
    let scene =
        editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
            .unwrap();
    let setup = scene.world_setup();
    let index = StructureIndex::build(
        &setup.terrain,
        setup.anchor,
        ResidencyMode::AllResident,
        Generation::START,
        TopologyEpoch::START,
        &CancelToken::new(),
    )
    .unwrap();
    assert!(index.graph().components().iter().all(|c| c.anchored));
}

fn turn_door(sim: &mut Simulation, request: u64) {
    use spall_core::{BRUSH_UNIT, BrushPoint, SphereBrush};
    sim.submit(spall_sim::EditIntent {
        request_id: spall_protocol::RequestId(request),
        actor: player_entity_for(0),
        target: spall_sim::EditTarget::Terrain,
        kind: spall_sim::EditKind::Door(sandbox::game::CABIN_DOOR),
        brush: SphereBrush::new(
            BrushPoint::from_units(
                37 * BRUSH_UNIT + BRUSH_UNIT / 2,
                4 * BRUSH_UNIT + BRUSH_UNIT / 2,
                24 * BRUSH_UNIT + BRUSH_UNIT / 2,
            ),
            0,
        )
        .unwrap(),
        explosion: None,
    })
    .unwrap();
    let mut committed = false;
    for _ in 0..8 {
        let report = sim.tick().unwrap();
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        committed |= !report.committed.is_empty();
    }
    assert!(committed);
}

#[test]
fn door_turns_preserve_damage_and_replay_exactly() {
    let scene =
        editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
            .unwrap();
    let mut setup = scene.world_setup();
    let mut damage = spall_voxel::EditPlan::new(setup.terrain.id());
    damage.set(GlobalCell::new(40, 8, 24), spall_core::MaterialId::AIR);
    setup.terrain.apply_edit(&damage).unwrap();
    let initial = setup.terrain.clone();
    let mut replay_setup = scene.world_setup();
    replay_setup.terrain.apply_edit(&damage).unwrap();
    let mut replay = spall_sim::SimWorld::new(replay_setup).unwrap();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    turn_door(&mut sim, 101);
    let opened = &sim.world().terrain().volume;
    assert!(
        matches!(
            opened.sample(GlobalCell::new(37, 8, 27)),
            Ok(Sample::Empty { .. })
        ),
        "hole rotates with panel"
    );
    assert!(
        matches!(
            opened.sample(GlobalCell::new(40, 6, 24)),
            Ok(Sample::Empty { .. })
        ),
        "closed doorway clears"
    );
    // The knob (panel cells one cell off the leaf, 1 m up, away from the hinge)
    // turned with the panel: closed (41, 8, 23/25) -> open (36/38, 8, 28).
    let panel = |v: &spall_voxel::Volume, x, z| {
        matches!(
            v.sample(GlobalCell::new(x, 8, z)),
            Ok(Sample::Filled(m)) if m == sandbox::game::CABIN_DOOR.panel
        )
    };
    assert!(
        panel(&initial, 41, 23) && panel(&initial, 41, 25),
        "knob on both faces"
    );
    assert!(
        panel(opened, 36, 28) && panel(opened, 38, 28),
        "knob opens with panel"
    );
    assert!(
        !panel(opened, 41, 23) && !panel(opened, 41, 25),
        "knob left the closed spot"
    );
    let door = spall_voxel::doors::Door::at(
        opened,
        GlobalCell::new(37, 4, 24),
        sandbox::game::CABIN_DOOR,
    )
    .unwrap();
    assert!(door.open);
    turn_door(&mut sim, 102);
    for entry in sim.journal().entries_after(0) {
        replay
            .replay_transaction(
                &entry.transaction,
                &entry.participants,
                entry.geometry_only_bulk(),
            )
            .unwrap();
    }
    assert_eq!(replay.world_hash(), sim.world().world_hash());
    for y in 4..14 {
        for x in 36..43 {
            for z in 23..30 {
                let cell = GlobalCell::new(x, y, z);
                assert_eq!(
                    sim.world().terrain().volume.sample(cell),
                    initial.sample(cell)
                );
            }
        }
    }
}

#[test]
fn door_prompt_requires_proximity_and_line_of_sight() {
    let scene =
        editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
            .unwrap();
    let mut terrain = scene.world_setup().terrain;
    let find = |volume: &spall_voxel::Volume, eye| {
        spall_voxel::doors::nearby_door(volume, eye, sandbox::game::CABIN_DOOR, 3.0)
    };
    assert!(find(&terrain, [10.0, 2.6, 8.5]).is_some());
    assert!(find(&terrain, [10.0, 2.6, 11.0]).is_none());
    let wall = spall_voxel::EditPlan::filled_box(
        terrain.id(),
        GlobalCell::new(36, 4, 29),
        GlobalCell::new(44, 15, 29),
        sandbox::game::materials::STONE,
    );
    terrain.apply_edit(&wall).unwrap();
    assert!(find(&terrain, [10.0, 2.6, 8.5]).is_none());
}

#[test]
fn occupied_door_turn_is_rejected_without_geometry_changes() {
    let scene =
        editor_scene::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cabin"))
            .unwrap();
    let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup())).unwrap();
    sim.add_player(player_entity_for(0), [10.0, 1.0, 7.0]);
    let hash = sim.world().world_hash();
    use spall_core::{BRUSH_UNIT, BrushPoint, SphereBrush};
    sim.submit(spall_sim::EditIntent {
        request_id: spall_protocol::RequestId(103),
        actor: player_entity_for(0),
        target: spall_sim::EditTarget::Terrain,
        kind: spall_sim::EditKind::Door(sandbox::game::CABIN_DOOR),
        brush: SphereBrush::new(
            BrushPoint::from_units(
                37 * BRUSH_UNIT + BRUSH_UNIT / 2,
                4 * BRUSH_UNIT + BRUSH_UNIT / 2,
                24 * BRUSH_UNIT + BRUSH_UNIT / 2,
            ),
            0,
        )
        .unwrap(),
        explosion: None,
    })
    .unwrap();
    let mut rejected = false;
    for _ in 0..8 {
        let report = sim.tick().unwrap();
        rejected |= !report.rejected.is_empty();
        assert!(report.committed.is_empty());
    }
    assert!(rejected);
    assert_eq!(hash, sim.world().world_hash());
}
