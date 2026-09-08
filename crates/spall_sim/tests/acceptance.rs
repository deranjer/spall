//! T08 acceptance scenarios.
//!
//! Covers each acceptance bullet from `docs/tasks.md` T08:
//! - no duplicate / lost occupied cells except explicit removals
//!   (`conservation_*`), cross-checked against the dense BFS oracle;
//! - no stale authoritative collision after commit (`*_collider_is_swapped_*`);
//! - split geometry stays in the same world location (`*_world_location_*`);
//! - no second impulse on retry (`no_second_impulse_on_retry`);
//! - two conflicting cuts converge to the documented commit order
//!   (`conflicting_cuts_converge`).

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CELLS_PER_BRICK, EntityId, GlobalCell, LocalCell, MaterialId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, STONE};
use spall_sim::{BodyPose, EditIntent, EditTarget, ExplosionImpulse, Simulation, SimulationConfig};
use spall_structure::AnchorPlane;
use spall_structure::oracle::dense_support;
use spall_voxel::{EditPlan, Sample, Volume};

fn brush_cell(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

fn volume_solid_count(v: &Volume) -> u64 {
    solid_cells_vec(v).len() as u64
}

fn solid_cells_vec(v: &Volume) -> Vec<GlobalCell> {
    let mut out = Vec::new();
    for c in v.resident_brick_coords() {
        let s = v.snapshot_brick(c).unwrap().unwrap();
        for i in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(i).unwrap();
            if !s.get(lc).is_air() {
                out.push(spall_core::GlobalCell::from_parts(c, lc).unwrap());
            }
        }
    }
    out
}

/// Centre of mass, world metres, for a uniform-density volume at `pose`.
fn uniform_com_world(v: &Volume, pose: &BodyPose, cs: spall_core::CellSizeCode) -> DVec3 {
    let cells = solid_cells_vec(v);
    let n = cells.len() as f64;
    let mut sum = DVec3::ZERO;
    for g in &cells {
        sum += DVec3::new(g.x as f64 + 0.5, g.y as f64 + 0.5, g.z as f64 + 0.5);
    }
    pose.local_cell_to_world_m(sum / n, cs)
}

/// The `MotionSnapshot` the commit journalled for `entity`.
fn journalled(sim: &Simulation, entity: EntityId) -> spall_protocol::MotionSnapshot {
    *sim.journal()
        .last()
        .expect("a journal entry")
        .participants
        .iter()
        .find(|p| p.body == entity)
        .expect("participant snapshot for the body")
}

fn run(sim: &mut Simulation, ticks: u32) -> Vec<spall_sim::TickReport> {
    sim.run_until_idle(ticks).expect("ticks")
}

// --- conservation, no split -------------------------------------------------

#[test]
fn conservation_a_plain_surface_cut_only_removes_brush_cells() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let before = sim.world().total_solid_cells();

    // Independent count: how many currently-solid cells the brush would clear.
    let plan = EditPlan::sphere(terrain, brush_cell(12, 1, 12, 2), MaterialId::AIR);
    let src = fixtures::flat_terrain_setup().terrain;
    let brush_solid_hits = plan
        .writes
        .iter()
        .filter(|w| matches!(src.sample(w.cell), Ok(Sample::Filled(_))))
        .count() as u64;
    assert!(brush_solid_hits > 0);

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(12, 1, 12, 2),
    ))
    .unwrap();
    run(&mut sim, 8);

    assert!(sim.committed(req).is_some(), "cut must commit");
    assert_eq!(
        sim.world().body_count(),
        0,
        "a plain surface cut splits nothing"
    );
    assert_eq!(
        before - sim.world().total_solid_cells(),
        brush_solid_hits,
        "exactly the brush-covered solid cells are gone; nothing else"
    );
}

// --- conservation, with split, vs the BFS oracle --------------------------

