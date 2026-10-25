use sandbox::editor_scene;
use spall_core::{BRUSH_UNIT, BrushPoint, GlobalCell, PlayerInput, SphereBrush, player_entity_for};
use spall_protocol::InputSeq;
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;
use std::path::{Path, PathBuf};

fn shed_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/shed")
}

/// The door hinge cell: scene (6.75, 1.0, 6.0) m at quarter-metre cells.
const HINGE: [i64; 3] = [27, 4, 24];

#[test]
fn shed_player_walks_in_and_out_through_its_door() {
    let scene = editor_scene::load(&shed_dir()).unwrap();
    assert_eq!(scene.player_spawns.len(), 1);
    let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup())).unwrap();
    let player = player_entity_for(0);
    let start = scene.player_spawns[0];
    sim.add_player(player, start);
    for _ in 0..60 {
        sim.tick().unwrap();
    }
    assert!(sim.player_state(player).unwrap().grounded);
    // The door starts closed and uses the shared cabin door spec.
    let door = spall_voxel::doors::Door::at(
        &sim.world().terrain().volume,
        GlobalCell::new(HINGE[0], HINGE[1], HINGE[2]),
        sandbox::game::CABIN_DOOR,
    )
    .expect("hinge present");
    assert!(!door.open);
    // Knob cells ride the panel: closed (31, 8, 23/25) -> open (26/28, 8, 28).
    let panel = |sim: &Simulation, x, z| {
        matches!(
            sim.world().terrain().volume.sample(GlobalCell::new(x, 8, z)),
            Ok(Sample::Filled(m)) if m == sandbox::game::CABIN_DOOR.panel
        )
    };
    assert!(panel(&sim, 31, 23) && panel(&sim, 31, 25));
    turn_door(&mut sim);
    assert!(panel(&sim, 26, 28) && panel(&sim, 28, 28));
    let mut seq = 0;
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
        assert!(state.grounded, "{state:?}");
        if direction < 0.0 {
            assert!(state.position_m[2] < 6.0, "must pass the door: {state:?}");
        } else {
            assert!(
                // Inside the walls (z 6.25..9.75, x 6.25..8.75) and on the floor.
                (6.3..9.7).contains(&state.position_m[2])
                    && (6.3..8.7).contains(&state.position_m[0])
                    && state.position_m[1] > 0.9,
                "must return inside: {state:?}"
            );
        }
    }
}

#[test]
fn shed_has_no_floating_structural_fragments() {
    use spall_jobs::{Generation, TopologyEpoch};
    use spall_structure::{CancelToken, ResidencyMode, StructureIndex};
    let setup = editor_scene::load(&shed_dir()).unwrap().world_setup();
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

#[test]
fn shed_has_a_woodpile_and_a_named_axe_marker_above_its_block() {
    let terrain = editor_scene::load(&shed_dir())
        .unwrap()
        .world_setup()
        .terrain;
    let solid = |x: i64, y: i64, z: i64| {
        matches!(
            terrain.sample(GlobalCell::new(x, y, z)),
            Ok(Sample::Filled(_))
        )
    };
    // Shed cell (1, 3, 8) is woodpile; the shed origin is cell (24, 2, 24).
    assert!(solid(24 + 1, 2 + 3, 24 + 8), "woodpile");
    let text = std::fs::read_to_string(shed_dir().join("scenes/main.ron")).unwrap();
    let at = text.find("name: \"axe_spawn\"").expect("axe_spawn entity");
    let tail = &text[at..];
    let t = tail.find("translation: (").unwrap() + "translation: (".len();
    let p: Vec<f64> = tail[t..tail[t..].find(')').unwrap() + t]
        .split(',')
        .map(|v| v.trim().parse().unwrap())
        .collect();
    let [cx, cy, cz] = [p[0], p[1], p[2]].map(|v| (v / 0.25).floor() as i64);
    assert!(solid(cx, cy - 1, cz), "axe rests on the chopping block");
    assert!(!solid(cx, cy, cz), "the spawn cell above the block is free");
}

fn turn_door(sim: &mut Simulation) {
    sim.submit(spall_sim::EditIntent {
        request_id: spall_protocol::RequestId(100),
        actor: player_entity_for(0),
        target: spall_sim::EditTarget::Terrain,
        kind: spall_sim::EditKind::Door(sandbox::game::CABIN_DOOR),
        brush: SphereBrush::new(
            BrushPoint::from_units(
                HINGE[0] * BRUSH_UNIT + BRUSH_UNIT / 2,
                HINGE[1] * BRUSH_UNIT + BRUSH_UNIT / 2,
                HINGE[2] * BRUSH_UNIT + BRUSH_UNIT / 2,
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
