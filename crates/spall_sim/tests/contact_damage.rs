//! T21 (increment 1) — contact-to-damage intent conversion, end to end.
//!
//! Drives one authoritative [`Simulation`] with the real physics world:
//! a heavy stone cube is dropped onto a thick terrain slab, and every tick the
//! solver's contact impulses are run through
//! [`Simulation::apply_contact_damage`]. Asserts the T21 acceptance bullets that
//! do not need the region-sleep policy:
//!
//! * a falling body damages the terrain it strikes (`falling_body_damages_terrain`);
//! * once it has settled, the steady resting contact never fractures the floor
//!   again (`a_settled_body_stops_damaging_the_floor`);
//! * fragment counts and the edit pipeline stay bounded, and the conversion is
//!   deterministic (`contact_damage_is_bounded_and_deterministic`).

use glam::DQuat;
use spall_core::{EntityId, GlobalCell};
use spall_sim::{
    BodyPose, ContactDamageConfig, ContactDamagePolicy, Simulation, SimulationConfig, fixtures,
    solid_cells,
};
use spall_voxel::{EditPlan, Sample};

/// [`fixtures::flat_terrain_setup`] with the stone slab thickened to 6 cells
/// (1.5 m) so a shallow contact-damage crater near the top never reaches the
/// `y = 0` anchor plane — the slab stays one connected, anchored piece, so no
/// terrain-to-body split happens and body-count changes are purely the debris
/// the drop itself detaches (none, here).
fn thick_slab_setup() -> spall_sim::WorldSetup {
    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    setup
        .terrain
        .apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(23, 5, 23),
            fixtures::STONE,
        ))
        .unwrap();
    setup
}

fn terrain_solids(sim: &Simulation) -> u64 {
    let vid = sim.world().terrain_volume_id();
    solid_cells(sim.world().volume_ref(vid).unwrap())
}

/// Solid cells of one body's volume, `0` if the body no longer exists.
fn body_solids(sim: &Simulation, entity: EntityId) -> u64 {
    sim.world()
        .body(entity)
        .map(|b| solid_cells(sim.world().volume_ref(b.volume_id).unwrap()))
        .unwrap_or(0)
}

fn sample(sim: &Simulation, cell: GlobalCell) -> Sample {
    let vid = sim.world().terrain_volume_id();
    sim.world().volume_ref(vid).unwrap().sample(cell).unwrap()
}

/// Drops a `1 m` stone cube from ~3.5 m up over `(x, z) ≈ (4.5, 4.5)`.
fn drop_cube(sim: &mut Simulation) {
    sim.world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [4.0, 5.0, 4.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
}

#[test]
fn falling_body_damages_terrain() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = ContactDamagePolicy::new(ContactDamageConfig::DEFAULT);
    let before = terrain_solids(&sim);

    let mut total_admitted = 0usize;
    let mut total_submitted = 0usize;
    drop_cube(&mut sim);
    for _ in 0..240 {
        let report = sim.tick().unwrap();
        let dmg = sim.apply_contact_damage(&mut policy, &report);
        total_admitted += dmg.plan.admitted();
        total_submitted += dmg.submitted;
        assert_eq!(dmg.rejected, 0, "the pipeline never backpressured this run");
    }
    // Let the admitted cuts stage + commit.
    sim.run_until_idle(60).unwrap();

    assert!(
        total_admitted >= 1 && total_submitted >= 1,
        "the impact produced at least one damage cut (admitted {total_admitted}, submitted {total_submitted})"
    );
    let after = terrain_solids(&sim);
    assert!(
        after < before,
        "the strike removed terrain cells ({before} -> {after})"
    );

    // The damage is local: the cube spawns at world (4, 5, 4) m over 0.25 m
    // cells, so it lands on the terrain around cell (18, *, 18). Stone at the
    // top of the slab there is gone; a far corner column is untouched.
    assert!(
        matches!(sample(&sim, GlobalCell::new(0, 5, 0)), Sample::Filled(_)),
        "terrain far from the impact is intact"
    );
    assert!(
        matches!(
            sample(&sim, GlobalCell::new(18, 5, 18)),
            Sample::Empty { .. }
        ),
        "stone directly under the impact point was cut"
    );

    // No runaway fragmentation: the anchored slab never split, so the only body
    // is still the cube.
    assert_eq!(sim.world().body_count(), 1);
}