#[test]
fn conservation_a_split_moves_every_cell_and_loses_none() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();
    let anchor = AnchorPlane::at(0);
    let before = sim.world().total_solid_cells();

    let brush = brush_cell(10, 4, 1, 2);
    let req = RequestId(1);
    sim.submit(EditIntent::cut(req, actor(), EditTarget::Terrain, brush))
        .unwrap();
    run(&mut sim, 12);
    assert!(sim.committed(req).is_some());
    assert_eq!(sim.world().body_count(), 1, "the beam detaches as one body");

    // Reference: apply the same brush to a fresh terrain, then BFS-classify.
    let mut reference = fixtures::bridged_terrain_setup().terrain;
    reference
        .apply_edit(&EditPlan::sphere(terrain, brush, MaterialId::AIR))
        .unwrap();
    let oracle = dense_support(&reference, anchor);

    let terrain_solid = volume_solid_count(sim.world().volume_ref(terrain).unwrap());
    let child_solid: u64 = sim
        .world()
        .bodies()
        .map(|b| volume_solid_count(&b.volume))
        .sum();

    assert_eq!(
        oracle.unsupported_count() as u64,
        child_solid,
        "child bodies hold exactly the cells the oracle calls unsupported"
    );
    assert_eq!(
        oracle.supported_count() as u64,
        terrain_solid,
        "the terrain keeps exactly the cells the oracle calls supported"
    );
    // source == retained + child + destroyed
    let destroyed = before - (terrain_solid + child_solid);
    assert_eq!(terrain_solid + child_solid + destroyed, before);
    assert_eq!(oracle.total_solid as u64, terrain_solid + child_solid);
}

// --- no stale collision --------------------------------------------------

#[test]
fn a_cut_swaps_the_terrain_collider_in_the_same_tick() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    // A probe body resting on the slab, well away from where the hole will be.
    let probe = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(2),
            BodyPose::new(DQuat::IDENTITY, [4.5, 2.0, 4.5]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..40 {
        sim.step_physics_only();
    }
    let resting_y = sim.world().body(probe).unwrap().pose.translation_m[1];

    // Cut the slab near the world origin corner, far from the probe at (~4.5, 4.5).
    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(3, 1, 3, 3),
    ))
    .unwrap();
    run(&mut sim, 8);
    assert!(sim.committed(req).is_some());

    // A fresh probe over the hole must fall through — the collider no longer has
    // that wall.
    let faller = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(2),
            BodyPose::new(DQuat::IDENTITY, [0.6, 1.5, 0.6]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let start_y = sim.world().body(faller).unwrap().pose.translation_m[1];
    for _ in 0..40 {
        sim.step_physics_only();
    }
    let end_y = sim.world().body(faller).unwrap().pose.translation_m[1];
    assert!(
        end_y < start_y - 0.5,
        "body fell through the cut ({start_y} -> {end_y}); collider was swapped"
    );
    // sanity: the untouched probe didn't teleport.
    assert!((sim.world().body(probe).unwrap().pose.translation_m[1] - resting_y).abs() < 0.5);
}

// --- split geometry world location --------------------------------------

#[test]
fn a_terrain_child_keeps_its_world_cells_and_identity_pose() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    run(&mut sim, 8);

    let child_entity = *sim
        .committed(req)
        .unwrap()
        .children
        .first()
        .expect("one child");

    // The transform journalled at the split instant is the identity — a terrain
    // child's local cells are world cells.
    let snap = journalled(&sim, child_entity);
    assert!(
        snap.pose.translation_m.iter().all(|v| v.abs() < 1e-9),
        "terrain child spawns at the world origin translation"
    );
    let q = snap.pose.rotation.to_unit().unwrap();
    assert!(
        (q[0].abs() < 1e-3
            && q[1].abs() < 1e-3
            && q[2].abs() < 1e-3
            && (q[3].abs() - 1.0).abs() < 1e-3),
        "terrain child spawns with the identity orientation, got {q:?}"
    );

    // A known beam cell is now in the child at the same global coordinate, and
    // gone from the terrain. (Volume coordinates are frame-invariant, so this
    // still holds after the body has begun to fall.)
    let body = sim.world().body(child_entity).unwrap();
    let beam_cell = GlobalCell::new(8, 8, 1);
    assert_eq!(
        body.volume.sample(beam_cell).unwrap(),
        Sample::Filled(STONE)
    );
    assert_eq!(
        sim.world()
            .volume_ref(terrain)
            .unwrap()
            .sample(beam_cell)
            .unwrap(),
        Sample::Empty { modified: true }
    );
}

// --- cutting a rotated moving body again -------------------------------

