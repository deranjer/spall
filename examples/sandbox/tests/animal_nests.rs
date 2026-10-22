#![cfg(feature = "client")]
//! ENG-236: a crow builds a nest of real terrain cells on a tree top, nothing
//! it builds splits off, it roosts in the nest at night, and it rebuilds a
//! nest that was cut away.
//!
//! Run: `cargo test --release -p sandbox --features client --test animal_nests`.

use sandbox::animals::{AnimalDirector, LOCOMOTION_ROOST};
use sandbox::game::materials::WOOD;
use spall_core::time_of_day::{DAY_CYCLE_TICKS, state_at_tick};
use spall_core::{
    BRUSH_UNIT, BrushPoint, CellSizeCode, EntityId, GlobalCell, MaterialId, PLAYER_ENTITY_BASE,
    SphereBrush,
};
use spall_server::creature_service::{
    CreatureAction, CreatureActionOutcome, CreatureDirector, CreatureIdPlan, CreatureService,
};
use spall_sim::{EditIntent, EditTarget, RequestId, Simulation, SimulationConfig, fixtures};
use spall_voxel::{EditPlan, Sample, Volume};

/// World size in cells (0.25 m): a 2-cell stone floor, air above, and one
/// wooden tree in the middle.
const SIDE: i64 = 160;
const HEIGHT: i64 = 48;
/// The tree's trunk column and its top cell (4 m of trunk).
const TRUNK: [i64; 2] = [80, 80];
const TOP_Y: i64 = 17;

fn top() -> GlobalCell {
    GlobalCell::new(TRUNK[0], TOP_Y, TRUNK[1])
}

/// The nine cells a nest on [`top`] fills: a floor on the top, four round it,
/// and a rim over those four.
fn nest() -> Vec<GlobalCell> {
    let t = top();
    let mut cells = vec![GlobalCell::new(t.x, t.y + 1, t.z)];
    for y in [t.y + 1, t.y + 2] {
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            cells.push(GlobalCell::new(t.x + dx, y, t.z + dz));
        }
    }
    cells
}

fn simulation() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    let mut fill = |a: [i64; 3], b: [i64; 3], material: MaterialId| {
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(a[0], a[1], a[2]),
                GlobalCell::new(b[0], b[1], b[2]),
                material,
            ))
            .unwrap();
    };
    fill([0, 0, 0], [SIDE - 1, 1, SIDE - 1], fixtures::STONE);
    fill([0, 2, 0], [SIDE - 1, HEIGHT - 1, SIDE - 1], MaterialId::AIR);
    let [x, z] = TRUNK;
    fill([x, 2, z], [x, TOP_Y, z], WOOD);
    // A small crown: two crossed branches a little under the top.
    fill([x - 2, 15, z], [x + 2, 15, z], WOOD);
    fill([x, 15, z - 2], [x, 15, z + 2], WOOD);
    setup.terrain = volume;
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE - 1, HEIGHT - 1, SIDE - 1),
    );
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    sim
}

/// The director shifted `offset` ticks on its clock.
struct Shifted(AnimalDirector, u64);

impl CreatureDirector for Shifted {
    fn motion(&mut self, tick: u64, sim: &Simulation) -> Vec<CreatureAction> {
        self.0.motion(tick + self.1, sim)
    }
    fn on_action_outcome(&mut self, creature_id: u64, outcome: CreatureActionOutcome) {
        self.0.on_action_outcome(creature_id, outcome);
    }
    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<spall_protocol::CreatureRecord> {
        self.0.plan(tick + self.1, sim, ids)
    }
}

/// One crow born 3 m from the tree, its clock `offset` ticks on.
fn crow_service(offset: u64) -> CreatureService {
    let anchor = [TRUNK[0] as f64 * 0.25 + 3.0, 0.5, TRUNK[1] as f64 * 0.25];
    let director = AnimalDirector::new(vec![anchor], &[0, 1], 3)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    CreatureService::new(Box::new(Shifted(director, offset)))
}

fn wood_at(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(sim.world().terrain().volume.sample(cell), Ok(Sample::Filled(m)) if m == WOOD)
}

fn nest_cells_built(sim: &Simulation) -> usize {
    nest().into_iter().filter(|&c| wood_at(sim, c)).count()
}

/// Ticks the world until `done` or `limit` ticks, checking every tick that
/// nothing split off as a body. Returns the ticks taken.
fn run_until(
    sim: &mut Simulation,
    service: &CreatureService,
    limit: u32,
    mut done: impl FnMut(&Simulation) -> bool,
) -> Option<u32> {
    for tick in 0..limit {
        sim.tick().unwrap();
        service.run_tick(sim).unwrap();
        service.run_motion(sim).unwrap();
        assert_eq!(
            sim.world().body_count(),
            0,
            "a cell split off as a loose body at tick {tick}"
        );
        if done(sim) {
            return Some(tick);
        }
    }
    None
}

