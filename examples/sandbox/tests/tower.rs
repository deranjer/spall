//! The lookout tower (ENG-255): structure, a real capsule on its stairs and
//! ladders, and an end-to-end climb from the door to the win zone.
use sandbox::editor_scene;
use sandbox::game::materials;
use spall_core::{BRUSH_UNIT, BrushPoint, GlobalCell, PlayerInput, SphereBrush, player_entity_for};
use spall_protocol::InputSeq;
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;
use std::path::{Path, PathBuf};

fn tower_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tower")
}

/// The tower asset origin is scene cell (24, 2, 24): placement (6, 0.5, 6) m.
const ORIGIN: [i64; 3] = [24, 2, 24];
/// The door hinge cell: tower-local (15, 2, 0).
const HINGE: [i64; 3] = [39, 4, 24];
/// Landing tops above the ground floor, in cells (see the generator).
const LANDINGS: [i64; 7] = [0, 12, 32, 44, 64, 76, 88];

/// Scene metres of the centre of a tower-local cell column.
fn at(x: f64, z: f64) -> [f64; 2] {
    [6.0 + x * 0.25, 6.0 + z * 0.25]
}
/// Scene metres of the top of landing `k`.
fn landing_y(k: usize) -> f64 {
    0.5 + (2 + LANDINGS[k]) as f64 * 0.25
}

struct Pilot {
    sim: Simulation,
    seq: u64,
}

impl Pilot {
    fn new(start: [f64; 3]) -> Self {
        let scene = editor_scene::load(&tower_dir()).unwrap();
        let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup())).unwrap();
        sim.add_player(player_entity_for(0), start);
        let mut pilot = Self { sim, seq: 0 };
        pilot.settle(60);
        pilot
    }
    fn state(&self) -> spall_physics::CharacterState {
        self.sim.player_state(player_entity_for(0)).unwrap()
    }
    fn tick(&mut self, forward: f32, view: [f32; 3]) {
        self.seq += 1;
        assert!(self.sim.set_player_input(
            player_entity_for(0),
            PlayerInput {
                movement: [0.0, 0.0, forward],
                view_dir: view,
                buttons: 0
            },
            InputSeq(self.seq)
        ));
        self.sim.tick().unwrap();
    }
    fn settle(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.tick(0.0, [0.0, 0.0, 1.0]);
        }
    }
    /// Walk toward the tower-local cell position `(x, z)` until within 0.15 m.
    fn walk_to(&mut self, x: f64, z: f64) {
        let [tx, tz] = at(x, z);
        for _ in 0..900 {
            let p = self.state().position_m;
            let (dx, dz) = (tx - p[0], tz - p[2]);
            if dx.hypot(dz) < 0.15 {
                return;
            }
            self.tick(1.0, [dx as f32, 0.0, dz as f32]);
        }
        panic!("never reached ({x}, {z}): {:?}", self.state());
    }
    /// Hold W facing `view` until `done` or the tick budget runs out.
    fn hold_w(
        &mut self,
        view: [f32; 3],
        mut done: impl FnMut(&spall_physics::CharacterState) -> bool,
    ) {
        for _ in 0..1200 {
            if done(&self.state()) {
                return;
            }
            self.tick(1.0, view);
        }
        panic!("never finished: {:?}", self.state());
    }
    /// Face +x, climb the stairs of the section that starts on landing `k` and
    /// stop once standing on landing `k + 1`.
    fn climb_stairs(&mut self, k: usize) {
        self.walk_to(4.5, 29.5);
        let top = landing_y(k + 1);
        self.hold_w([1.0, 0.0, 0.0], |s| {
            s.grounded && (s.position_m[1] - top).abs() < 0.05 && s.position_m[0] > at(29.0, 0.0)[0]
        });
    }
    /// Face +x, climb the ladder of the section that starts on landing `k`,
    /// step onto landing `k + 1` and walk on past the ladder's top.
    fn climb_ladder(&mut self, k: usize) {
        self.walk_to(4.0, 14.0);
        self.walk_to(4.0, 10.0);
        let top = landing_y(k + 1);
        let mut climbed = false;
        self.hold_w([1.0, 0.0, 0.0], |s| {
            climbed |= s.climbing;
            climbed
                && !s.climbing
                && s.grounded
                && (s.position_m[1] - top).abs() < 0.05
                && s.position_m[0] > at(9.0, 0.0)[0]
        });
    }
}

