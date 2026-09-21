//! Containment (T23 / G4): collision correctness and out-of-world lifecycle are
//! different assertions and are checked separately, from transformed occupied
//! geometry / collider bounds, never from a body's origin alone.

use spall_sim::containment::containment_census;
use spall_sim::fixtures::{self, solid_block};
use spall_sim::{BodyPose, Simulation, SimulationConfig};

fn scene() -> Simulation {
    Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap()
}

fn spawn(sim: &mut Simulation, at: [f64; 3], v: [f64; 3]) -> spall_core::EntityId {
    sim.world_mut()
        .spawn_body(
            solid_block(2),
            BodyPose::new(glam::DQuat::IDENTITY, at),
            v,
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap()
}

#[test]
fn a_body_resting_on_the_ground_is_neither_penetrating_nor_external() {
    let mut sim = scene();
    let e = spawn(&mut sim, [30.0, 1.0, 30.0], [0.0; 3]);
    for _ in 0..300 {
        sim.tick().unwrap();
    }
    let c = containment_census(sim.world(), 4096, 0.25);
    assert!(c.deep_penetrations.iter().all(|r| r.entity != e.get()));
    assert!(c.external.iter().all(|r| r.entity != e.get()));
}

/// Collision correctness: a body whose *cells* are embedded in the slab is flagged with
/// its depth, even though its origin (the cube's corner) is nowhere near the deepest
/// point. Checked before any step so the solver has not yet pushed it out.
#[test]
fn a_body_embedded_in_the_ground_is_a_penetration_not_an_external_body() {
    let mut sim = scene();
    let e = spawn(&mut sim, [30.0, 0.3, 30.0], [0.0; 3]);
    let c = containment_census(sim.world(), 4096, 0.25);
    let row = c
        .deep_penetrations
        .iter()
        .find(|r| r.entity == e.get())
        .expect("embedded body flagged");
    assert!(
        row.penetration_depth_m >= 0.25,
        "depth {}",
        row.penetration_depth_m
    );
    assert!(row.overlapped_cells > 0);
    assert!(c.external.iter().all(|r| r.entity != e.get()));
}

/// Out-of-world lifecycle: only a body wholly outside the world box is external. One
/// hanging over the slab edge, still inside the bounds, is not; once it has fallen below
/// the world floor it is, and it is *not* a penetration.
#[test]
fn only_a_body_wholly_outside_the_world_bounds_is_external() {
    let mut sim = scene();
    // Inside the 256 x 128 x 256 m world but past the 96 x 56 m slab: nothing under it.
    let e = spawn(&mut sim, [150.0, 5.0, 30.0], [0.0; 3]);
    let c = containment_census(sim.world(), 4096, 0.25);
    assert!(
        c.external.iter().all(|r| r.entity != e.get()),
        "in bounds at spawn"
    );
    for _ in 0..240 {
        sim.tick().unwrap();
    }
    let b = sim.world().body(e).unwrap();
    assert!(
        b.pose.translation_m[1] < -5.0,
        "it fell out of the world: {:?}",
        b.pose.translation_m
    );
    let c = containment_census(sim.world(), 4096, 0.25);
    let row = c
        .external
        .iter()
        .find(|r| r.entity == e.get())
        .expect("flagged external");
    assert!(row.velocity_m_s[1] < -10.0, "and is free-falling");
    assert!(c.deep_penetrations.iter().all(|r| r.entity != e.get()));
}

/// What exists today for an external body: identity and matter are conserved while it
/// falls out of the world (it stays an ordinary dynamic body — there is no external
/// set yet, see the ignored test below).
#[test]
fn an_escaping_body_keeps_its_identity_and_its_matter() {
    let mut sim = scene();
    let e = spawn(&mut sim, [150.0, 5.0, 30.0], [0.0; 3]);
    let before = sim.world().total_solid_cells();
    let volume_before = sim.world().body(e).unwrap().volume_id;
    for _ in 0..600 {
        sim.tick().unwrap();
    }
    let b = sim.world().body(e).expect("same entity id still resolves");
    assert_eq!(b.volume_id, volume_before);
    assert_eq!(
        sim.world().total_solid_cells(),
        before,
        "no matter created or destroyed"
    );
    assert!(
        !b.dormant,
        "today an external body is NOT moved to a dormant external set"
    );
}

/// README (line 65): "persist out-of-bounds debris in a dormant external-body set".
/// Declared, not implemented: an external body should leave the solver (dormant), keep
/// its identity/pose/velocity/matter in the checkpoint, and survive a restart.
#[test]
#[ignore = "declared by README, not implemented: no dormant external-body set exists yet"]
fn an_external_body_becomes_dormant_and_persists() {
    let mut sim = scene();
    let e = spawn(&mut sim, [150.0, 5.0, 30.0], [0.0; 3]);
    for _ in 0..600 {
        sim.tick().unwrap();
    }
    let b = sim.world().body(e).unwrap();
    assert!(b.dormant, "an external body must leave the solver");
}

/// A 3 m rod lying across the 1 m perimeter wall with **both ends in air**: no corner of its
/// bounds is in solid terrain, so a corner-only prefilter never looked at it.
fn rod(id: spall_core::VolumeId) -> spall_voxel::Volume {
    use spall_core::{CellSizeCode, GlobalCell};
    let mut v = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
    v.apply_edit(&spall_voxel::EditPlan::filled_box(
        id,
        GlobalCell::new(0, 0, 0),
        GlobalCell::new(11, 0, 0),
        fixtures::STONE,
    ))
    .unwrap();
    v
}

#[test]
fn a_body_straddling_a_thin_wall_with_both_ends_in_air_is_still_found() {
    let mut sim = scene();
    // West wall: x 0..1 m, y 1..7 m. The rod spans x -1..2 m at y 3 m.
    let e = sim
        .world_mut()
        .spawn_body(
            rod,
            BodyPose::new(glam::DQuat::IDENTITY, [-1.0, 3.0, 20.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let c = containment_census(sim.world(), 4096, 0.25);
    let row = c
        .deep_penetrations
        .iter()
        .find(|r| r.entity == e.get())
        .expect("straddling body flagged");
    assert!(row.overlapped_cells > 0);
    assert!(
        c.prefilter_passed >= 1 && c.skipped_aabb_too_large == 0 && c.skipped_budget == 0,
        "full coverage: {c:?}"
    );
}

#[test]
fn skipped_bodies_are_reported_never_silently_clear() {
    use spall_sim::containment::{CensusOptions, containment_census_with};
    let mut sim = scene();
    for i in 0..6 {
        spawn(&mut sim, [40.0 + 3.0 * f64::from(i), 5.0, 30.0], [0.0; 3]);
    }
    let base = CensusOptions {
        max_cells: 4096,
        min_depth_m: 0.25,
        max_aabb_samples: 1_000_000,
        sample_budget: u64::MAX,
        start_offset: 0,
    };
    let full = containment_census_with(sim.world(), base);
    assert_eq!(full.skipped_budget + full.skipped_aabb_too_large, 0);
    // A tiny per-body cap: every body is reported as skipped, not as clear.
    let tiny = containment_census_with(
        sim.world(),
        CensusOptions {
            max_aabb_samples: 2,
            ..base
        },
    );
    assert_eq!(tiny.skipped_aabb_too_large, tiny.bodies, "{tiny:?}");
    assert_eq!(tiny.prefilter_passed, 0);
    // A budget that fits about half: the rest are `skipped_budget`, and the next scan rotates on.
    let per_body = full.prefilter_samples / full.bodies.max(1);
    let half = containment_census_with(
        sim.world(),
        CensusOptions {
            sample_budget: per_body * full.bodies / 2,
            ..base
        },
    );
    assert!(
        half.skipped_budget > 0 && half.skipped_budget < half.bodies,
        "{half:?}"
    );
    assert!(
        half.next_offset > 0,
        "the next scan resumes at the first skipped body"
    );
}
