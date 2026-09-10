//! T23 / G3 row 7, slice C — the late-join baseline and one-brick repair
//! capture paths include a server-evicted brick's durable geometry, so a joiner
//! reaches the exact canonical topology hash.

use spall_client::{ReplicaConfig, ReplicaWorld};
use spall_core::BrickCoord;
use spall_protocol::{RepairKey, RepairRequest};
use spall_server::{logical_brick_repair_patch, logical_world_baseline, world_baseline};
use spall_sim::{MemoryBacking, Simulation, SimulationConfig, fixtures};

fn sim_with_backing() -> (Simulation, MemoryBacking) {
    let sim = Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let backing = MemoryBacking::from_volume(&sim.world().terrain().volume);
    (sim, backing)
}

fn evictable_bricks(sim: &Simulation) -> Vec<BrickCoord> {
    sim.world()
        .terrain()
        .volume
        .resident_brick_coords()
        .into_iter()
        .filter(|c| c.x.abs() + c.y.abs() + c.z.abs() >= 2)
        .collect()
}

#[test]
fn a_late_join_baseline_over_an_evicted_world_reconstructs_the_full_topology() {
    let (mut sim, backing) = sim_with_backing();
    let terrain = sim.world().terrain_volume_id();
    let full_hash = sim.world().world_hash();

    for c in evictable_bricks(&sim) {
        assert!(sim.world_mut().evict_brick(terrain, c).unwrap());
    }
    assert!(sim.world().has_evicted());
    // The evicted-aware server hash is unchanged (slice B); the baseline must
    // reach the same value on a fully resident replica.
    assert_eq!(sim.world().world_hash(), full_hash);

    let baseline = logical_world_baseline(&sim, Some(&backing));
    let replica = ReplicaWorld::from_baseline_world(&baseline, ReplicaConfig::default())
        .expect("the baseline installs");

    assert_eq!(
        replica.world_hash(),
        full_hash,
        "a joiner did not reconstruct the full world from an evicted server"
    );
}

#[test]
fn a_one_brick_repair_patch_covers_an_evicted_brick() {
    let (mut sim, backing) = sim_with_backing();
    let terrain = sim.world().terrain_volume_id();
    let victim = evictable_bricks(&sim)[0];
    let want_rev = sim
        .world()
        .terrain()
        .volume
        .brick_revision(victim)
        .unwrap()
        .unwrap();

    assert!(sim.world_mut().evict_brick(terrain, victim).unwrap());

    // A plain resident-only patch cannot serve it...
    assert!(
        spall_server::brick_repair_patch(
            &sim,
            &RepairRequest {
                key: RepairKey::Brick {
                    volume: terrain,
                    coord: victim
                },
                expected_revision: want_rev,
                current_revision: spall_core::Revision::ZERO,
                expected_hash: spall_protocol::Hash32::ZERO,
                current_hash: spall_protocol::Hash32::ZERO,
            },
        )
        .is_none()
    );

    // ...but the backing-aware one does, at the right revision.
    let patch = logical_brick_repair_patch(
        &sim,
        &RepairRequest {
            key: RepairKey::Brick {
                volume: terrain,
                coord: victim,
            },
            expected_revision: want_rev,
            current_revision: spall_core::Revision::ZERO,
            expected_hash: spall_protocol::Hash32::ZERO,
            current_hash: spall_protocol::Hash32::ZERO,
        },
        Some(&backing),
    )
    .expect("the backing-aware repair patch serves the evicted brick");

    let bv = &patch.volumes[0];
    assert_eq!(bv.bricks.len(), 1);
    assert_eq!(
        [
            bv.bricks[0].coord[0],
            bv.bricks[0].coord[1],
            bv.bricks[0].coord[2]
        ],
        [victim.x, victim.y, victim.z]
    );
    assert_eq!(bv.bricks[0].revision, want_rev.get());
}

#[test]
fn world_baseline_is_unchanged_when_nothing_is_evicted() {
    let sim = Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup())).unwrap();
    let a = world_baseline(&sim);
    let b = logical_world_baseline(&sim, None);
    assert_eq!(a.encode(), b.encode());

    let replica = ReplicaWorld::from_baseline_world(&a, ReplicaConfig::default()).unwrap();
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}
