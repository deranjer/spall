//! T23 / G3 row 7, slice B — a committed transaction validates on a replica
//! **regardless of which bricks each side has evicted**, because both compute
//! `result_hashes` / `world_hash` over the *logical* brick set (resident ∪
//! retained evicted digests). See `docs/reports/G3-residency-hash.md`.

use spall_client::{ApplyOutcome, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, SphereBrush, VolumeId};
use spall_protocol::{RequestId, TopologyTransaction};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

fn west_column_cut() -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(10 * BRUSH_UNIT + h, 6 * BRUSH_UNIT + h, 3 * BRUSH_UNIT + h),
        2 * BRUSH_UNIT,
    )
    .unwrap();
    EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

/// A fully-resident `separated_regions` server that cuts the west column.
/// Returns the sim and the committed transaction.
fn server_run() -> (Simulation, TopologyTransaction) {
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();

    sim.submit(west_column_cut()).unwrap();
    sim.run_until_idle(24).unwrap();

    let tx = sim
        .committed(RequestId(1))
        .expect("the west column cut committed")
        .topology
        .clone();
    (sim, tx)
}

fn fresh_replica() -> ReplicaWorld {
    ReplicaWorld::from_baseline(
        spall_voxel::fixtures::separated_regions_scene(VolumeId::new(1).unwrap()),
        ReplicaConfig::default(),
    )
}

/// Terrain bricks at least two bricks (Manhattan) from the west structure at
/// brick `(0,0,0)` — never a face neighbour of it, so evicting one cannot
/// affect the west column cut's structural analysis.
fn east_bricks(replica: &ReplicaWorld) -> Vec<BrickCoord> {
    let vid = replica.terrain_volume_id();
    replica
        .volume(vid)
        .unwrap()
        .resident_brick_coords()
        .into_iter()
        .filter(|c| c.x.abs() + c.y.abs() + c.z.abs() >= 2)
        .collect()
}

#[test]
fn a_replica_that_has_evicted_a_clean_brick_still_validates_a_full_server_transaction() {
    let (sim, tx) = server_run();

    let mut replica = fresh_replica();
    let terrain = replica.terrain_volume_id();
    // Evict bricks the west column cut does not touch or structurally depend on.
    for c in east_bricks(&replica) {
        assert!(replica.evict_brick(terrain, c), "brick {c:?} was resident");
    }
    assert!(!replica.evicted(terrain).is_empty());

    match replica.apply_transaction(&tx) {
        ApplyOutcome::Published { .. } => {}
        other => panic!("expected Published (result hash over the logical view), got {other:?}"),
    }
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "a replica with evicted bricks did not converge with the full server"
    );
}

#[test]
fn the_servers_reported_hash_is_the_same_before_and_after_it_evicts_a_brick() {
    let (mut sim, _tx) = server_run();
    let terrain = sim.world().terrain_volume_id();
    let full_replica = {
        let mut r = fresh_replica();
        r.apply_transaction(&_tx);
        r
    };
    let before = sim.world().world_hash();
    assert_eq!(before, full_replica.world_hash());

    for c in east_bricks(&full_replica) {
        assert!(sim.world_mut().evict_brick(terrain, c).unwrap());
    }
    assert_eq!(
        sim.world().world_hash(),
        before,
        "the server's reported world_hash moved when it evicted a clean brick"
    );
    assert_eq!(sim.world().world_hash(), full_replica.world_hash());
}

#[test]
fn evicting_a_replica_brick_does_not_move_its_world_hash() {
    let mut replica = fresh_replica();
    let terrain = replica.terrain_volume_id();
    let before = replica.world_hash();

    for c in east_bricks(&replica) {
        assert!(replica.evict_brick(terrain, c));
    }
    assert_eq!(
        replica.world_hash(),
        before,
        "replica world_hash moved when a clean brick was evicted"
    );
}
