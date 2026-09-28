//! ENG-56: when an authoritative volume becomes empty its physics collision
//! must be retired atomically with the committing edit.
//!
//! Before the fix, `commit` skipped collider replacement when
//! `OccupancyGrid::from_volume` returned `None` and `PhysicsWorld` had no
//! removal path, so a fully-erased dynamic body kept resting on — and blocking —
//! its obsolete solid shape after the transaction committed.

use glam::DQuat;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CellSizeCode, EntityId, GlobalCell, MaterialId, SphereBrush, VolumeId};
use spall_physics::PhysicsConfig;
use spall_protocol::RequestId;
use spall_sim::fixtures::{self};
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig, WorldSetup};
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

fn settle(sim: &mut Simulation, steps: usize) {
    for _ in 0..steps {
        sim.step_physics_only();
    }
}

fn body_y(sim: &Simulation, e: EntityId) -> f64 {
    sim.world().body(e).unwrap().pose.translation_m[1]
}

/// A one-cell dynamic body rests on the floor with a probe body stacked on top
/// of it. Cutting the lower body's only cell must, on the committing tick,
/// retire it and its collider so the probe drops through the cleared cell onto
/// the floor — and never rises again.
#[test]
fn a_body_emptied_by_a_cut_is_retired_and_stops_colliding_on_the_commit_tick() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    // Floor STONE occupies world y in [0.0, 0.5] (cells y 0..1 at 0.25 m).
    let lower = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [3.0, 0.5, 3.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let probe = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [3.0, 1.05, 3.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();

    settle(&mut sim, 90);
    assert_eq!(sim.world().body_count(), 2, "both one-cell bodies are live");
    let lower_rest = body_y(&sim, lower);
    let probe_rest = body_y(&sim, probe);
    assert!(
        (0.35..0.65).contains(&lower_rest),
        "the lower body settled on the floor (y {lower_rest})"
    );
    assert!(
        probe_rest > lower_rest + 0.15,
        "the probe settled on top of the lower body (probe {probe_rest}, lower {lower_rest})"
    );

    // Cut the lower body's only cell (body-local coordinates).
    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(lower),
        brush_cell(0, 0, 0, 1),
    ))
    .unwrap();

    // Drive ticks and catch the state on the tick the cut commits.
    let mut probe_y_on_commit_tick = None;
    for _ in 0..20 {
        sim.tick().unwrap();
        if sim.committed(req).is_some() {
            probe_y_on_commit_tick = Some(body_y(&sim, probe));
            break;
        }
    }
    assert!(sim.committed(req).is_some(), "the emptying cut commits");

    // --- no targetable / live empty body ---------------------------------
    assert_eq!(
        sim.world().body_count(),
        1,
        "the emptied body is retired, only the probe is left"
    );
    assert!(
        sim.world().body(lower).is_none(),
        "the emptied entity no longer resolves to a body"
    );
    assert!(
        sim.world().bodies().all(|b| b.entity != Some(lower)),
        "the emptied body is not enumerable"
    );

    // --- the probe falls through the cleared region, same committed tick -
    let y_on_commit = probe_y_on_commit_tick.expect("probe y captured on the commit tick");
    assert!(
        y_on_commit < probe_rest - 1e-4,
        "the probe is already falling on the tick the cut committed ({probe_rest} -> {y_on_commit})"
    );

    // --- no late-motion resurrection ------------------------------------
    let mut highest_after = y_on_commit;
    for _ in 0..300 {
        sim.step_physics_only();
        let y = body_y(&sim, probe);
        assert!(y.is_finite(), "probe stays finite");
        highest_after = highest_after.max(y);
    }
    assert!(
        highest_after <= probe_rest + 1e-3,
        "the probe never rises back toward the retired collider ({highest_after} vs {probe_rest})"
    );

    let probe_final = body_y(&sim, probe);
    assert!(
        (probe_final - lower_rest).abs() < 0.1,
        "the probe came to rest on the floor where the retired cell used to hold it up \
         (probe {probe_final}, floor rest {lower_rest})"
    );
}

/// A single-body world (no stacked probe): the one-cell body's own resting spot
/// is cleared, and a fresh probe dropped straight through that column reaches
/// the floor rather than stopping on the obsolete collider.
#[test]
fn a_probe_dropped_through_a_retired_cell_reaches_the_floor() {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();

    let body = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [5.0, 0.5, 5.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 60);
    let floor_rest = body_y(&sim, body);

    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(body),
        brush_cell(0, 0, 0, 1),
    ))
    .unwrap();
    sim.run_until_idle(10).unwrap();
    assert!(sim.committed(req).is_some());
    assert_eq!(sim.world().body_count(), 0, "the world holds no live body");

    let probe = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [5.0, 3.5, 5.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 400);
    let probe_final = body_y(&sim, probe);
    assert!(
        (probe_final - floor_rest).abs() < 0.1,
        "probe fell through the cleared cell to the floor (probe {probe_final}, floor {floor_rest})"
    );
}

/// A tiny terrain slab, wrapped in a resident air envelope, whose every solid
/// cell one cut erases. Terrain keeps its (now empty) record but loses its
/// physical collider, so a probe dropped onto it free-falls instead of resting.
#[test]
fn empty_terrain_ownership_loses_its_collider() {
    let tid = VolumeId::new(1).unwrap();
    let mut terrain = Volume::new(tid, CellSizeCode::Quarter);
    terrain
        .apply_edit(&EditPlan::filled_box(
            tid,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(4, 9, 4),
            MaterialId::AIR,
        ))
        .unwrap();
    terrain
        .apply_edit(&EditPlan::filled_box(
            tid,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(1, 1, 1),
            fixtures::STONE,
        ))
        .unwrap();
    let setup = WorldSetup {
        terrain,
        terrain_collider_region: (GlobalCell::new(0, 0, 0), GlobalCell::new(4, 9, 4)),
        materials: fixtures::stone_manifest(),
        anchor: AnchorPlane::at(0),
        physics: PhysicsConfig::default(),
    };
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
    let terrain_vid = sim.world().terrain_volume_id();

    // Erase every terrain cell in one cut.
    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush_cell(0, 0, 0, 4),
    ))
    .unwrap();
    sim.run_until_idle(10).unwrap();
    assert!(
        sim.committed(req).is_some(),
        "the terrain-clearing cut commits"
    );

    // Terrain keeps its record, now holding no solid cell.
    let terrain_vol = sim
        .world()
        .volume_ref(terrain_vid)
        .expect("terrain still exists");
    assert_eq!(
        spall_sim::solid_cells(terrain_vol),
        0,
        "the terrain volume is empty"
    );

    // A probe dropped where the slab was free-falls — nothing to land on.
    let probe = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(1),
            BodyPose::new(DQuat::IDENTITY, [0.25, 3.0, 0.25]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    settle(&mut sim, 240);
    let probe_y = body_y(&sim, probe);
    assert!(
        probe_y < -2.0,
        "the probe free-falls past the retired terrain collider (y {probe_y})"
    );
}
