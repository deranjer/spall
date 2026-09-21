use glam::DQuat;
use spall_core::{EntityId, player_entity_for};
use spall_sim::{
    BodyPose, DebrisLifetimeConfig, DebrisLifetimePolicy, Simulation, SimulationConfig, fixtures,
};

fn config() -> DebrisLifetimeConfig {
    DebrisLifetimeConfig {
        max_solid_volume_m3: 0.125,
        dormant_ticks: 3,
        player_clearance_m: 5.0,
        body_clearance_m: 0.5,
        max_candidates_per_tick: 8,
        max_removals_per_tick: 1,
    }
}
fn scene() -> Simulation {
    Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap()
}
fn fragment(sim: &mut Simulation, x: f64, edge: i64) -> EntityId {
    let id = sim
        .world_mut()
        .spawn_body(
            fixtures::solid_block(edge),
            BodyPose::new(DQuat::IDENTITY, [x, 2.0, 4.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    assert!(sim.world_mut().deactivate_body(id));
    id
}
fn step(sim: &mut Simulation, p: &mut DebrisLifetimePolicy) -> spall_sim::TickReport {
    let mut report = sim.tick().unwrap();
    sim.apply_debris_lifetime(p, &mut report).unwrap();
    report
}

#[test]
fn only_explicitly_approved_small_debris_expires_and_replay_agrees() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 2);
    let protected = fragment(&mut sim, 10.0, 2);
    let large = fragment(&mut sim, 16.0, 3);
    let mut replay = scene();
    fragment(&mut replay, 4.0, 2);
    fragment(&mut replay, 10.0, 2);
    fragment(&mut replay, 16.0, 3);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    p.approve(large).unwrap();
    for _ in 0..3 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
    let r = step(&mut sim, &mut p);
    assert_eq!(r.debris_retired.len(), 1);
    assert_eq!(r.debris_retired[0].entity, a);
    assert_eq!(r.debris_retired[0].destroyed_cells, 8);
    assert!(sim.world().body(a).is_none());
    assert!(sim.world().body(protected).is_some());
    assert!(sim.world().body(large).is_some());
    assert_eq!(sim.journal().entries().len(), 1);
    let entry = &sim.journal().entries()[0];
    replay
        .world_mut()
        .replay_transaction(&entry.transaction, &entry.participants, None)
        .unwrap();
    assert_eq!(sim.world().world_hash(), replay.world().world_hash());
    for _ in 0..6 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
}

#[test]
fn wake_and_redormancy_between_observations_restart_the_entire_grace_period() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 1);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    for _ in 0..3 {
        step(&mut sim, &mut p);
    }
    assert!(sim.world_mut().reactivate_body(a));
    assert!(sim.world_mut().deactivate_body(a));
    for _ in 0..3 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
    assert_eq!(step(&mut sim, &mut p).debris_retired.len(), 1);
}

#[test]
fn player_presence_and_supporting_neighbours_protect_debris() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 1);
    let b = fragment(&mut sim, 10.0, 1);
    let neighbour = fragment(&mut sim, 10.2, 1);
    sim.add_player(player_entity_for(1), [4.0, 2.0, 4.0]);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    p.approve(b).unwrap();
    for _ in 0..12 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
    for id in [a, b, neighbour] {
        assert!(sim.world().body(id).is_some());
    }
}

#[test]
fn revocation_and_fresh_policy_protect_bodies_and_budget_limits_removals() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 1);
    let b = fragment(&mut sim, 10.0, 1);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    p.approve(b).unwrap();
    for _ in 0..3 {
        step(&mut sim, &mut p);
    }
    p.protect(a);
    assert_eq!(step(&mut sim, &mut p).debris_retired[0].entity, b);
    let mut fresh = DebrisLifetimePolicy::new(config()).unwrap();
    for _ in 0..6 {
        assert!(step(&mut sim, &mut fresh).debris_retired.is_empty());
    }
    let c = fragment(&mut sim, 16.0, 1);
    fresh.approve(a).unwrap();
    fresh.approve(c).unwrap();
    for _ in 0..3 {
        step(&mut sim, &mut fresh);
    }
    assert_eq!(step(&mut sim, &mut fresh).debris_retired.len(), 1);
    assert_eq!(step(&mut sim, &mut fresh).debris_retired.len(), 1);
}

#[test]
fn missing_policy_ticks_cannot_count_as_observed_dormancy() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 1);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    step(&mut sim, &mut p);
    for _ in 0..6 {
        sim.tick().unwrap();
    }
    for _ in 0..3 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
    assert_eq!(step(&mut sim, &mut p).debris_retired.len(), 1);
}

#[test]
fn pending_body_edit_takes_precedence_over_expiry() {
    use spall_core::{
        SphereBrush,
        units::{BRUSH_UNIT, BrushPoint},
    };
    use spall_sim::{EditIntent, EditTarget, RequestId};
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 2);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    for _ in 0..3 {
        step(&mut sim, &mut p);
    }
    sim.submit(EditIntent::cut(
        RequestId(10),
        EntityId::new(1).unwrap(),
        EditTarget::Body(a),
        SphereBrush::new(
            BrushPoint::from_units(BRUSH_UNIT / 2, BRUSH_UNIT / 2, BRUSH_UNIT / 2),
            BRUSH_UNIT / 2,
        )
        .unwrap(),
    ))
    .unwrap();
    for _ in 0..8 {
        assert!(step(&mut sim, &mut p).debris_retired.is_empty());
    }
    assert!(sim.world().body(a).is_some());
    assert!(
        sim.committed(RequestId(10)).is_some(),
        "the player's edit still commits"
    );
}

#[test]
fn late_retirement_failure_does_not_delete_matter_or_append_a_journal_entry() {
    let mut sim = scene();
    let a = fragment(&mut sim, 4.0, 1);
    let mut p = DebrisLifetimePolicy::new(config()).unwrap();
    p.approve(a).unwrap();
    for _ in 0..3 {
        step(&mut sim, &mut p);
    }
    let (ne, nv, nt, _) = sim.world().registry().counters();
    *sim.world_mut().registry_mut() = spall_sim::IdRegistry::resume(ne, nv, nt, u64::MAX).unwrap();
    let hash = sim.world().world_hash();
    let counters = sim.world().registry().counters();
    let mut report = sim.tick().unwrap();
    assert!(sim.apply_debris_lifetime(&mut p, &mut report).is_err());
    assert_eq!(sim.world().world_hash(), hash);
    assert_eq!(sim.world().registry().counters(), counters);
    assert!(sim.world().body_is_dormant(a));
    assert!(sim.journal().entries().is_empty());
}

#[test]
fn invalid_or_unbounded_policy_configuration_is_refused() {
    let mut c = config();
    c.max_solid_volume_m3 = f64::NAN;
    assert!(DebrisLifetimePolicy::new(c).is_err());
    c = config();
    c.dormant_ticks = 0;
    assert!(DebrisLifetimePolicy::new(c).is_err());
    c = config();
    c.max_candidates_per_tick = usize::MAX;
    assert!(DebrisLifetimePolicy::new(c).is_err());
    c = config();
    c.body_clearance_m = 0.0;
    assert!(DebrisLifetimePolicy::new(c).is_err());
}
