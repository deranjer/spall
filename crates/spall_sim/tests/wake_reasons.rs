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
//! The first two tests pin the mechanism and its locality between edited bodies. The
//! forced re-sleep that once kept a far cut from waking its own pile was **withdrawn**
//! (increment 40): aggregate group balance does not prove independent-body stability
//! (`a_lever_on_a_separate_broad_dynamic_base_still_tips_when_unbalanced`), so the far-cut
//! locality test is `#[ignore]`d and every commit wakes its edited body's contact partners.
//! The remaining tests are correctness regressions any wake-locality scheme must keep.

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
        .find(|(k, _)| k.starts_with("edit.commit.parent_collider_publish"))
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

/// A cut near the *top* of a pole cannot change what supports a pile lying on the
/// plate, so it must not wake that pile (or the comb it rests on). Regression for
/// the mass-wake root cause: the collider rebuild used to wake every body in
/// contact with the edited body wherever the cut landed.
#[test]
#[ignore = "locality goal: needs a forced re-sleep, which was withdrawn (see docs/reports/G3.md increment 40)"]
fn a_commit_far_from_a_resting_pile_does_not_wake_it() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);

    // Top of the pole, ~9 m above the plate: cannot affect the plate's support.
    cut_pole(&mut sim, comb, 1, 38);
    for _ in 0..10 {
        sim.tick().unwrap();
    }
    assert_eq!(awake_count(&sim, &cubes), 0, "the pile stayed asleep");
    assert!(
        sim.world().body(comb).unwrap().sleeping,
        "the comb itself kept sleeping: none of its contacts changed"
    );
    let commit = sim
        .world()
        .wake_audit()
        .unwrap()
        .reasons
        .iter()
        .find(|(k, _)| k.starts_with("edit.commit.parent_collider_publish"))
        .map(|(_, s)| *s)
        .unwrap();
    assert_eq!(commit.bodies_woken, 0, "the commit woke nobody: {commit:?}");
}

/// The converse, which must stay true: a body whose actual support was removed
/// wakes and falls. A hole cut through the plate under one cube wakes that cube,
/// which drops to the ground, while a cube on the far side of the plate stays put.
#[test]
fn removing_the_support_under_a_resting_body_wakes_it_and_it_falls() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);
    let under = cubes[0];
    let far = cubes[15];
    let y0 = sim.world().body(under).unwrap().pose.translation_m[1];

    // A 1 m radius hole through the plate under the first cube (plate cells y 0..1;
    // the cube spans plate cells x 2..3, z 2..3).
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Body(comb),
        brush_cell([3, 1, 3], 4),
    ))
    .unwrap();
    let mut woke = false;
    for _ in 0..90 {
        sim.tick().unwrap();
        woke |= !sim.world().body(under).unwrap().sleeping;
    }
    let y1 = sim.world().body(under).unwrap().pose.translation_m[1];
    assert!(woke, "the cube over the removed support woke");
    assert!(
        y0 - y1 > 0.3,
        "and fell through the hole: {y0:.2} -> {y1:.2}"
    );
    // The hole also removes part of the plate's own ground contact, so the comb
    // (and, through it, the pile) may legitimately wake; what must hold is that the
    // cube far from the hole is still resting on the plate.
    let far_y = sim.world().body(far).unwrap().pose.translation_m[1];
    assert!(
        (far_y - y0).abs() < 0.2,
        "the far cube stayed on the plate: {far_y:.2} vs {y0:.2}"
    );
}

