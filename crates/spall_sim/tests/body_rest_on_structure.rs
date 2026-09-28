//! ENG-61: a cut that detaches a voxel body from terrain must leave that body
//! able to **come to rest on the remaining structure** — not free-fall forever,
//! and not clip down through the floor — even while the terrain collider is
//! rebuilt on every subsequent committed cut.
//!
//! This is the CPU-side proof for the networked `body-rest-on-structure` gate
//! fixture. It drives one authoritative [`Simulation`] through the same cut
//! script (column cut detaches the cross-brick beam; both clients excavate the
//! *outer* ends of the floor while leaving the span under the beam intact), then
//! steps physics until the beam settles and asserts it rests on the floor top.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::Sample;

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

#[test]
fn detached_cross_brick_beam_comes_to_rest_on_the_remaining_floor() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::cross_brick_bridged_setup())).unwrap();

    // The `body-rest-on-structure.json` cut script: (at_tick, cell, radius).
    // Cut 0/1 sever the seam column; the rest excavate the floor ends *outside*
    // the beam's x-span so the beam keeps a floor to land on. Every commit
    // rebuilds the terrain collider.
    let script: [(u64, [i64; 3], i64); 6] = [
        (4, [31, 4, 1], 3),
        (8, [32, 4, 2], 3),
        (16, [21, 1, 1], 1),
        (24, [43, 1, 1], 1),
        (32, [21, 1, 2], 1),
        (40, [43, 1, 2], 1),
    ];

    let mut req = 1u64;
    let mut next = 0usize;
    let mut peak_drop = 0.0_f64;
    let mut settled_at: Option<u64> = None;

    for tick in 1..=400u64 {
        if next < script.len() && script[next].0 == tick {
            let (_, cell, radius) = script[next];
            let _ = sim.submit(EditIntent::cut(
                RequestId(req),
                actor(),
                EditTarget::Terrain,
                brush_cell(cell[0], cell[1], cell[2], radius),
            ));
            req += 1;
            next += 1;
        }
        sim.tick().unwrap();

        if let Some(b) = sim.world().bodies().next() {
            let v = b.linvel_m_s;
            let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            peak_drop = peak_drop.max(-b.pose.translation_m[1]);
            if settled_at.is_none() && b.sleeping && speed < 0.02 && tick > 20 {
                settled_at = Some(tick);
            }
        }
    }

    assert_eq!(
        sim.world().body_count(),
        1,
        "the column cut detaches exactly the beam; the outer-end floor cuts spawn nothing"
    );
    let beam = sim.world().bodies().next().unwrap();
    let end_y = beam.pose.translation_m[1];
    let v = beam.linvel_m_s;
    let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();

    assert!(
        settled_at.is_some(),
        "the beam never reached a sustained rest (end speed {speed} m/s, y {end_y})"
    );
    assert!(beam.sleeping, "a settled beam is asleep");
    assert!(
        speed < 0.02,
        "settled beam is stationary (speed {speed} m/s)"
    );
    assert!(end_y.is_finite(), "beam position stayed finite");

    // Beam bottom starts at y = 7 cells = 1.75 m; the floor top is y = 2 cells =
    // 0.5 m. Coming to rest on the floor is a ~1.25 m drop of the body origin; a
    // free-fall over these 400 ticks would be tens of metres.
    assert!(
        (peak_drop - 1.25).abs() < 0.35 && (-end_y - 1.25).abs() < 0.35,
        "beam rests on the floor top, not through it and not in free-fall \
         (peak drop {peak_drop:.3} m, resting origin y {end_y:.3})"
    );

    // It settled on the *remaining* structure: the terrain still holds floor
    // cells directly under the beam's span.
    let terrain = sim.world().terrain_volume_id();
    let under_beam = sim
        .world()
        .volume_ref(terrain)
        .unwrap()
        .sample(GlobalCell::new(33, 1, 1))
        .unwrap();
    assert!(
        matches!(under_beam, Sample::Filled(_)),
        "the floor span the beam rests on is still there: {under_beam:?}"
    );

    // No deep interpenetration: a body that sank into the floor rather than
    // resting on it would show a large penetration here.
    let pen = sim.world().physics().max_penetration_m();
    assert!(pen < 0.15, "contact penetration stays shallow ({pen} m)");
}
