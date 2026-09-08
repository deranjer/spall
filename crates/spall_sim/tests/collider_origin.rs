//! ENG-55: a voxel collider must be installed (and rebuilt) at its occupancy
//! grid's origin, not near body-local zero.
//!
//! `spall_physics` builds a collider shape with the tight `OccupancyGrid`'s cell
//! `(0, 0, 0)` at the shape origin; the authoritative sim only ever handed Rapier
//! the volume/body pose and dropped `OccupancyGrid::origin`. Any volume whose
//! occupied minimum is not local cell zero therefore had its collision displaced
//! from its authoritative voxels, and a rebuild after cutting the minimum cells
//! shifted the tight origin again.
//!
//! These probes drive dynamic bodies past world positions that are only solid /
//! only air once the grid origin is honoured, so a displaced collider is caught:
//!
//! - `a_detached_terrain_child_*` — a terrain split whose child's floor sits only
//!   under the child's own x-range: a collider parked near local zero misses it
//!   and the body free-falls.
//! - `a_source_rebuild_*` — a non-splitting terrain cut that erases the tight
//!   origin's cells: the rebuilt collider must track the shifted origin, so the
//!   cleared span stops colliding and the far span keeps colliding.
//! - `a_rotated_moving_body_split_*` — a spinning body with a large grid origin:
//!   the child's world geometry and inherited velocity must still match a
//!   from-cells reference.

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CELLS_PER_BRICK, EntityId, GlobalCell, LocalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, STONE};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_structure::AnchorPlane;
use spall_voxel::{Sample, Volume};

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

fn run(sim: &mut Simulation, ticks: u32) {
    sim.run_until_idle(ticks).expect("ticks");
}

fn solid_cells_vec(v: &Volume) -> Vec<GlobalCell> {
    let mut out = Vec::new();
    for c in v.resident_brick_coords() {
        let s = v.snapshot_brick(c).unwrap().unwrap();
        for i in 0..CELLS_PER_BRICK as u16 {
            let lc = LocalCell::from_linear_index(i).unwrap();
            if !s.get(lc).is_air() {
                out.push(GlobalCell::from_parts(c, lc).unwrap());
            }
        }
    }
    out
}

/// Uniform-density centre of mass, world metres, for `v` at `pose`.
fn uniform_com_world(v: &Volume, pose: &BodyPose, cs: spall_core::CellSizeCode) -> DVec3 {
    let cells = solid_cells_vec(v);
    let n = cells.len() as f64;
    let mut sum = DVec3::ZERO;
    for g in &cells {
        sum += DVec3::new(g.x as f64 + 0.5, g.y as f64 + 0.5, g.z as f64 + 0.5);
    }
    pose.local_cell_to_world_m(sum / n, cs)
}

fn journalled(sim: &Simulation, entity: EntityId) -> spall_protocol::MotionSnapshot {
    *sim.journal()
        .last()
        .expect("a journal entry")
        .participants
        .iter()
        .find(|p| p.body == entity)
        .expect("participant snapshot for the body")
}

/// Drops a 1-cell probe at `world_m`, steps physics, and returns its resting Y.
fn drop_probe(sim: &mut Simulation, world_m: [f64; 3], steps: usize) -> f64 {
    let probe = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, world_m),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..steps {
        sim.step_physics_only();
    }
    sim.world().body(probe).unwrap().pose.translation_m[1]
}

// --- terrain split: the child lands on the floor beneath its real cells -----

#[test]
fn a_detached_terrain_child_lands_on_the_floor_under_its_authoritative_cells() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::far_bridged_terrain_setup())).unwrap();
    let terrain = sim.world().terrain_volume_id();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(31, 4, 1, 2),
    ))
    .unwrap();
    run(&mut sim, 8);

    let committed = sim.committed(req).expect("the column cut commits").clone();
    assert_eq!(sim.world().body_count(), 1, "the beam detaches as one body");
    let child = *committed.children.first().expect("one child");

    // Commit-tick coincidence: the journalled transform is the identity (terrain
    // child), and the beam's cells are exactly the world cells they were.
    let snap = journalled(&sim, child);
    assert!(
        snap.pose.translation_m.iter().all(|v| v.abs() < 1e-9),
        "a terrain child journals the identity translation, got {:?}",
        snap.pose.translation_m
    );
    let beam_cell = GlobalCell::new(36, 7, 1);
    assert_eq!(
        sim.world()
            .body(child)
            .unwrap()
            .volume
            .sample(beam_cell)
            .unwrap(),
        Sample::Filled(STONE),
        "the child owns the beam cell"
    );
    assert_eq!(
        sim.world()
            .volume_ref(terrain)
            .unwrap()
            .sample(beam_cell)
            .unwrap(),
        Sample::Empty { modified: true },
        "and the terrain no longer holds it"
    );

    // Physics: the beam's collider is under its real cells (world x in 6..10 m),
    // where the floor is, so it drops a short distance and comes to rest. A
    // collider parked near body-local zero (x in 0..4 m) has no floor beneath it
    // and the beam free-falls past y = -5.
    let start_y = sim.world().body(child).unwrap().pose.translation_m[1];
    for _ in 0..240 {
        sim.step_physics_only();
    }
    let end_y = sim.world().body(child).unwrap().pose.translation_m[1];
    assert!(
        end_y.is_finite() && end_y > -3.0,
        "the beam landed on the floor beneath it (y {start_y} -> {end_y})"
    );
    assert!(
        end_y < start_y + 0.5,
        "the beam settled downward, it did not launch (y {start_y} -> {end_y})"
    );
}

// --- source rebuild: the collider tracks the shifted tight origin ----------