#[test]
fn tower_has_no_floating_structural_fragments() {
    use spall_jobs::{Generation, TopologyEpoch};
    use spall_structure::{CancelToken, ResidencyMode, StructureIndex};
    let setup = editor_scene::load(&tower_dir()).unwrap().world_setup();
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
fn capsule_walks_up_a_stair_section() {
    // The foot of S1 on the ground floor, then up to the first landing.
    let mut pilot = Pilot::new([at(4.5, 29.5)[0], landing_y(0), at(4.5, 29.5)[1]]);
    pilot.climb_stairs(0);
    let end = pilot.state();
    assert!(end.grounded && !end.climbing, "{end:?}");
    assert!((end.position_m[1] - landing_y(1)).abs() < 0.05, "{end:?}");
}

#[test]
fn capsule_climbs_a_ladder_section_and_steps_onto_its_landing() {
    // On the first landing, in front of L1.
    let [x, z] = at(4.0, 10.0);
    let mut pilot = Pilot::new([x, landing_y(1), z]);
    assert!(!pilot.state().climbing);
    pilot.climb_ladder(1);
    let end = pilot.state();
    assert!(end.grounded && !end.climbing, "{end:?}");
    assert!((end.position_m[1] - landing_y(2)).abs() < 0.05, "{end:?}");
    assert!(
        end.position_m[0] > at(8.0, 0.0)[0],
        "past the ladder: {end:?}"
    );
}

#[test]
fn player_climbs_from_the_door_to_the_win_zone() {
    let scene = editor_scene::load(&tower_dir()).unwrap();
    assert_eq!(scene.player_spawns.len(), 1);
    let mut pilot = Pilot::new(scene.player_spawns[0]);
    turn_door(&mut pilot.sim);
    pilot.walk_to(18.5, 6.0);
    pilot.walk_to(4.5, 14.0);
    // S1, then (L, S) pairs: L1 S2 L2 S3, and the final ladder L3.
    pilot.climb_stairs(0);
    for k in [1, 3] {
        pilot.walk_to(29.5, 14.0);
        pilot.climb_ladder(k);
        pilot.walk_to(10.0, 15.0);
        pilot.walk_to(4.5, 15.0);
        pilot.climb_stairs(k + 1);
    }
    pilot.walk_to(29.5, 14.0);
    pilot.climb_ladder(5);
    // Round the beacon plinth to the zone.
    pilot.walk_to(10.0, 7.5);
    pilot.walk_to(26.0, 7.5);
    let (_, win) = win_zone();
    pilot.walk_to(win[0], win[1]);
    let end = pilot.state();
    assert!(end.grounded, "{end:?}");
    assert!((end.position_m[1] - landing_y(6)).abs() < 0.05, "{end:?}");
    // Nothing broke on the way up.
    assert!(pilot.sim.world().bodies().next().is_none());
}

/// The `win_zone` marker: its scene metres and the tower-local cell centre.
fn win_zone() -> ([f64; 3], [f64; 2]) {
    let text = std::fs::read_to_string(tower_dir().join("scenes/main.ron")).unwrap();
    let at_name = text.find("name: \"win_zone\"").expect("win_zone entity");
    let tail = &text[at_name..];
    let t = tail.find("translation: (").unwrap() + "translation: (".len();
    let p: Vec<f64> = tail[t..tail[t..].find(')').unwrap() + t]
        .split(',')
        .map(|v| v.trim().parse().unwrap())
        .collect();
    (
        [p[0], p[1], p[2]],
        [(p[0] - 6.0) / 0.25, (p[2] - 6.0) / 0.25],
    )
}

#[test]
fn win_zone_marker_sits_on_the_platform_with_headroom_and_a_beacon_burns_above() {
    let terrain = editor_scene::load(&tower_dir())
        .unwrap()
        .world_setup()
        .terrain;
    let solid = |x: i64, y: i64, z: i64| {
        matches!(
            terrain.sample(GlobalCell::new(x, y, z)),
            Ok(Sample::Filled(_))
        )
    };
    let (m, _) = win_zone();
    let cell = |v: f64| (v / 0.25 + 1e-3).floor() as i64;
    let [cx, cy, cz] = [cell(m[0]), cell(m[1]), cell(m[2])];
    // The whole 1 m radius disc stands on platform and has 2 m of air above it.
    for dx in -4..=4i64 {
        for dz in -4..=4i64 {
            if (dx * dx + dz * dz) as f64 > 16.0 {
                continue;
            }
            assert!(
                solid(cx + dx, cy - 1, cz + dz),
                "platform under ({dx}, {dz})"
            );
            for dy in 0..8 {
                assert!(
                    !solid(cx + dx, cy + dy, cz + dz),
                    "headroom ({dx}, {dy}, {dz})"
                );
            }
        }
    }
    assert_eq!(cy, ORIGIN[1] + 2 + LANDINGS[6]);
    // The beacon: emissive lamp cells above the plinth at the platform centre.
    assert!(matches!(
        terrain.sample(GlobalCell::new(ORIGIN[0] + 17, cy + 2, ORIGIN[2] + 17)),
        Ok(Sample::Filled(id)) if id == materials::LAMP
    ));
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
