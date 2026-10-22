#![cfg(feature = "client")]
//! ENG-243: a wolf pack surrounds a deer before taking it and eats the
//! carcass together; it backs away from a bear and from fire (the torch's lamp
//! voxel); it now and then hunts a player, biting a few times, and leaves a
//! player beside a fire alone.
//!
//! Run: `cargo test --release -p sandbox --features client --test animal_wolves`.

use sandbox::animals::{AnimalDirector, LOCOMOTION_EAT, LOCOMOTION_STALK};
use spall_core::{CellSizeCode, GlobalCell, MaterialId};
use spall_server::creature_service::CreatureService;
use spall_sim::{PLAYER_MAX_HEALTH, Simulation, SimulationConfig, fixtures};
use spall_voxel::{EditPlan, Volume};

/// Flat world size in cells (0.25 m): a 2-cell stone floor, air above.
const SIDE: i64 = 640;
const HEIGHT: i64 = 16;
const MIDDLE: f64 = SIDE as f64 * 0.125;
/// Spawn counts in species order: deer, crow, trout, bear, leopard, wolf.
const PACK: [u32; 6] = [0, 0, 0, 0, 0, 4];

/// The flat field, with the torch's lamp on the floor at each of `lamps`.
fn simulation(lamps: &[[f64; 2]]) -> Simulation {
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
    for lamp in lamps {
        let cell = [(lamp[0] / 0.25) as i64, 2, (lamp[1] / 0.25) as i64];
        fill(cell, cell, sandbox::game::materials::LAMP);
    }
    setup.terrain = volume;
    setup.terrain_collider_region = (
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(SIDE - 1, HEIGHT - 1, SIDE - 1),
    );
    setup.materials = sandbox::game::manifest();
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    sim.initialize_creature_host().unwrap();
    sim.set_creature_shape(sandbox::animal_visual::creature_shape().unwrap())
        .unwrap();
    sim
}

fn service(anchors: Vec<[f64; 3]>, targets: &[u32], seed: u64) -> CreatureService {
    let director = AnimalDirector::new(anchors, targets, seed)
        .unwrap()
        .with_radius_m(0.0, 0.5);
    CreatureService::new(Box::new(director))
}

fn run(sim: &mut Simulation, service: &CreatureService) {
    sim.tick().unwrap();
    service.run_tick(sim).unwrap();
    service.run_motion(sim).unwrap();
}

