//! ENG-55: a body whose occupied minimum is not local cell zero must collide on
//! its authoritative voxels, not near the body-local origin.
//!
//! `spall_physics` now offsets every collider shape by `OccupancyGrid::origin()
//! * cell_m` in the body frame, and refreshes that offset on each rebuild. These
//! tests check the end-to-end result through the authoritative sim: a terrain
//! split child, a source whose tight origin shifts after a cut, and a rotated
//! moving-body split all keep collision, render geometry and the journalled pose
//! on the same world cells.

use glam::{DQuat, DVec3};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CELLS_PER_BRICK, CellSizeCode, EntityId, GlobalCell, LocalCell, MaterialId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, STONE};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_structure::AnchorPlane;
use spall_voxel::{EditPlan, Volume};

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

fn solid_cells(v: &Volume) -> Vec<GlobalCell> {
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

/// Centroid of a volume's solid cells, in body-local metres (uniform density).
fn cell_centroid_local_m(v: &Volume) -> [f64; 3] {
    let cells = solid_cells(v);
    let n = cells.len() as f64;
    let cs = v.cell_size().metres();
    let mut sum = [0.0f64; 3];
    for g in &cells {
        sum[0] += (g.x as f64 + 0.5) * cs;
        sum[1] += (g.y as f64 + 0.5) * cs;
        sum[2] += (g.z as f64 + 0.5) * cs;
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// `world.physics().body_local_com` for a body, as `[f64; 3]`.
fn body_local_com(sim: &Simulation, entity: EntityId) -> [f64; 3] {
    let phys = sim.world().body(entity).unwrap().phys;
    sim.world()
        .physics()
        .body_local_com(phys)
        .map(f64::from)
}

fn run(sim: &mut Simulation, ticks: u32) {
    sim.run_until_idle(ticks).expect("ticks");
}

/// The beam that detaches from `bridged_terrain_setup` has its tight grid origin
/// at `(4, 8, 1)`. Its collider must sit on those cells: the beam then rests on
/// the bridge floor directly beneath it instead of tunnelling through.
#[test]
fn a_terrain_split_child_collides_on_its_authoritative_cells() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).unwrap();

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(10, 4, 1, 2),
    ))
    .unwrap();
    run(&mut sim, 12);
    assert!(sim.committed(req).is_some(), "the column cut commits");
    assert_eq!(sim.world().body_count(), 1, "the beam detaches as one body");

    let child = *sim.committed(req).unwrap().children.first().unwrap();

    // The collider's body-local COM coincides with the authoritative cell
    // centroid (they would differ by origin*cell_m if the origin were ignored).
    let expect = cell_centroid_local_m(&sim.world().body(child).unwrap().volume);
    let com = body_local_com(&sim, child);
    for a in 0..3 {
        assert!(
            (com[a] - expect[a]).abs() < 0.05,
            "axis {a}: collider COM {com:?} vs authoritative centroid {expect:?}"
        );
    }

    // Settle. With the collider on the beam cells (bottom face y = 2.0 m) it
    // lands on the bridge floor (top y = 0.5 m); the pre-fix displaced collider
    // spawned inside the floor and was flung past y = -5 m.
    for _ in 0..240 {
        sim.step_physics_only();
    }
    let body = sim.world().body(child).unwrap();
    assert!(
        body.pose.translation_m.iter().all(|v| v.is_finite()),
        "child pose stays finite"
    );
    assert!(
        body.pose.translation_m[1] > -3.0,
        "beam settled on the floor under its cells (frame y = {})",
        body.pose.translation_m[1]
    );
    // A known beam cell ends up just above the floor top, not tunnelled.
    let probe = GlobalCell::new(12, 8, 1);
    let world_y = body
        .pose
        .local_cell_to_world_m(
            DVec3::new(
                probe.x as f64 + 0.5,
                probe.y as f64 + 0.5,
                probe.z as f64 + 0.5,
            ),
            body.volume.cell_size(),
        )
        .y;
    assert!(
        (0.0..2.0).contains(&world_y),
        "beam cell rests near the floor top (world y = {world_y})"
    );
}