/// Terrain digs use the same rule: a dig far from a pile on the ground leaves it
/// asleep, a dig right under it wakes it.
#[test]
fn terrain_digs_wake_only_bodies_near_the_dig() {
    let mut sim = scene();
    let mut pile = Vec::new();
    for i in 0..3 {
        for j in 0..3 {
            pile.push(
                sim.world_mut()
                    .spawn_body(
                        solid_block(2),
                        BodyPose::new(
                            glam::DQuat::IDENTITY,
                            [60.0 + 0.6 * i as f64, 1.05, 40.0 + 0.6 * j as f64],
                        ),
                        [0.0; 3],
                        [0.0; 3],
                        2600.0,
                        0,
                    )
                    .unwrap(),
            );
        }
    }
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &pile), 0, "the pile settled");

    // A dig ~20 m away.
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell([160, 1, 160], 1),
    ))
    .unwrap();
    for _ in 0..30 {
        sim.tick().unwrap();
    }
    assert_eq!(awake_count(&sim, &pile), 0, "a distant dig woke nothing");

    // A dig under the first cube (world x 60.0..60.5, z 40.0..40.5 -> cell 240, 160).
    sim.submit(EditIntent::cut(
        RequestId(2),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell([241, 2, 161], 2),
    ))
    .unwrap();
    let mut woke = 0;
    for _ in 0..30 {
        sim.tick().unwrap();
        woke = woke.max(awake_count(&sim, &pile));
    }
    assert!(woke > 0, "a dig under the pile woke the bodies above it");
}

/// A far cut leaves the pile tracked for a forced re-sleep for a few steps. A later
/// edit that legitimately removes support inside that window must still wake the
/// body and let it fall.
#[test]
fn a_harmless_far_cut_then_support_removal_still_wakes_and_falls() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);
    let under = cubes[0];
    let y0 = sim.world().body(under).unwrap().pose.translation_m[1];

    cut_pole(&mut sim, comb, 1, 38);
    sim.tick().unwrap(); // far cut commits; pile is tracked for re-sleep
    sim.submit(EditIntent::cut(
        RequestId(2),
        EntityId::new(1).unwrap(),
        EditTarget::Body(comb),
        brush_cell([3, 1, 3], 4),
    ))
    .unwrap();
    for _ in 0..90 {
        sim.tick().unwrap();
    }
    let y1 = sim.world().body(under).unwrap().pose.translation_m[1];
    assert!(
        y0 - y1 > 0.3,
        "support removal inside the window still drops the cube"
    );
}

/// A small linear impulse on a tracked body inside the window must survive: the
/// body keeps the velocity it was given.
#[test]
fn a_small_impulse_during_the_resleep_window_is_not_erased() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    // A lone cube far from the comb, resting on the ground.
    let lone = sim
        .world_mut()
        .spawn_body(
            solid_block(2),
            BodyPose::new(glam::DQuat::IDENTITY, [50.0, 1.0, 30.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);
    assert!(sim.world().body(lone).unwrap().sleeping);

    cut_pole(&mut sim, comb, 1, 38);
    sim.tick().unwrap();
    let phys = sim.world().body(lone).unwrap().phys;
    sim.world_mut()
        .physics_mut()
        .apply_impulse(phys, [100.0, 0.0, 0.0]); // ~0.3 m/s
    let mut speed = 0.0f64;
    for _ in 0..3 {
        sim.tick().unwrap();
        let v = sim.world().body(lone).unwrap().linvel_m_s;
        speed = speed.max((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt());
    }
    assert!(
        speed > 0.05,
        "the impulse survived the steps (peak speed {speed})"
    );
}

/// Torque on the edited body inside the window must survive too.
#[test]
fn a_torque_impulse_during_the_resleep_window_is_not_erased() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);

    cut_pole(&mut sim, comb, 1, 38);
    sim.tick().unwrap();
    let phys = sim.world().body(comb).unwrap().phys;
    sim.world_mut()
        .physics_mut()
        .apply_torque_impulse(phys, [0.0, 3.0e6, 0.0]);
    sim.tick().unwrap();
    let w = sim.world().body(comb).unwrap().angvel_rad_s;
    let spin = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
    assert!(spin > 0.005, "the torque survived the step (spin {spin})");
    assert!(!sim.world().body(comb).unwrap().sleeping);
    for _ in 0..6 {
        sim.tick().unwrap();
    }
    assert!(
        !sim.world().body(comb).unwrap().sleeping,
        "still awake after the window"
    );
}

