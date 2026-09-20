//! T23 / G4: why does asleep rubble wake? Deterministic regression tests for the
//! mass-wake root cause found with `SimWorld::enable_wake_audit`.
//!
//! **Root cause.** Every body edit rebuilds the edited body's collider
//! (`commit` -> `PhysicsWorld::rebuild_collider`), which wakes *every* body resting
//! on it, wherever the edit landed. In the G4 workload each comb-pole cut rebuilt
//! the comb's plate collider and woke the whole rubble pile lying on the plate (up
//! to 506 bodies in one commit; `edit.commit_publish` in the audit), and the
//! woken bodies then woke their neighbours through the contact islands (82,611
//! wake events, 853 in one physics step). Piles bombarded by an edit stream every
//! few seconds could never finish the ~2 s needed to sleep, so they never became
//! dormant. Dormancy deactivate/reactivate themselves woke nobody (0 of ~6,800
//! each) but churned continuously.
//!
//! The first two tests pin the mechanism and its locality; the third states the
//! required behaviour (a commit must wake only bodies whose support could have
//! changed) and is `#[ignore]`d until a targeted-wake change is authorised.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, solid_block};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::fixtures as vfix;

fn brush_cell(c: [i64; 3], radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            c[0] * BRUSH_UNIT + h,
            c[1] * BRUSH_UNIT + h,
            c[2] * BRUSH_UNIT + h,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

/// A comb standing on the ground at `at` (awake), plus a `4 x 4` pile of `2`-cell
/// cubes dropped onto its plate. Returns the comb and the cubes.
fn comb_with_pile(sim: &mut Simulation, at: [f64; 3]) -> (EntityId, Vec<EntityId>) {
    let comb = sim
        .world_mut()
        .spawn_body(
            vfix::g4_comb_body,
            BodyPose::new(glam::DQuat::IDENTITY, at),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let mut cubes = Vec::new();
    for i in 0..4 {
        for j in 0..4 {
            // Above the plate's top (0.5 m over the comb origin), clear of poles
            // (poles stand at odd cell coordinates; drop between them).
            let p = [
                at[0] + 0.55 + 0.9 * i as f64,
                at[1] + 0.9,
                at[2] + 0.55 + 0.9 * j as f64,
            ];
            cubes.push(
                sim.world_mut()
                    .spawn_body(
                        solid_block(2),
                        BodyPose::new(glam::DQuat::IDENTITY, p),
                        [0.0; 3],
                        [0.0; 3],
                        2600.0,
                        0,
                    )
                    .unwrap(),
            );
        }
    }
    (comb, cubes)
}

fn awake_count(sim: &Simulation, ids: &[EntityId]) -> usize {
    ids.iter()
        .filter(|e| {
            let b = sim.world().body(**e).unwrap();
            !b.dormant && !b.sleeping
        })
        .count()
}

fn settle(sim: &mut Simulation, ticks: u64) {
    for _ in 0..ticks {
        sim.tick().unwrap();
    }
}

fn cut_pole(sim: &mut Simulation, comb: EntityId, req: u64, y: i64) {
    sim.submit(EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Body(comb),
        brush_cell([1, y, 1], 1),
    ))
    .unwrap();
}

fn scene() -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    sim.world_mut().enable_wake_audit();
    sim
}

/// The mechanism: cutting a pole rebuilds the comb collider and wakes the pile
/// resting on the plate, and the audit attributes it to the commit.
#[test]
fn editing_a_comb_wakes_the_pile_resting_on_it() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(
        awake_count(&sim, &cubes),
        0,
        "the pile settles to sleep before the edit"
    );

    cut_pole(&mut sim, comb, 1, 3);
    sim.tick().unwrap(); // commit + step
    let woken = awake_count(&sim, &cubes);
    assert!(woken > 0, "the edit woke {woken} resting cubes");

    let audit = sim.world().wake_audit().unwrap();
    let commit = audit
        .reasons
        .iter()
        .find(|(k, _)| k.starts_with("edit.commit_publish"))
        .map(|(_, s)| *s)
        .expect("commit publish was probed");
    assert!(
        commit.bodies_woken > 0,
        "wake attributed to the commit: {commit:?}"
    );
}

/// Locality: the wake is per edited body, not global. A pile on a *different*
/// comb stays asleep through the same commit.
#[test]
fn an_edit_does_not_wake_an_unrelated_pile() {
    let mut sim = scene();
    let (comb_a, _pile_a) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    let (_comb_b, pile_b) = comb_with_pile(&mut sim, [50.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &pile_b), 0);

    cut_pole(&mut sim, comb_a, 1, 3);
    for _ in 0..30 {
        sim.tick().unwrap();
    }
    assert_eq!(
        awake_count(&sim, &pile_b),
        0,
        "an unrelated pile must stay asleep"
    );
}

/// Required behaviour, not yet true: a cut near the *top* of a pole cannot change
/// what supports a pile lying on the plate, so it must not wake that pile.
/// Fails today because the rebuild wakes everything in contact with the collider.
#[test]
#[ignore = "fails until a targeted-wake change (wake only bodies whose support may have changed) is authorised"]
fn a_commit_far_from_a_resting_pile_should_not_wake_it() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);

    // Top of the pole, ~9 m above the plate: cannot affect the plate's support.
    cut_pole(&mut sim, comb, 1, 38);
    for _ in 0..10 {
        sim.tick().unwrap();
    }
    assert_eq!(
        awake_count(&sim, &cubes),
        0,
        "the pile should have stayed asleep"
    );
}