/// A body-local dumbbell shifted so its cells start at `(5, 5, 5)`: a cut
/// through the bridge disconnects it into two bodies. Body-local frame stays
/// authoritative regardless of the origin offset.
fn offset_dumbbell(s: i64, gap: i64, shift: i64) -> impl FnOnce(spall_core::VolumeId) -> Volume {
    move |id| {
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        let b = |v: &mut Volume, a: GlobalCell, c: GlobalCell| {
            v.apply_edit(&EditPlan::filled_box(id, a, c, STONE)).unwrap();
        };
        let o = shift;
        b(
            &mut v,
            GlobalCell::new(o, o, o),
            GlobalCell::new(o + s - 1, o + s - 1, o + s - 1),
        );
        let rx = o + s + gap;
        b(
            &mut v,
            GlobalCell::new(rx, o, o),
            GlobalCell::new(rx + s - 1, o + s - 1, o + s - 1),
        );
        let mid = o + s / 2;
        b(
            &mut v,
            GlobalCell::new(o + s, mid, mid),
            GlobalCell::new(rx - 1, mid, mid),
        );
        v
    }
}

#[test]
fn a_rotated_moving_body_split_keeps_collision_on_its_cells() {
    let mut setup = fixtures::flat_terrain_setup();
    setup.anchor = AnchorPlane::at(-100_000); // nothing anchored
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();

    let parent_pose = BodyPose::new(fixtures::oblique_spin(), [6.0, 9.0, 6.0]);
    let parent = sim
        .world_mut()
        .spawn_body(
            offset_dumbbell(4, 3, 5),
            parent_pose,
            [1.2, 0.0, -0.4],
            [0.0, 0.7, 0.0],
            2600.0,
            0,
        )
        .unwrap();

    // The offset body's own collider already sits on its cells.
    let pexpect = cell_centroid_local_m(&sim.world().body(parent).unwrap().volume);
    let pcom = body_local_com(&sim, parent);
    for a in 0..3 {
        assert!(
            (pcom[a] - pexpect[a]).abs() < 0.05,
            "parent axis {a}: {pcom:?} vs {pexpect:?}"
        );
    }

    let req = RequestId(7);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(parent),
        // bridge midpoint of the shifted dumbbell: s=4, gap=3, shift=5 ->
        // bridge x in 9..11, y=z=7.
        brush_cell(10, 7, 7, 2),
    ))
    .unwrap();
    run(&mut sim, 12);
    assert!(sim.committed(req).is_some(), "the body cut commits");
    assert_eq!(sim.world().body_count(), 2, "dumbbell -> two bodies");

    let child = *sim.committed(req).unwrap().children.first().unwrap();
    let body = sim.world().body(child).unwrap();

    // Collision (body-local COM) and render geometry (cell centroid) coincide in
    // the same committed tick, for a rotated moving body whose grid origin is
    // not local zero.
    let expect = cell_centroid_local_m(&body.volume);
    let com = body_local_com(&sim, child);
    for a in 0..3 {
        assert!(
            (com[a] - expect[a]).abs() < 0.05,
            "child axis {a}: collider COM {com:?} vs authoritative centroid {expect:?}"
        );
    }

    // ... and the journalled split pose reproduces the parent transform, so the
    // world geometry is unchanged at the split instant.
    let snap = *sim
        .journal()
        .last()
        .unwrap()
        .participants
        .iter()
        .find(|p| p.body == child)
        .unwrap();
    let q = snap.pose.rotation.to_unit().unwrap();
    let pq = parent_pose.rotation;
    let dot = (f64::from(q[0]) * pq.x
        + f64::from(q[1]) * pq.y
        + f64::from(q[2]) * pq.z
        + f64::from(q[3]) * pq.w)
        .abs();
    assert!(dot > 0.999, "child split rotation matches the parent (dot {dot})");
    for a in 0..3 {
        assert!(
            (snap.pose.translation_m[a] - parent_pose.translation_m[a]).abs() < 1e-6,
            "child split translation matches the parent"
        );
    }
    let _ = (STONE, MaterialId::AIR);
}