/// A long lever balanced on a 0.25 m wide base (`x` cell 30): an edit that touches no
/// support contact can still tip it by moving the centre of mass.
fn lever(id: spall_core::VolumeId) -> spall_voxel::Volume {
    use spall_core::{CellSizeCode, GlobalCell};
    let mut v = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
    for (lo, hi) in [
        ([30, 0, 8], [30, 3, 15]), // base: one cell (0.25 m) wide
        ([9, 4, 8], [51, 11, 15]), // beam, centred on the base
    ] {
        v.apply_edit(&spall_voxel::EditPlan::filled_box(
            id,
            GlobalCell::new(lo[0], lo[1], lo[2]),
            GlobalCell::new(hi[0], hi[1], hi[2]),
            fixtures::STONE,
        ))
        .unwrap();
    }
    v
}

fn settled_lever() -> (Simulation, EntityId, [f32; 4]) {
    let mut sim = scene();
    let body = sim
        .world_mut()
        .spawn_body(
            lever,
            BodyPose::new(glam::DQuat::IDENTITY, [30.0, 1.0, 30.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 600);
    assert!(
        sim.world().body(body).unwrap().sleeping,
        "the balanced lever sleeps"
    );
    let q = sim
        .world()
        .body(body)
        .unwrap()
        .pose
        .rotation
        .to_array()
        .map(|v| v as f32);
    (sim, body, q)
}

fn tilted(sim: &Simulation, body: EntityId, q0: [f32; 4]) -> f32 {
    let q = sim
        .world()
        .body(body)
        .unwrap()
        .pose
        .rotation
        .to_array()
        .map(|v| v as f32);
    (0..4).map(|i| (q[i] - q0[i]).abs()).fold(0.0, f32::max)
}

/// Removing a counterweight far from every support contact moves the centre of mass
/// off the base: the lever must wake and tip, not be put back to sleep.
#[test]
fn removing_a_counterweight_that_unbalances_a_body_wakes_and_tips_it() {
    let (mut sim, body, q0) = settled_lever();
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Body(body),
        brush_cell([13, 7, 11], 4),
    ))
    .unwrap();
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    assert!(tilted(&sim, body, q0) > 0.02, "the lever tipped over");
}

/// Same for a material change with no geometry change near the support: replacing the
/// stone at one end with lighter dirt unbalances it.
#[test]
fn a_material_density_change_that_unbalances_a_body_wakes_and_tips_it() {
    let (mut sim, body, q0) = settled_lever();
    sim.submit(EditIntent {
        request_id: RequestId(1),
        actor: EntityId::new(1).unwrap(),
        target: EditTarget::Body(body),
        kind: spall_sim::EditKind::Place(fixtures::DIRT),
        brush: brush_cell([13, 7, 11], 4),
        explosion: None,
    })
    .unwrap();
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    assert!(tilted(&sim, body, q0) > 0.02, "the lever tipped over");
}