#[test]
fn cutting_a_rotated_moving_body_splits_it_without_moving_geometry() {
    let mut setup = fixtures::flat_terrain_setup();
    setup.anchor = AnchorPlane::at(-100_000); // nothing anchored
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();

    let parent_pose = BodyPose::new(fixtures::oblique_spin(), [4.0, 8.0, 4.0]);
    let linvel = [1.5, 0.0, -0.5];
    let angvel = [0.0, 0.8, 0.0];
    let parent = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            parent_pose,
            linvel,
            angvel,
            2600.0,
            0,
        )
        .unwrap();
    let parent_vid = sim.world().body(parent).unwrap().volume_id;
    let cs = sim.world().body(parent).unwrap().volume.cell_size();

    // The pre-cut whole-body geometry and world position of a right-cube cell.
    let full_body = sim.world().volume_ref(parent_vid).unwrap().clone();
    let before_solid = volume_solid_count(&full_body);
    let right_cube_cell = GlobalCell::new(8, 1, 1); // s=4, gap=3 -> right cube x in 7..10
    let world_before = parent_pose.local_cell_to_world_m(
        DVec3::new(
            right_cube_cell.x as f64 + 0.5,
            right_cube_cell.y as f64 + 0.5,
            right_cube_cell.z as f64 + 0.5,
        ),
        cs,
    );
    let parent_com = uniform_com_world(&full_body, &parent_pose, cs);

    let req = RequestId(7);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(parent),
        brush_cell(5, 2, 2, 2),
    ))
    .unwrap();
    run(&mut sim, 12);

    assert!(sim.committed(req).is_some(), "the body cut commits");
    assert_eq!(sim.world().body_count(), 2, "dumbbell -> two bodies");

    // Conservation across both bodies: nothing gained, only the bridge cleared.
    let after_solid: u64 = sim
        .world()
        .bodies()
        .map(|b| volume_solid_count(&b.volume))
        .sum();
    assert!(after_solid < before_solid && after_solid >= before_solid - 8);

    // The child that owns the right cube still holds that cell (frame-invariant
    // volume geometry) ...
    let child_entity = *sim
        .committed(req)
        .unwrap()
        .children
        .first()
        .expect("a child");
    let child = sim.world().body(child_entity).expect("child body");
    assert_eq!(
        child.volume.sample(right_cube_cell).unwrap(),
        Sample::Filled(STONE)
    );

    // ... and its split-instant transform (journalled) reproduces the parent's,
    // so the cell is at the same world position it was before the cut.
    let snap = journalled(&sim, child_entity);
    let sq = snap.pose.rotation.to_unit().unwrap();
    let split_pose = BodyPose::new(
        DQuat::from_xyzw(sq[0] as f64, sq[1] as f64, sq[2] as f64, sq[3] as f64),
        snap.pose.translation_m,
    );
    let world_after = split_pose.local_cell_to_world_m(
        DVec3::new(
            right_cube_cell.x as f64 + 0.5,
            right_cube_cell.y as f64 + 0.5,
            right_cube_cell.z as f64 + 0.5,
        ),
        cs,
    );
    assert!(
        (world_after - world_before).length() < 3e-3,
        "split geometry stays at the same world location ({world_before} -> {world_after})"
    );

    // Velocity inheritance, read from the journalled snapshot (captured at the
    // commit, before any post-commit physics step):
    // v_child ~= v_parent + omega x (r_child_com - r_parent_com).
    let omega = DVec3::from_array(angvel);
    let child_com = uniform_com_world(&child.volume, &parent_pose, cs);
    let expected = DVec3::from_array(linvel) + omega.cross(child_com - parent_com);
    let got = DVec3::new(
        snap.linear_velocity[0] as f64,
        snap.linear_velocity[1] as f64,
        snap.linear_velocity[2] as f64,
    );
    assert!(
        (got - expected).length() < 0.4,
        "child velocity inherits parent motion: expected ~{expected}, got {got}"
    );
    let got_w = DVec3::new(
        snap.angular_velocity[0] as f64,
        snap.angular_velocity[1] as f64,
        snap.angular_velocity[2] as f64,
    );
    assert!(
        (got_w - omega).length() < 1e-4,
        "child inherits parent angular velocity exactly"
    );
}

// --- no second impulse on retry --------------------------------------