fn horizontal(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

fn wolves(sim: &Simulation) -> Vec<spall_protocol::CreatureRecord> {
    sim.creature_records()
        .into_iter()
        .filter(|c| c.species == "wolf")
        .collect()
}

/// Whether `at` lies inside the wolves' ring: no gap between neighbouring
/// wolves, seen from `at`, is half a turn or more.
fn surrounded(at: [f64; 3], wolves: &[[f64; 3]]) -> bool {
    if wolves.len() < 3 {
        return false;
    }
    let mut angles: Vec<f64> = wolves
        .iter()
        .map(|w| f64::atan2(w[2] - at[2], w[0] - at[0]))
        .collect();
    angles.sort_by(f64::total_cmp);
    let wrap = angles[0] + std::f64::consts::TAU - angles[angles.len() - 1];
    let widest = angles
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .fold(wrap, f64::max);
    widest < std::f64::consts::PI
}

fn player_entity() -> spall_core::EntityId {
    spall_core::EntityId::new(spall_core::PLAYER_ENTITY_BASE + 1).unwrap()
}

#[test]
fn a_pack_surrounds_a_deer_takes_it_and_eats_together() {
    let mut sim = simulation(&[]);
    // A deer and a pack born 45 m apart: out of the deer's notice, within
    // the pack's.
    let anchors = vec![[MIDDLE - 22.0, 0.5, MIDDLE], [MIDDLE + 23.0, 0.5, MIDDLE]];
    let service = service(anchors, &[1, 0, 0, 0, 0, 4], 7);
    let mut deer_id = None;
    let mut deer_at = [0.0; 3];
    let mut stalked = false;
    let mut surrounded_before_kill = false;
    let mut killed = None;
    let mut eaters = std::collections::HashSet::new();
    for tick in 0..20_000_u64 {
        run(&mut sim, &service);
        let records = sim.creature_records();
        let pack = wolves(&sim);
        match records.iter().find(|c| c.species == "deer") {
            Some(deer) => {
                deer_id = Some(deer.creature_id);
                deer_at = deer.translation_m;
                stalked |= pack.iter().any(|w| w.locomotion_tag == LOCOMOTION_STALK);
                let at: Vec<_> = pack.iter().map(|w| w.translation_m).collect();
                surrounded_before_kill |= surrounded(deer_at, &at);
            }
            None if deer_id.is_some() && killed.is_none() => killed = Some((tick, deer_at)),
            None => {}
        }
        if killed.is_some() {
            for wolf in &pack {
                if wolf.locomotion_tag == LOCOMOTION_EAT {
                    eaters.insert(wolf.creature_id);
                }
            }
            if eaters.len() >= 3 {
                break;
            }
        }
    }
    let (tick, _) = killed.expect("the pack never took the deer");
    eprintln!("deer taken at tick {tick}");
    assert!(stalked, "no wolf crept into its place on the ring");
    assert!(
        surrounded_before_kill,
        "the deer was never inside the pack's ring before the kill at {tick}"
    );
    assert!(
        eaters.len() >= 3,
        "only {} wolves ate the carcass",
        eaters.len()
    );
}

#[test]
fn a_pack_backs_away_from_a_bear() {
    let mut sim = simulation(&[]);
    // The pack and a bear born 12 m apart.
    let anchors = vec![[MIDDLE, 0.5, MIDDLE], [MIDDLE + 12.0, 0.5, MIDDLE]];
    let service = service(anchors, &[0, 0, 0, 1, 0, 4], 3);
    for _ in 0..900 {
        run(&mut sim, &service);
    }
    let bear = sim
        .creature_records()
        .into_iter()
        .find(|c| c.species == "bear")
        .expect("no bear")
        .translation_m;
    let pack = wolves(&sim);
    assert_eq!(pack.len(), 4);
    for wolf in pack {
        let distance = horizontal(wolf.translation_m, bear);
        assert!(
            distance > 18.0,
            "a wolf is only {distance:.1} m from the bear"
        );
    }
}

#[test]
fn wolves_keep_away_from_fire() {
    let lamp = [MIDDLE + 2.0, MIDDLE];
    let mut sim = simulation(&[lamp]);
    let service = service(vec![[MIDDLE, 0.5, MIDDLE]], &PACK, 5);
    let fire = [lamp[0], 0.5, lamp[1]];
    let mut closest = f64::INFINITY;
    for tick in 0..3_000 {
        run(&mut sim, &service);
        // Give them a few seconds to get clear of where they were born.
        if tick > 400 {
            assert_eq!(wolves(&sim).len(), 4, "the pack is not all there");
            for wolf in wolves(&sim) {
                closest = closest.min(horizontal(wolf.translation_m, fire));
            }
        }
    }
    assert!(
        closest > 6.0,
        "a wolf came within {closest:.1} m of the fire"
    );
}

/// A hungry pack and a player standing 25 m off, with no deer about, for
/// up to 6 minutes; returns the player's health after every change.
fn pack_and_player(lamps: &[[f64; 2]], player_at: [f64; 3]) -> Vec<u16> {
    let mut sim = simulation(lamps);
    let service = service(vec![[MIDDLE, 0.5, MIDDLE]], &PACK, 11);
    let player = player_entity();
    sim.add_player(player, player_at);
    let mut healths = vec![PLAYER_MAX_HEALTH];
    for _ in 0..60 {
        run(&mut sim, &service);
    }
    assert_eq!(wolves(&sim).len(), 4, "the pack is not all there");
    for _ in 0..21_600 {
        run(&mut sim, &service);
        // The player stands still.
        sim.teleport_player(player, player_at);
        let health = sim.player_health(player).unwrap();
        if healths.last() != Some(&health) {
            healths.push(health);
        }
    }
    healths
}

#[test]
fn a_hungry_pack_sometimes_hunts_a_player_and_gives_up_after_three_bites() {
    use sandbox::game::WOLF_BITE_DAMAGE;
    let healths = pack_and_player(&[], [MIDDLE - 25.0, 0.6, MIDDLE]);
    let bites: Vec<u16> = (0..=3)
        .map(|n| PLAYER_MAX_HEALTH - n * WOLF_BITE_DAMAGE)
        .collect();
    assert_eq!(healths, bites, "health after each bite");
}

#[test]
fn a_player_beside_a_fire_is_left_alone() {
    let player = [MIDDLE - 25.0, 0.6, MIDDLE];
    let healths = pack_and_player(&[[player[0] + 1.0, player[2]]], player);
    assert_eq!(
        healths,
        [PLAYER_MAX_HEALTH],
        "a wolf bit a player at a fire"
    );
}

#[test]
fn snow_leopards_with_no_mountain_do_not_keep_wolves_from_spawning() {
    // The flat field has no high or steep ground, so no snow leopard can be
    // placed; the species after it must still get its turns.
    let mut sim = simulation(&[]);
    let service = service(vec![[MIDDLE, 0.5, MIDDLE]], &[0, 0, 0, 0, 4, 4], 9);
    for _ in 0..200 {
        run(&mut sim, &service);
    }
    assert_eq!(wolves(&sim).len(), 4);
}
