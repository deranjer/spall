//! T21 (increment 2) — region dormancy for settled debris, end to end.
//!
//! Drives one authoritative [`Simulation`] with the real physics world and the
//! [`DormancyPolicy`], covering the `sleep-wake` acceptance:
//!
//! * settled rubble with a quiet interaction region is deactivated — dropped
//!   from the physics step while its authoritative record is untouched
//!   (`settled_debris_deactivates_without_changing_the_world`);
//! * an edit targeting dormant rubble wakes it and it is still destructible
//!   (`an_edit_wakes_dormant_rubble_and_it_stays_destructible`);
//! * a player approaching dormant rubble reactivates it
//!   (`a_player_approaching_wakes_dormant_rubble`);
//! * dormancy never changes `world_hash` or conservation vs. a run without it
//!   (`dormancy_is_invisible_to_the_authoritative_state`).

use glam::DQuat;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush, player_entity_for};
use spall_sim::{
    BodyPose, DormancyConfig, DormancyPolicy, EditIntent, EditTarget, Simulation, SimulationConfig,
    fixtures, solid_cells,
};
use spall_voxel::EditPlan;

fn thick_slab_setup() -> spall_sim::WorldSetup {
    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    setup
        .terrain
        .apply_edit(&EditPlan::filled_box(
            id,
            spall_core::GlobalCell::new(0, 0, 0),
            spall_core::GlobalCell::new(23, 5, 23),
            fixtures::STONE,
        ))
        .unwrap();
    setup
}

fn test_config() -> DormancyConfig {
    DormancyConfig {
        settle_ticks: 20,
        wake_margin_m: 3.0,
        min_dormant_ticks: 5,
        max_deactivations_per_tick: 8,
        max_reactivations_per_tick: 8,
        still_speed_m_s: 0.05,
    }
}

/// Drops a `1 m` stone cube over `(x, z) ≈ (4.5, 4.5)` m and runs (ticking +
/// `apply_dormancy`) until it is dormant or `max_ticks` is hit. Returns the
/// body entity and the tick it went dormant.
fn drop_and_settle(
    sim: &mut Simulation,
    policy: &mut DormancyPolicy,
    max_ticks: u32,
) -> (EntityId, Option<u32>) {
    let entity = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(4),
            BodyPose::new(DQuat::IDENTITY, [4.0, 5.0, 4.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let mut dormant_at = None;
    for tick in 1..=max_ticks {
        sim.tick().unwrap();
        sim.apply_dormancy(policy);
        if dormant_at.is_none() && sim.world().body_is_dormant(entity) {
            dormant_at = Some(tick);
        }
    }
    (entity, dormant_at)
}

fn body_solid_cells(sim: &Simulation, entity: EntityId) -> u64 {
    let vid = sim.world().body(entity).unwrap().volume_id;
    solid_cells(sim.world().volume_ref(vid).unwrap())
}

fn cut_body(entity: EntityId, req: u64, x: i64, y: i64, z: i64, r: i64) -> EditIntent {
    let h = BRUSH_UNIT / 2;
    EditIntent::cut(
        spall_sim::RequestId(req),
        EntityId::new(1).unwrap(),
        EditTarget::Body(entity),
        SphereBrush::new(
            BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
            r * BRUSH_UNIT,
        )
        .unwrap(),
    )
}

#[test]
fn settled_debris_deactivates_without_changing_the_world() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = DormancyPolicy::new(test_config());

    let (entity, dormant_at) = drop_and_settle(&mut sim, &mut policy, 500);

    assert!(
        dormant_at.is_some(),
        "the settled cube went dormant with no player nearby"
    );
    assert!(sim.world().body_is_dormant(entity));
    assert_eq!(sim.world().dormant_body_count(), 1);
    assert_eq!(
        sim.world().physics().active_body_count(),
        1,
        "only the fixed terrain is still stepped"
    );
    // The body is still a full authoritative entity: enumerable, owns its
    // volume, all its mass still present.
    assert_eq!(sim.world().body_count(), 1);
    assert!(sim.world().body(entity).unwrap().sleeping);
    assert!(sim.world().total_solid_cells() > 0);
}

#[test]
fn an_edit_wakes_dormant_rubble_and_it_stays_destructible() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = DormancyPolicy::new(test_config());
    let (entity, dormant_at) = drop_and_settle(&mut sim, &mut policy, 500);
    assert!(dormant_at.is_some());

    let before = body_solid_cells(&sim, entity);

    // A cut targeting the dormant body: `submit` must wake it so the edit lands.
    let status = sim
        .submit(cut_body(entity, 1, 2, 2, 2, 2))
        .expect("intent admitted");
    assert!(
        matches!(status.outcome, spall_protocol::ActionOutcome::Queued),
        "edit against dormant rubble is queued, not rejected"
    );
    assert!(
        !sim.world().body_is_dormant(entity),
        "submitting an edit reactivated the body"
    );

    sim.run_until_idle(60).unwrap();
    assert!(
        sim.committed(spall_sim::RequestId(1)).is_some(),
        "the cut committed"
    );
    assert!(
        body_solid_cells(&sim, entity) < before,
        "dormant rubble is still destructible ({} -> {})",
        before,
        body_solid_cells(&sim, entity)
    );
}

#[test]
fn a_player_approaching_wakes_dormant_rubble() {
    let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
    let mut policy = DormancyPolicy::new(test_config());
    let (entity, dormant_at) = drop_and_settle(&mut sim, &mut policy, 500);
    assert!(dormant_at.is_some());

    // A player materialises next to the settled cube (world ~4.5, *, 4.5).
    sim.add_player(player_entity_for(0), [4.5, 1.6, 5.0]);

    let mut woke_at = None;
    for tick in 0..40 {
        sim.tick().unwrap();
        sim.apply_dormancy(&mut policy);
        if woke_at.is_none() && !sim.world().body_is_dormant(entity) {
            woke_at = Some(tick);
        }
    }
    assert!(
        woke_at.is_some(),
        "the dormant cube reactivated once the player was within the wake margin"
    );
    assert_eq!(sim.world().dormant_body_count(), 0);
    assert_eq!(sim.world().physics().active_body_count(), 2);
}

#[test]
fn dormancy_is_invisible_to_the_authoritative_state() {
    // Same drop, run the same number of ticks, with and without the dormancy
    // pass and with nothing ever interacting with the body.
    let run = |with_dormancy: bool| {
        let mut sim = Simulation::new(SimulationConfig::new(thick_slab_setup())).unwrap();
        let mut policy = DormancyPolicy::new(test_config());
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
        for _ in 0..500 {
            sim.tick().unwrap();
            if with_dormancy {
                sim.apply_dormancy(&mut policy);
            }
        }
        (
            sim.world().world_hash(),
            sim.world().total_solid_cells(),
            sim.world().body_count(),
        )
    };

    let plain = run(false);
    let dormant = run(true);
    assert_eq!(plain.0, dormant.0, "dormancy did not change the world hash");
    assert_eq!(plain.1, dormant.1, "conservation identical");
    assert_eq!(plain.2, dormant.2, "same body count");
}