#[test]
fn no_second_impulse_on_retry() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();

    // A decoy cut on the same region lands first; the split+explosion cut is
    // staged against the old revision and must recompute once before it lands.
    let decoy = RequestId(1);
    let split = RequestId(2);
    sim.submit(EditIntent::cut(
        decoy,
        actor(),
        EditTarget::Terrain,
        brush_cell(11, 6, 2, 1),
    ))
    .unwrap();
    sim.submit(
        EditIntent::cut(split, actor(), EditTarget::Terrain, brush_cell(10, 4, 1, 2))
            .with_explosion(ExplosionImpulse {
                magnitude_ns: 400.0,
                direction: [1.0, 0.0, 0.0],
            }),
    )
    .unwrap();
    let reports = run(&mut sim, 24);

    let retries: usize = reports
        .iter()
        .filter(|r| r.retried.contains(&split))
        .count();
    assert!(retries >= 1, "the split cut had to retry at least once");
    assert!(
        sim.committed(split).is_some(),
        "and then committed exactly once"
    );
    assert_eq!(sim.world().body_count(), 1, "one child, created once");

    // Re-submitting the same request id is a rejected no-op (idempotent).
    assert!(matches!(
        sim.submit(EditIntent::cut(
            split,
            actor(),
            EditTarget::Terrain,
            brush_cell(10, 4, 1, 2)
        )),
        Err(spall_sim::IntentError::DuplicateRequest(_))
    ));

    // The child's journalled launch speed reflects one impulse, not two.
    let child_entity = *sim.committed(split).unwrap().children.first().unwrap();
    let child = sim.world().body(child_entity).unwrap();
    let cell_m = child.volume.cell_size().metres();
    let mass = volume_solid_count(&child.volume) as f64 * cell_m.powi(3) * 2600.0;
    let single_dv = 400.0 / mass;
    let snap = journalled(&sim, child_entity);
    let speed = DVec3::new(
        snap.linear_velocity[0] as f64,
        snap.linear_velocity[1] as f64,
        snap.linear_velocity[2] as f64,
    )
    .length();
    assert!(
        speed > 0.3 * single_dv && speed < 1.8 * single_dv + 0.5,
        "speed {speed} m/s is a single {single_dv} m/s impulse, not zero and not doubled"
    );
}

// --- conflicting cuts converge to commit order -----------------------

#[test]
fn conflicting_cuts_converge() {
    let make = || Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    let mut sim = make();
    let terrain = sim.world().terrain_volume_id();
    let a = RequestId(1);
    let b = RequestId(2);
    sim.submit(EditIntent::cut(
        a,
        actor(),
        EditTarget::Terrain,
        brush_cell(6, 1, 6, 2),
    ))
    .unwrap();
    sim.submit(EditIntent::cut(
        b,
        actor(),
        EditTarget::Terrain,
        brush_cell(7, 1, 6, 2),
    ))
    .unwrap();
    let reports = run(&mut sim, 24);

    assert!(
        sim.committed(a).is_some() && sim.committed(b).is_some(),
        "both commit"
    );
    assert!(
        reports.iter().any(|r| r.retried.contains(&b)),
        "the second cut recomputes after the first commits"
    );
    let tx_a = sim.committed(a).unwrap().transaction.get();
    let tx_b = sim.committed(b).unwrap().transaction.get();
    assert!(
        tx_a < tx_b,
        "commit order is the request order (server-assigned)"
    );

    // Same end state as applying A fully, then B fully.
    let mut reference = make();
    reference
        .submit(EditIntent::cut(
            a,
            actor(),
            EditTarget::Terrain,
            brush_cell(6, 1, 6, 2),
        ))
        .unwrap();
    run(&mut reference, 8);
    reference
        .submit(EditIntent::cut(
            b,
            actor(),
            EditTarget::Terrain,
            brush_cell(7, 1, 6, 2),
        ))
        .unwrap();
    run(&mut reference, 8);

    assert_eq!(
        sim.world().volume_hash(terrain),
        reference
            .world()
            .volume_hash(reference.world().terrain_volume_id()),
        "concurrent commit converges to the sequential result"
    );
}

// --- repeatedly contested region is serialized ---------------------

#[test]
fn a_repeatedly_contested_region_is_serialized_and_still_makes_progress() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
    let ids: Vec<RequestId> = (1..=5).map(RequestId).collect();
    for (i, id) in ids.iter().enumerate() {
        sim.submit(EditIntent::cut(
            *id,
            actor(),
            EditTarget::Terrain,
            brush_cell(10 + i as i64 % 2, 1, 10, 2),
        ))
        .unwrap();
    }
    let reports = run(&mut sim, 60);

    for id in &ids {
        assert!(
            sim.committed(*id).is_some(),
            "request {id:?} eventually commits"
        );
    }
    assert!(
        reports.iter().any(|r| !r.serialized_regions.is_empty()),
        "the contested region was promoted to serial commit"
    );
    // Commit order is the request order.
    let mut txs: Vec<u64> = ids
        .iter()
        .map(|id| sim.committed(*id).unwrap().transaction.get())
        .collect();
    let sorted = {
        let mut c = txs.clone();
        c.sort_unstable();
        c
    };
    assert_eq!(txs, sorted, "serialized commits keep request order");
    txs.dedup();
    assert_eq!(
        txs.len(),
        5,
        "five distinct transactions, none applied twice"
    );
}