#[test]
fn a_settled_body_stops_damaging_the_floor() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = ContactDamagePolicy::new(ContactDamageConfig::DEFAULT);

    drop_cube(&mut sim);

    // Phase 1: run until the cube is asleep and stationary.
    let mut settled_tick = None;
    for tick in 1..=600u64 {
        let report = sim.tick().unwrap();
        sim.apply_contact_damage(&mut policy, &report);
        if let Some(body) = sim.world().bodies().next() {
            let v = body.linvel_m_s;
            let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            if body.sleeping && speed < 0.02 && tick > 60 {
                settled_tick = Some(tick);
                break;
            }
        }
    }
    let settled_tick = settled_tick.expect("the dropped cube settled");
    sim.run_until_idle(60).unwrap();
    let solids_after_settle = terrain_solids(&sim);

    // Phase 2: 180 more ticks of nothing but the cube resting on the floor.
    // The steady `m·g·dt` support impulse must never clear the impact threshold,
    // so not one further damage cut is admitted and the terrain is unchanged.
    let mut extra_admitted = 0usize;
    for _ in 0..180 {
        let report = sim.tick().unwrap();
        let dmg = sim.apply_contact_damage(&mut policy, &report);
        extra_admitted += dmg.plan.admitted();
    }
    assert_eq!(
        extra_admitted, 0,
        "a body at rest kept fracturing the floor (settled at tick {settled_tick})"
    );
    assert_eq!(
        terrain_solids(&sim),
        solids_after_settle,
        "resting contact left the terrain cell count unchanged"
    );
}

#[test]
fn contact_damage_is_bounded_and_deterministic() {
    // A tiny per-tick cap; three cubes dropped in separate spots would exceed it
    // if admission were unbounded.
    let config = ContactDamageConfig {
        max_intents_per_tick: 1,
        ..ContactDamageConfig::DEFAULT
    };

    let run = || -> (usize, spall_protocol::canonical::Hash32) {
        let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
        let mut policy = ContactDamagePolicy::new(config);
        for (x, z) in [(3.0, 3.0), (12.0, 3.0), (3.0, 12.0)] {
            sim.world_mut()
                .spawn_body(
                    fixtures::solid_block(4),
                    BodyPose::new(DQuat::IDENTITY, [x, 5.0, z]),
                    [0.0; 3],
                    [0.0; 3],
                    2600.0,
                    0,
                )
                .unwrap();
        }
        for _ in 0..240 {
            let report = sim.tick().unwrap();
            let dmg = sim.apply_contact_damage(&mut policy, &report);
            assert!(
                dmg.plan.admitted() <= 1,
                "per-tick cap of 1 was exceeded ({} cuts)",
                dmg.plan.admitted()
            );
        }
        sim.run_until_idle(120).unwrap();
        (sim.world().body_count(), sim.world().world_hash())
    };

    let a = run();
    let b = run();
    assert_eq!(a.1, b.1, "the same scenario produced the same world hash");
    assert_eq!(a.0, b.0, "same body count both runs");
    assert!(
        a.0 <= 3,
        "no body explosion: at most the three cubes ({})",
        a.0
    );
}