#[test]
fn a_source_rebuild_after_cutting_the_minimum_cells_moves_the_collider_with_the_origin() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::far_raised_block_setup())).unwrap();

    // Baseline: the slab's top face (y = 10 cells = 2.5 m) collides at its far
    // x end (world x = 10.5 m; cells x 24..43 -> world 6..11 m).
    let far_top = [10.5, 3.2, 0.375];
    let rest0 = drop_probe(&mut sim, far_top, 120);
    assert!(
        rest0 > 2.0,
        "probe rests on the intact slab top (y = {rest0})"
    );

    // Erase the slab's low-x cells, which currently define its tight occupancy
    // origin. This is a non-splitting terrain edit: the collider is rebuilt from
    // the shrunk grid, whose origin has moved +~4 cells on X.
    let bodies_before = sim.world().body_count();
    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(25, 8, 2, 3),
    ))
    .unwrap();
    run(&mut sim, 8);
    assert!(sim.committed(req).is_some(), "the low-x cut commits");
    assert_eq!(
        sim.world().body_count(),
        bodies_before,
        "the terrain cut detached nothing"
    );

    // The cleared low-x span (world x ~= 6.35 m) must no longer collide: a probe
    // there falls straight through. A collider still parked at the old origin
    // keeps solid geometry under this column.
    let cleared = drop_probe(&mut sim, [6.35, 3.2, 0.375], 90);
    assert!(
        cleared.is_finite() && cleared < 1.5,
        "probe fell through the cleared low-x span (y = {cleared})"
    );

    // The untouched far x end must still collide at the same height: a collider
    // that shifted left with the shrunk grid would leave world x = 10.5 m in the
    // air.
    let rest1 = drop_probe(&mut sim, far_top, 120);
    assert!(
        rest1 > 2.0,
        "probe still rests on the far end of the rebuilt collider (y = {rest1})"
    );
}

// --- rotated moving-body split: geometry and velocity stay put ------------

#[test]
fn a_rotated_moving_body_split_keeps_child_geometry_and_inherited_velocity() {
    let mut setup = fixtures::flat_terrain_setup();
    setup.anchor = AnchorPlane::at(-100_000); // nothing is anchored
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();

    // A dumbbell whose occupancy minimum is 40 cells (10 m) off the volume
    // origin, spun about an oblique axis and moving.
    let off = GlobalCell::new(40, 40, 40);
    let parent_pose = BodyPose::new(fixtures::oblique_spin(), [3.0, 30.0, 3.0]);
    let linvel = [1.5, 0.0, -0.5];
    let angvel = [0.0, 0.8, 0.0];
    let parent = sim
        .world_mut()
        .spawn_body(
            fixtures::offset_dumbbell(4, 3, off),
            parent_pose,
            linvel,
            angvel,
            2600.0,
            0,
        )
        .unwrap();
    let parent_vid = sim.world().body(parent).unwrap().volume_id;
    let cs = sim.world().body(parent).unwrap().volume.cell_size();

    let full_body = sim.world().volume_ref(parent_vid).unwrap().clone();
    // Right cube: local x 7..10 -> shifted by `off`.
    let right_cube_cell = GlobalCell::new(off.x + 8, off.y + 1, off.z + 1);
    let cell_centre = DVec3::new(
        right_cube_cell.x as f64 + 0.5,
        right_cube_cell.y as f64 + 0.5,
        right_cube_cell.z as f64 + 0.5,
    );
    let world_before = parent_pose.local_cell_to_world_m(cell_centre, cs);
    let parent_com = uniform_com_world(&full_body, &parent_pose, cs);

    let req = RequestId(7);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(parent),
        brush_cell(off.x + 5, off.y + 2, off.z + 2, 2),
    ))
    .unwrap();
    run(&mut sim, 12);

    let committed = sim.committed(req).expect("the body cut commits").clone();
    assert_eq!(sim.world().body_count(), 2, "dumbbell -> two bodies");
    let child = *committed.children.first().expect("a child");
    let child_body = sim.world().body(child).expect("child body");
    assert_eq!(
        child_body.volume.sample(right_cube_cell).unwrap(),
        Sample::Filled(STONE),
        "the child holds the right cube"
    );

    // The split-instant transform (journalled) reproduces the parent's, so the
    // cell is at the same world position it was before the cut.
    let snap = journalled(&sim, child);
    let sq = snap.pose.rotation.to_unit().unwrap();
    let split_pose = BodyPose::new(
        DQuat::from_xyzw(sq[0] as f64, sq[1] as f64, sq[2] as f64, sq[3] as f64),
        snap.pose.translation_m,
    );
    let world_after = split_pose.local_cell_to_world_m(cell_centre, cs);
    assert!(
        (world_after - world_before).length() < 3e-3,
        "split geometry stays at the same world location ({world_before} -> {world_after})"
    );

    // Inherited velocity: v_child ~= v_parent + omega x (r_child_com - r_parent_com),
    // with the parent COM taken in the same origin-aware world frame. A parent
    // COM computed without the grid-origin offset is wrong by ~10 m, which
    // omega = 0.8 rad/s turns into several m/s of error here.
    let omega = DVec3::from_array(angvel);
    let child_com = uniform_com_world(&child_body.volume, &parent_pose, cs);
    let expected = DVec3::from_array(linvel) + omega.cross(child_com - parent_com);
    let got = DVec3::new(
        snap.linear_velocity[0] as f64,
        snap.linear_velocity[1] as f64,
        snap.linear_velocity[2] as f64,
    );
    assert!(
        (got - expected).length() < 0.5,
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