/// The converse: an edit at the lever's end that keeps it balanced (a cut at the very
/// tip) leaves it asleep.
/// Forced re-sleep must never trap an awake neighbour: bodies dropped onto a tracked
/// pile in the same tick as a far cut must land on it and come to rest, not be
/// pressed through the ground (the first version sank ~200 rubble rods through the
/// slab and out of the world in the full workload).
#[test]
fn an_awake_body_landing_on_a_tracked_pile_is_not_pressed_through_the_floor() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    assert_eq!(awake_count(&sim, &cubes), 0);

    let mut drops = Vec::new();
    for i in 0..6 {
        drops.push(
            sim.world_mut()
                .spawn_body(
                    solid_block(2),
                    BodyPose::new(
                        glam::DQuat::IDENTITY,
                        [30.4 + 0.55 * i as f64, 3.2 + 0.6 * i as f64, 30.9],
                    ),
                    [0.0; 3],
                    [0.0, -3.0, 0.0],
                    2600.0,
                    0,
                )
                .unwrap(),
        );
    }
    cut_pole(&mut sim, comb, 1, 38);
    for _ in 0..400 {
        sim.tick().unwrap();
    }
    for e in drops.iter().chain(cubes.iter()) {
        let y = sim.world().body(*e).unwrap().pose.translation_m[1];
        assert!(y > 0.9, "body {e:?} stayed above the slab (y = {y:.2})");
    }
    let comb_y = sim.world().body(comb).unwrap().pose.translation_m[1];
    assert!(
        comb_y > 0.9,
        "the comb stayed on the slab (y = {comb_y:.2})"
    );
}

#[test]
fn a_balanced_edit_on_the_lever_leaves_it_asleep() {
    let (mut sim, body, _q0) = settled_lever();
    // One-cell nibble on the outer edge of the beam: tiny mass and COM change.
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Body(body),
        brush_cell([51, 8, 11], 1),
    ))
    .unwrap();
    for _ in 0..30 {
        sim.tick().unwrap();
    }
    assert!(
        sim.world().body(body).unwrap().sleeping,
        "a negligible edit keeps the lever asleep"
    );
}

#[test]
#[ignore = "diagnostic"]
fn debug_far_cut_audit() {
    let mut sim = scene();
    let (comb, cubes) = comb_with_pile(&mut sim, [30.0, 1.0, 30.0]);
    settle(&mut sim, 600);
    println!(
        "comb asleep before: {}",
        sim.world().body(comb).unwrap().sleeping
    );
    cut_pole(&mut sim, comb, 1, 38);
    for k in 0..8 {
        sim.tick().unwrap();
        println!(
            "tick {k}: comb asleep {}, awake cubes {}, bodies {}",
            sim.world().body(comb).unwrap().sleeping,
            awake_count(&sim, &cubes),
            sim.world().body_count()
        );
    }
    for (k, s) in &sim.world().wake_audit().unwrap().reasons {
        println!("{k}: {s:?}");
    }
}

/// Review reproduction (2026-09-20): a stable broad dynamic base must not mask an
/// unstable lever resting on it; the two bodies are not welded, so aggregate
/// balance proves nothing about the lever. This is why forced re-sleep was withdrawn.
#[test]
fn a_lever_on_a_separate_broad_dynamic_base_still_tips_when_unbalanced() {
    use spall_core::{CellSizeCode, GlobalCell};
    let mut sim = scene();
    let base = sim
        .world_mut()
        .spawn_body(
            |id| {
                let mut v = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
                v.apply_edit(&spall_voxel::EditPlan::filled_box(
                    id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(63, 3, 47),
                    fixtures::STONE,
                ))
                .unwrap();
                v
            },
            BodyPose::new(glam::DQuat::IDENTITY, [28.0, 1.0, 28.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let body = sim
        .world_mut()
        .spawn_body(
            lever,
            BodyPose::new(glam::DQuat::IDENTITY, [30.0, 2.0, 30.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 600);
    assert!(sim.world().body(base).unwrap().sleeping, "base settled");
    assert!(sim.world().body(body).unwrap().sleeping, "lever settled");
    let q0 = sim
        .world()
        .body(body)
        .unwrap()
        .pose
        .rotation
        .to_array()
        .map(|v| v as f32);
    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Body(body),
        brush_cell([13, 7, 11], 4),
    ))
    .unwrap();
    for _ in 0..120 {
        sim.tick().unwrap();
    }
    let angle = tilted(&sim, body, q0);
    println!(
        "tilt delta {angle}, asleep {}",
        sim.world().body(body).unwrap().sleeping
    );
    assert!(
        angle > 0.02,
        "unbalanced lever must tip independently of its stable base"
    );
}
