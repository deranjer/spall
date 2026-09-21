//! Per-brick terrain colliders (prototype): a distant terrain dig must leave an unrelated pile
//! asleep, removing real support must still wake and drop what rested on it, geometry and
//! collision across brick boundaries must match the single-collider terrain, and the colliders
//! must track the authoritative volume's revisions.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, solid_block};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};

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

fn scene(bricks: bool) -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    sim.world_mut().enable_wake_audit();
    if bricks {
        sim.world_mut().enable_terrain_brick_colliders().unwrap();
    }
    sim
}

fn dig(sim: &mut Simulation, req: u64, cell: [i64; 3], radius: i64) {
    sim.submit(EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(cell, radius),
    ))
    .unwrap();
}

fn cube(sim: &mut Simulation, at: [f64; 3]) -> EntityId {
    cube_moving(sim, at, [0.0; 3])
}

fn cube_moving(sim: &mut Simulation, at: [f64; 3], v: [f64; 3]) -> EntityId {
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

fn awake(sim: &Simulation, ids: &[EntityId]) -> usize {
    ids.iter()
        .filter(|e| {
            let b = sim.world().body(**e).unwrap();
            !b.sleeping && !b.dormant
        })
        .count()
}

/// A settled scatter of cubes: `n x n` cubes with `pitch` metres between centres.
fn scatter(sim: &mut Simulation, origin: [f64; 2], n: usize, pitch: f64) -> Vec<EntityId> {
    let mut ids = Vec::new();
    for i in 0..n {
        for j in 0..n {
            ids.push(cube(
                sim,
                [
                    origin[0] + pitch * i as f64,
                    1.05,
                    origin[1] + pitch * j as f64,
                ],
            ));
        }
    }
    ids
}

fn settle(sim: &mut Simulation, ticks: u64) {
    for _ in 0..ticks {
        sim.tick().unwrap();
    }
}

fn peak_awake_after(sim: &mut Simulation, ids: &[EntityId], ticks: u64) -> usize {
    let mut peak = 0;
    for _ in 0..ticks {
        sim.tick().unwrap();
        peak = peak.max(awake(sim, ids));
    }
    peak
}

#[test]
fn brick_colliders_cover_exactly_the_terrain_and_track_its_revisions() {
    let mut sim = scene(true);
    sim.world().validate_terrain_brick_colliders().unwrap();
    assert!(sim.world().terrain_brick_colliders_enabled());
    assert!(
        sim.world().terrain_physics_bodies().len() > 50,
        "one collider per solid brick"
    );
    // Publication is at the tick boundary: right after a tick that commits digs, every
    // collider matches the authoritative revision it was built from.
    // A brush must satisfy `centre y >= radius` on this bounded volume (its floor is y = 0), else
    // the edit is rejected; assert each dig really commits so the check is not vacuous.
    for (i, cell) in [[100, 2, 100], [230, 2, 120], [232, 2, 118]]
        .into_iter()
        .enumerate()
    {
        dig(&mut sim, i as u64 + 1, cell, 2);
        let report = sim.tick().unwrap();
        assert_eq!(report.committed.len(), 1, "dig {i}: {:?}", report.rejected);
        sim.world()
            .validate_terrain_brick_colliders()
            .unwrap_or_else(|e| panic!("after dig {i}: {e}"));
    }
}

#[test]
fn authoritative_terrain_is_identical_with_and_without_brick_colliders() {
    let mut hashes = Vec::new();
    for bricks in [false, true] {
        let mut sim = scene(bricks);
        for (i, cell) in [[100, 1, 100], [230, 1, 120], [40, 1, 200]]
            .into_iter()
            .enumerate()
        {
            dig(&mut sim, i as u64 + 1, cell, 2);
            sim.tick().unwrap();
        }
        hashes.push((
            sim.world().world_hash().to_string(),
            sim.world().total_solid_cells(),
        ));
    }
    assert_eq!(
        hashes[0], hashes[1],
        "the physics split never changes authoritative geometry"
    );
}

#[test]
fn a_distant_dig_leaves_an_unrelated_pile_asleep_where_the_single_collider_wakes_it() {
    let mut result = Vec::new();
    for bricks in [false, true] {
        let mut sim = scene(bricks);
        let pile = scatter(&mut sim, [50.0, 30.0], 8, 1.2);
        settle(&mut sim, 600);
        assert_eq!(
            awake(&sim, &pile),
            0,
            "the pile settles asleep (bricks={bricks})"
        );
        dig(&mut sim, 1, [160, 1, 160], 1); // ~45 m away
        result.push(peak_awake_after(&mut sim, &pile, 20));
    }
    assert_eq!(
        result[1], 0,
        "with brick colliders the distant dig wakes nothing"
    );
    assert!(
        result[0] > 0,
        "and the single whole-world collider does wake the pile ({} of 64): the comparison is meaningful",
        result[0]
    );
}

#[test]
fn removing_real_support_still_wakes_and_drops_what_rested_on_it() {
    let mut sim = scene(true);
    let pile = scatter(&mut sim, [50.0, 30.0], 8, 1.2);
    settle(&mut sim, 600);
    assert_eq!(awake(&sim, &pile), 0);
    let under = pile[0]; // at (50.0, 1.05, 30.0)
    let y0 = sim.world().body(under).unwrap().pose.translation_m[1];
    // A hole 1 m across in the slab under it, centred on the cube (cell = world / 0.25). A sphere
    // cannot go below the volume's floor, so it carves a 0.75 m-deep stepped bowl.
    dig(&mut sim, 1, [201, 4, 121], 4);
    let mut woke = false;
    let mut lowest = y0;
    for _ in 0..120 {
        sim.tick().unwrap();
        let b = sim.world().body(under).unwrap();
        woke |= !b.sleeping;
        lowest = lowest.min(b.pose.translation_m[1]);
    }
    assert!(woke, "the cube over the removed support woke");
    // It drops into the bowl; on a step it may wedge and be pushed back up, so judge the lowest
    // point reached, not the final pose.
    assert!(
        y0 - lowest > 0.4,
        "and fell into the hole: {y0:.2} -> lowest {lowest:.2}"
    );
    // Cubes in bricks the dig did not touch (x >= 56 m is brick 7; the dig is in brick 6) kept
    // sleeping.
    let far: Vec<EntityId> = pile
        .iter()
        .copied()
        .filter(|e| sim.world().body(*e).unwrap().pose.translation_m[0] >= 57.0)
        .collect();
    assert!(!far.is_empty());
    assert_eq!(
        awake(&sim, &far),
        0,
        "bricks the dig did not touch stay asleep"
    );
    sim.world().validate_terrain_brick_colliders().unwrap();
}

/// A body resting across the boundary between two bricks (x = 56 m) must sit and slide exactly
/// as it does on the single whole-world collider.
#[test]
fn collision_across_a_brick_boundary_matches_the_single_collider() {
    let mut rest = Vec::new();
    let mut slide = Vec::new();
    for bricks in [false, true] {
        let mut sim = scene(bricks);
        let straddler = cube(&mut sim, [55.75, 1.05, 45.0]);
        let slider = cube_moving(&mut sim, [55.4, 1.05, 50.0], [3.5, 0.0, 0.0]);
        settle(&mut sim, 300);
        let s = sim.world().body(straddler).unwrap();
        rest.push((s.pose.translation_m, s.sleeping));
        let d = sim.world().body(slider).unwrap();
        slide.push((d.pose.translation_m, d.sleeping));
    }
    for a in 0..3 {
        assert!(
            (rest[0].0[a] - rest[1].0[a]).abs() < 0.005,
            "rest {a}: {rest:?}"
        );
    }
    assert!(
        rest[1].1,
        "the straddler comes to rest asleep on two bricks: {rest:?} slide {slide:?}"
    );
    assert!(
        (rest[1].0[1] - 1.0).abs() < 0.3,
        "and sits on the ground, not in it: {rest:?}"
    );
    for a in 0..3 {
        assert!(
            (slide[0].0[a] - slide[1].0[a]).abs() < 0.05,
            "slide {a}: {slide:?}"
        );
    }
    assert!(
        slide[1].0[0] > 56.0,
        "the slider crossed the boundary: {slide:?}"
    );
}

#[test]
#[ignore = "diagnostic"]
fn probe_support_removal() {
    for bricks in [false, true] {
        let mut sim = scene(bricks);
        let pile = scatter(&mut sim, [50.0, 30.0], 8, 1.2);
        settle(&mut sim, 600);
        let under = pile[0];
        let p0 = sim.world().body(under).unwrap().pose.translation_m;
        dig(&mut sim, 1, [201, 4, 121], 4);
        let r = sim.tick().unwrap();
        println!(
            "bricks={bricks}: committed {} rejected {:?} pose0 {:?} sleeping {}",
            r.committed.len(),
            r.rejected,
            p0,
            sim.world().body(under).unwrap().sleeping
        );
        for _ in 0..120 {
            sim.tick().unwrap();
        }
        let b = sim.world().body(under).unwrap();
        println!(
            "   after 120: y {:.2} sleeping {}",
            b.pose.translation_m[1], b.sleeping
        );
    }
}

#[test]
#[ignore = "diagnostic"]
fn probe_support_removal_trace() {
    let mut sim = scene(true);
    let pile = scatter(&mut sim, [50.0, 30.0], 2, 1.2);
    settle(&mut sim, 600);
    let under = pile[0];
    dig(&mut sim, 1, [201, 4, 121], 4);
    for t in 0..60 {
        let r = sim.tick().unwrap();
        if t < 3 || t % 10 == 0 {
            let b = sim.world().body(under).unwrap();
            println!(
                "t{t} committed {} y {:.3} v {:?} sleeping {}",
                r.committed.len(),
                b.pose.translation_m[1],
                b.linvel_m_s,
                b.sleeping
            );
        }
    }
    for y in 0..5 {
        for (dx, dz) in [(0, 0), (1, 1), (-2, -2), (3, 3)] {
            let s = sim
                .world()
                .terrain()
                .volume
                .sample(spall_core::GlobalCell::new(200 + dx, y, 120 + dz));
            println!("  cell ({},{y},{}): {s:?}", 200 + dx, 120 + dz);
        }
    }
}

// --- empty / refill, multi-brick edits, collapse, character and contact seams, residency ---------

use spall_core::BrickCoord;
use spall_sim::EditKind;

fn place(sim: &mut Simulation, req: u64, cell: [i64; 3], radius: i64) {
    let mut intent = EditIntent::cut(
        RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush_cell(cell, radius),
    );
    intent.kind = EditKind::Place(fixtures::STONE);
    sim.submit(intent).unwrap();
}

fn cube_rest_y(sim: &mut Simulation, at: [f64; 3]) -> (f64, bool) {
    let id = cube(sim, at);
    settle(sim, 240);
    let b = sim.world().body(id).unwrap();
    (b.pose.translation_m[1], b.sleeping)
}

/// A flat slab plus a separate 8 x 8 x 2 cell island in brick (1, 0, 0), on an unbounded volume so
/// a brush may dip below y = 0 and clear the island completely.
fn island_setup() -> spall_sim::WorldSetup {
    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    setup
        .terrain
        .apply_edit(&spall_voxel::EditPlan::filled_box(
            id,
            spall_core::GlobalCell::new(32, 0, 0),
            spall_core::GlobalCell::new(39, 1, 7),
            fixtures::STONE,
        ))
        .unwrap();
    setup.terrain_collider_region.1 = spall_core::GlobalCell::new(39, 9, 23);
    setup
}

#[test]
fn a_brick_emptied_then_refilled_tracks_the_volume_and_supports_a_body_like_the_single_collider() {
    let brick = BrickCoord::new(1, 0, 0);
    let mut rests = Vec::new();
    for bricks in [false, true] {
        let mut sim = Simulation::new(SimulationConfig::new(island_setup())).unwrap();
        if bricks {
            sim.world_mut().enable_terrain_brick_colliders().unwrap();
            assert_eq!(sim.world().terrain_brick_has_collider(brick), Some(true));
        }
        // Clear the whole island: the brick becomes empty.
        dig(&mut sim, 1, [35, 1, 3], 8);
        let report = sim.tick().unwrap();
        assert_eq!(report.committed.len(), 1, "{:?}", report.rejected);
        if bricks {
            sim.world().validate_terrain_brick_colliders().unwrap();
            assert_eq!(
                sim.world().terrain_brick_has_collider(brick),
                Some(false),
                "an emptied brick keeps no collider"
            );
            // The neighbouring slab brick is untouched.
            assert_eq!(
                sim.world()
                    .terrain_brick_has_collider(BrickCoord::new(0, 0, 0)),
                Some(true)
            );
        }
        // Refill it; the same brick must get a collider again from the refilled geometry.
        place(&mut sim, 2, [35, 1, 3], 8);
        let report = sim.tick().unwrap();
        assert_eq!(report.committed.len(), 1, "{:?}", report.rejected);
        if bricks {
            sim.world().validate_terrain_brick_colliders().unwrap();
            assert_eq!(sim.world().terrain_brick_has_collider(brick), Some(true));
        }
        // The refill is a stone dome up to ~2.25 m high; drop a cube on its apex from above.
        rests.push(cube_rest_y(&mut sim, [8.75, 4.0, 0.75]));
    }
    assert_eq!(rests[0].1, rests[1].1, "same sleep state: {rests:?}");
    assert!(
        (rests[0].0 - rests[1].0).abs() < 0.02,
        "rest height matches the single collider: {rests:?}"
    );
    assert!(
        rests[1].0 > 0.4,
        "it rests on the refilled geometry, not through it: {rests:?}"
    );
}

#[test]
fn a_dig_spanning_four_bricks_rebuilds_only_those_and_matches_the_single_collider() {
    let far = [BrickCoord::new(0, 0, 0), BrickCoord::new(6, 0, 4)];
    let mut hashes = Vec::new();
    let mut before = None;
    for bricks in [false, true] {
        let mut sim = scene(bricks);
        if bricks {
            before = Some(far.map(|c| {
                sim.world()
                    .terrain()
                    .volume
                    .brick_revision(c)
                    .ok()
                    .flatten()
            }));
        }
        // Centre on the four-brick corner (cell 96, 96).
        dig(&mut sim, 1, [96, 10, 96], 10);
        let report = sim.tick().unwrap();
        assert_eq!(report.committed.len(), 1, "{:?}", report.rejected);
        if bricks {
            sim.world().validate_terrain_brick_colliders().unwrap();
            let after = far.map(|c| {
                sim.world()
                    .terrain()
                    .volume
                    .brick_revision(c)
                    .ok()
                    .flatten()
            });
            assert_eq!(
                before.unwrap(),
                after,
                "untouched bricks keep their revisions"
            );
        }
        hashes.push(sim.world().world_hash());
    }
    assert_eq!(hashes[0], hashes[1], "authoritative topology is identical");
}

#[test]
fn a_cross_brick_collapse_detaches_the_same_beam_and_rests_like_the_single_collider() {
    let script: [(u64, [i64; 3], i64); 4] = [
        (4, [31, 4, 1], 3),
        (8, [32, 4, 2], 3),
        (16, [21, 1, 1], 1),
        (24, [43, 1, 1], 1),
    ];
    let mut out = Vec::new();
    for bricks in [false, true] {
        let mut sim =
            Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();
        if bricks {
            sim.world_mut().enable_terrain_brick_colliders().unwrap();
        }
        let mut next = 0;
        for tick in 1..=400u64 {
            if next < script.len() && script[next].0 == tick {
                let (_, cell, radius) = script[next];
                let _ = sim.submit(EditIntent::cut(
                    RequestId(next as u64 + 1),
                    EntityId::new(1).unwrap(),
                    EditTarget::Terrain,
                    brush_cell(cell, radius),
                ));
                next += 1;
            }
            sim.tick().unwrap();
            if bricks {
                sim.world()
                    .validate_terrain_brick_colliders()
                    .unwrap_or_else(|e| panic!("tick {tick}: {e}"));
            }
        }
        let beam = sim.world().bodies().next().expect("beam detached");
        out.push((
            sim.world().body_count(),
            beam.pose.translation_m,
            beam.sleeping,
            sim.world().world_hash(),
        ));
    }
    assert_eq!(out[0].0, 1);
    assert_eq!(
        out[0].0, out[1].0,
        "same number of detached bodies: {out:?}"
    );
    assert!(
        out[1].2,
        "the beam settles asleep on the remaining floor: {out:?}"
    );
    for a in 0..3 {
        assert!(
            (out[0].1[a] - out[1].1[a]).abs() < 0.05,
            "resting pose axis {a}: {out:?}"
        );
    }
    assert_eq!(out[0].3, out[1].3, "authoritative topology identical");
}

#[test]
fn a_walking_player_crosses_brick_seams_like_on_the_single_collider() {
    use spall_core::{PlayerInput, player_entity_for};
    use spall_protocol::InputSeq;
    use spall_sim::fixtures::{WALK_ARENA_SPAWNS, walk_arena_setup};
    let mut ends = Vec::new();
    for bricks in [false, true] {
        let mut sim = Simulation::new(SimulationConfig::new(walk_arena_setup())).unwrap();
        if bricks {
            sim.world_mut().enable_terrain_brick_colliders().unwrap();
        }
        let player = player_entity_for(0);
        // Spawn at x = 7 m, past the row-15 near-origin defect (see G3.md).
        let _ = WALK_ARENA_SPAWNS;
        sim.add_player(player, [7.0, 1.0, 1.5]);
        let mut min_y = f64::MAX;
        let mut crossed = [false; 2];
        for seq in 1..=300u64 {
            sim.set_player_input(
                player,
                PlayerInput {
                    movement: [0.0, 0.0, 1.0],
                    view_dir: [1.0, 0.0, 0.0],
                    buttons: 0,
                },
                InputSeq(seq),
            );
            sim.tick().unwrap();
            let s = sim.player_state(player).unwrap();
            if seq > 5 {
                min_y = min_y.min(s.position_m[1]);
            }
            crossed[0] |= s.position_m[0] > 8.5;
            crossed[1] |= s.position_m[0] > 16.5;
            // Stay on the 30 m lane: 300 ticks ends near x = 28 m, before its far edge.
        }
        let s = sim.player_state(player).unwrap();
        ends.push((s.position_m, s.grounded, min_y, crossed));
    }
    assert!(ends[1].3 == [true, true], "crossed two seams: {ends:?}");
    assert!(ends[1].1, "grounded at the end: {ends:?}");
    assert!(ends[1].2 > 0.9, "never dropped through a seam: {ends:?}");
    // Same path within 0.2 m along the lane and 0.05 m in height (the two colliders are built
    // from the same cells but the player's terrain window differs in float noise).
    assert!((ends[0].0[0] - ends[1].0[0]).abs() < 0.2, "{ends:?}");
    assert!((ends[0].0[1] - ends[1].0[1]).abs() < 0.05, "{ends:?}");
    assert!((ends[0].0[2] - ends[1].0[2]).abs() < 0.05, "{ends:?}");
}

#[test]
fn contacts_against_brick_colliders_are_recognised_as_terrain_contacts() {
    let mut sim = scene(true);
    let id = cube_moving(&mut sim, [55.75, 3.0, 45.0], [0.0, -4.0, 0.0]);
    let phys = sim.world().body(id).unwrap().phys;
    let mut saw_terrain_contact = false;
    for _ in 0..120 {
        sim.tick().unwrap();
        for c in sim.world().physics().contact_impulses() {
            if c.bodies[0] != phys && c.bodies[1] != phys {
                continue;
            }
            let other = if c.bodies[0] == phys {
                c.bodies[1]
            } else {
                c.bodies[0]
            };
            if sim.world().is_terrain_physics_body(other) {
                saw_terrain_contact = true;
            }
        }
    }
    assert!(
        saw_terrain_contact,
        "the falling cube touching a brick collider is a terrain contact"
    );
}

#[test]
fn residency_combinations_are_rejected_explicitly() {
    // Enabling with a residency backing installed is refused.
    let mut sim = scene(false);
    sim.world_mut().set_backing(std::sync::Arc::new(
        spall_sim::backing::MemoryBacking::default(),
    ));
    let err = sim
        .world_mut()
        .enable_terrain_brick_colliders()
        .unwrap_err();
    assert!(err.to_string().contains("residency"), "{err}");
    assert!(!sim.world().terrain_brick_colliders_enabled());

    // With brick colliders on, evicting a terrain brick is refused and nothing changes.
    let mut sim = scene(true);
    let terrain = sim.world().terrain().volume_id;
    let err = sim
        .world_mut()
        .evict_brick(terrain, BrickCoord::new(2, 0, 2))
        .unwrap_err();
    assert!(
        err.to_string().contains("do not support terrain eviction"),
        "{err}"
    );
    sim.world().validate_terrain_brick_colliders().unwrap();
}