fn crow(sim: &Simulation) -> spall_protocol::CreatureRecord {
    sim.creature_records()
        .into_iter()
        .find(|c| c.species == "crow")
        .expect("the crow was born")
}

#[test]
fn a_crow_builds_a_nest_of_terrain_cells_and_rebuilds_it_when_cut() {
    let mut sim = simulation();
    // 06:00, morning: building, no roosting.
    let service = crow_service(0);
    // Visits: runs of new cells separated by at least 4 s of flying away.
    let (mut tick, mut last_cell_at, mut cells, mut visits) = (0_u32, None::<u32>, 0, 0);
    let built = run_until(&mut sim, &service, 9_000, |sim| {
        tick += 1;
        let now = nest_cells_built(sim);
        if now > cells {
            if last_cell_at.is_none_or(|at| tick - at > 240) {
                visits += 1;
            }
            last_cell_at = Some(tick);
            cells = now;
        }
        now == 9
    });
    eprintln!("nest finished at tick {built:?} over {visits} visits");
    assert!(
        built.is_some(),
        "only {} of 9 nest cells built",
        nest_cells_built(&sim)
    );
    assert!(visits >= 3, "the nest went up in {visits} visits");
    // The nest stands on the tree top, which is still there.
    assert!(wood_at(&sim, top()));

    // A player cuts the whole nest away (the top survives).
    let t = top();
    let centre = BrushPoint::from_units(
        t.x * BRUSH_UNIT + BRUSH_UNIT / 2,
        (t.y + 2) * BRUSH_UNIT,
        t.z * BRUSH_UNIT + BRUSH_UNIT / 2,
    );
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        SphereBrush::new(centre, BRUSH_UNIT * 6 / 5).unwrap(),
    ))
    .unwrap();
    sim.run_until_idle(64).unwrap();
    assert_eq!(nest_cells_built(&sim), 0, "the cut left nest cells");
    assert!(wood_at(&sim, top()), "the cut took the tree top");

    let rebuilt = run_until(&mut sim, &service, 9_000, |sim| nest_cells_built(sim) == 9);
    eprintln!("nest rebuilt after {rebuilt:?} ticks");
    assert!(
        rebuilt.is_some(),
        "the crow rebuilt only {} of 9 cells",
        nest_cells_built(&sim)
    );
}

#[test]
fn a_crow_roosts_in_its_nest_at_night() {
    // 21:00: the sun is down for every crow's bedtime lag.
    let night = DAY_CYCLE_TICKS * 5 / 8;
    assert!(state_at_tick(night).sun_height() < 0.0);
    let mut sim = simulation();
    let service = crow_service(night);
    // With no nest yet it builds one first, then settles into it.
    let roosted = run_until(&mut sim, &service, 12_000, |sim| {
        sim.creature_records()
            .iter()
            .any(|c| c.species == "crow" && c.locomotion_tag == LOCOMOTION_ROOST)
    });
    eprintln!("roosting at tick {roosted:?}");
    assert!(roosted.is_some(), "the crow never roosted");
    assert_eq!(
        nest_cells_built(&sim),
        9,
        "it roosted in an unfinished nest"
    );
    // It stays put, sitting on the nest floor inside the rim.
    run_until(&mut sim, &service, 600, |_| false);
    let bird = crow(&sim);
    assert_eq!(bird.locomotion_tag, LOCOMOTION_ROOST, "it left its roost");
    let t = top();
    let [x, y, z] = bird.translation_m;
    let off = (x - (t.x as f64 + 0.5) * 0.25).hypot(z - (t.z as f64 + 0.5) * 0.25);
    assert!(off < 0.15, "roosting {off:.2} m off the nest centre");
    let floor = (t.y + 2) as f64 * 0.25;
    assert!(
        (floor - 0.1..floor + 0.6).contains(&y),
        "root at {y:.2} m, nest floor at {floor:.2} m"
    );

    // A player walking up under the tree still sends it off.
    sim.add_player(
        EntityId::new(PLAYER_ENTITY_BASE + 1).unwrap(),
        [x - 6.0, 0.6, z],
    );
    run_until(&mut sim, &service, 24, |_| false);
    assert_ne!(
        crow(&sim).locomotion_tag,
        LOCOMOTION_ROOST,
        "a player 6 m off did not spook the roosting crow"
    );
}