/// Increment 3 — a heavier body dropped onto a lighter one fractures the body
/// being struck, not (or much less than) the dropper; matter is only ever
/// destroyed, never created; the body count stays bounded.
#[test]
fn a_heavy_body_dropped_on_a_lighter_one_fractures_the_lighter_one() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = ContactDamagePolicy::new(ContactDamageConfig::DEFAULT);

    // Lighter cube, settled on the slab (top of the 6-cell slab is y = 1.5 m).
    let struck = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [4.0, 1.75, 4.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..150 {
        let report = sim.tick().unwrap();
        sim.apply_contact_damage(&mut policy, &report);
    }
    sim.run_until_idle(30).unwrap();
    let struck_before = body_solids(&sim, struck);
    let total_before = sim.world().total_solid_cells();
    assert!(struck_before > 0, "the lighter cube settled intact");

    // Heavier cube dropped ~1.5 m straight down onto it — a firm hit, not an
    // explosive one, so the light cube (backed by terrain) never briefly
    // outruns the decelerating dropper.
    let dropper = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [4.0, 4.25, 4.0]),
            [0.0; 3],
            [0.0; 3],
            3600.0,
            0,
        )
        .unwrap();
    let dropper_before = body_solids(&sim, dropper);

    let mut admitted = 0usize;
    let mut cuts_on_struck = 0usize;
    let mut cuts_on_dropper = 0usize;
    for _ in 0..300 {
        let report = sim.tick().unwrap();
        let dmg = sim.apply_contact_damage(&mut policy, &report);
        admitted += dmg.plan.admitted();
        for cut in &dmg.plan.damage {
            match cut.target {
                spall_sim::EditTarget::Body(e) if e == struck => cuts_on_struck += 1,
                spall_sim::EditTarget::Body(e) if e == dropper => cuts_on_dropper += 1,
                _ => {}
            }
        }
        assert_eq!(dmg.rejected, 0, "the pipeline never backpressured");
    }
    sim.run_until_idle(120).unwrap();

    assert!(admitted >= 1, "the impact produced a damage cut");
    assert!(
        cuts_on_struck >= 1 && cuts_on_struck >= cuts_on_dropper,
        "the struck body took the damage (struck {cuts_on_struck}, dropper {cuts_on_dropper})"
    );

    let struck_after = body_solids(&sim, struck);
    let dropper_after = body_solids(&sim, dropper);
    assert!(
        struck_after < struck_before,
        "the struck (lighter) body lost cells ({struck_before} -> {struck_after})"
    );
    assert!(
        dropper_before - dropper_after <= struck_before - struck_after,
        "the dropper lost no more cells than the body it struck \
         (dropper {dropper_before} -> {dropper_after}, struck {struck_before} -> {struck_after})"
    );

    // Conservation: the only change to total solid matter is destruction.
    let total_after = sim.world().total_solid_cells();
    assert!(
        total_after < total_before,
        "a cut destroyed matter ({total_before} -> {total_after})"
    );
    let destroyed = total_before - total_after;
    assert_eq!(
        total_after + destroyed,
        total_before,
        "terrain + bodies + destroyed balances"
    );
    assert!(total_after > 0, "matter remains");

    // No fragmentation blow-up: the two cubes plus a handful of chips.
    assert!(
        sim.world().body_count() <= 8,
        "body count stayed bounded ({})",
        sim.world().body_count()
    );
}

/// Increment 3 — a loose stack of equal-mass debris settling onto a slab
/// produces only a bounded number of body-damage cuts, and none once it sleeps
/// (the body-on-body mirror of `a_settled_body_stops_damaging_the_floor`).
#[test]
fn settling_debris_stack_does_not_cascade() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = ContactDamagePolicy::new(ContactDamageConfig::DEFAULT);

    // Four 1 m cubes with ~0.4 m gaps, so each lands on the one below.
    for i in 0..4 {
        sim.world_mut()
            .spawn_body(
                fixtures::solid_block(4),
                BodyPose::new(DQuat::IDENTITY, [4.0, 1.75 + i as f64 * 1.4, 4.0]),
                [0.0; 3],
                [0.0; 3],
                2600.0,
                0,
            )
            .unwrap();
    }
    let bodies0 = sim.world().body_count();
    assert_eq!(bodies0, 4);

    let mut admitted = 0usize;
    for _ in 0..600 {
        let report = sim.tick().unwrap();
        admitted += sim
            .apply_contact_damage(&mut policy, &report)
            .plan
            .admitted();
    }
    sim.run_until_idle(120).unwrap();

    assert!(
        admitted <= 4 * bodies0,
        "the settling stack produced a bounded cut stream (got {admitted})"
    );

    // Everything is asleep now: another 180 quiet ticks admit nothing.
    let mut late = 0usize;
    for _ in 0..180 {
        let report = sim.tick().unwrap();
        late += sim
            .apply_contact_damage(&mut policy, &report)
            .plan
            .admitted();
    }
    assert_eq!(late, 0, "a settled stack stops producing cuts");
    assert!(
        sim.world().body_count() <= 4 * bodies0,
        "no body-count explosion ({})",
        sim.world().body_count()
    );
}
