//! T23 / G3 row 7, slice E — a replica that has evicted terrain bricks reloads
//! them through the ordinary repair path and drops the retained digest as the
//! authoritative geometry lands, so the logical view never carries a
//! resident-*and*-evicted brick and the replica still converges with the
//! server. See `docs/reports/G3-residency-hash.md` (reload lifecycle).

use spall_client::{ApplyOutcome, ClientResidency, ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, SphereBrush};
use spall_protocol::{Hash32, RepairKey, RepairRequest, RequestId};
use spall_server::{brick_repair_patch, world_baseline};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

fn server() -> Simulation {
    let mut setup = fixtures::separated_regions_setup();
    setup.physics.disable_ccd = true;
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

fn replica_of(sim: &Simulation) -> ReplicaWorld {
    ReplicaWorld::from_baseline_world(&world_baseline(sim), ReplicaConfig::default())
        .expect("the baseline installs")
}

/// East-region terrain bricks — at least two bricks (Manhattan) from the west
/// structure at brick `(0,0,0)`, so evicting them cannot affect a west edit.
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

/// A radius-1 excavation of the east floor end — terrain-only, detaches nothing.
fn east_floor_cut() -> EditIntent {
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(73 * BRUSH_UNIT + h, BRUSH_UNIT + h, 75 * BRUSH_UNIT + h),
        BRUSH_UNIT,
    )
    .unwrap();
    EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    )
}

fn repair_req(volume: spall_core::VolumeId, coord: BrickCoord) -> RepairRequest {
    RepairRequest {
        key: RepairKey::Brick { volume, coord },
        expected_revision: spall_core::Revision::ZERO,
        current_revision: spall_core::Revision::ZERO,
        expected_hash: Hash32::ZERO,
        current_hash: Hash32::ZERO,
    }
}

#[test]
fn a_repair_patch_reload_clears_the_replicas_retained_digest() {
    let sim = server();
    let mut replica = replica_of(&sim);
    let terrain = replica.terrain_volume_id();
    let server_hash = sim.world().world_hash();
    assert_eq!(replica.world_hash(), server_hash);

    let victims = east_bricks(&replica);
    assert!(victims.len() >= 2, "need a couple of evictable east bricks");
    for c in &victims {
        assert!(replica.evict_brick(terrain, *c), "brick {c:?} was resident");
    }
    assert!(!replica.evicted(terrain).is_empty());
    // Digests retained -> the reported hash is unmoved (slice B).
    assert_eq!(replica.world_hash(), server_hash);

    // Reload each evicted brick from an ordinary (resident-only) server repair
    // patch. The server never evicted anything, so no backing is needed.
    for c in &victims {
        let patch = brick_repair_patch(&sim, &repair_req(terrain, *c))
            .expect("the server serves its resident brick");
        replica
            .apply_baseline_patch(&patch)
            .expect("the patch applies");
    }

    assert!(
        replica.evicted(terrain).is_empty(),
        "the retained digests were not dropped as the geometry came back"
    );
    assert_eq!(
        replica.volume(terrain).unwrap().resident_brick_count(),
        sim.world().terrain().volume.resident_brick_count(),
    );
    // `world_hash` folds the logical view; a lingering digest would have
    // panicked here with a resident/evicted conflict.
    assert_eq!(replica.world_hash(), server_hash);
}

#[test]
fn an_edit_touching_an_evicted_replica_region_reloads_and_converges() {
    let mut sim = server();
    let terrain = sim.world().terrain_volume_id();
    let mut replica = replica_of(&sim);

    // The replica evicts the whole east region before the edit arrives.
    for c in east_bricks(&replica) {
        assert!(replica.evict_brick(terrain, c));
    }

    // The server excavates the east floor end and commits.
    sim.submit(east_floor_cut()).unwrap();
    sim.run_until_idle(24).unwrap();
    let tx = sim
        .committed(RequestId(1))
        .expect("the east floor cut committed")
        .topology
        .clone();
    let server_hash = sim.world().world_hash();
    assert_ne!(
        server_hash,
        replica.world_hash(),
        "the cut moved the server"
    );

    // The replica cannot apply it against an evicted `before` brick — it gaps
    // and asks for a repair, then applies once the geometry is back.
    let mut pending = match replica.apply_transaction(&tx) {
        ApplyOutcome::NeedsRepair(reqs) => reqs,
        other => panic!("expected NeedsRepair over the evicted region, got {other:?}"),
    };
    assert!(!pending.is_empty());

    // Every brick the transaction needed reloaded — its digest must be gone
    // afterwards even though far, untouched east bricks stay evicted.
    let touched: Vec<BrickCoord> = pending
        .iter()
        .map(|r| match r.key {
            RepairKey::Brick { coord, .. } => coord,
            _ => unreachable!(),
        })
        .collect();

    let mut guard = 0;
    while !pending.is_empty() {
        guard += 1;
        assert!(guard < 16, "repair loop did not converge");
        for req in std::mem::take(&mut pending) {
            let RepairKey::Brick { volume, coord } = req.key else {
                unreachable!("only brick repairs here")
            };
            if let Some(patch) = brick_repair_patch(&sim, &repair_req(volume, coord)) {
                replica.apply_baseline_patch(&patch).expect("patch applies");
            }
        }
        for (_, outcome) in replica.retry_pending_repair_txns() {
            if let ApplyOutcome::NeedsRepair(more) = outcome {
                pending.extend(more);
            }
        }
    }

    assert_eq!(
        replica.pending_repair_txn_count(),
        0,
        "a transaction stayed held after every repair landed"
    );
    for c in &touched {
        assert!(
            !replica.evicted(terrain).contains(*c),
            "digest for reloaded brick {c:?} was not dropped"
        );
    }
    assert_eq!(
        replica.world_hash(),
        server_hash,
        "the replica did not converge with the server after reloading the edited region"
    );
}

#[test]
fn wanted_reloads_targets_evicted_bricks_back_in_interest() {
    let sim = server();
    let mut replica = replica_of(&sim);
    let terrain = replica.terrain_volume_id();

    let east = east_bricks(&replica);
    for c in &east {
        assert!(replica.evict_brick(terrain, *c));
    }

    // Interest sitting on the resident west structure brick wants no reloads.
    assert!(
        ClientResidency::wanted_reloads(&replica, BrickCoord::new(0, 0, 0), 0, 64).is_empty(),
        "a resident interest centre should want no reloads"
    );

    // Interest centred on an evicted east brick names it (and any evicted
    // neighbour inside the box).
    let centre = east[0];
    let want = ClientResidency::wanted_reloads(&replica, centre, 1, 64);
    assert!(
        want.contains(&RepairKey::Brick {
            volume: terrain,
            coord: centre,
        }),
        "an evicted brick back in interest was not requested: {want:?}"
    );
    for key in &want {
        let RepairKey::Brick { coord, .. } = key else {
            unreachable!()
        };
        assert!(
            replica.evicted(terrain).contains(*coord),
            "wanted_reloads named a brick that is not evicted"
        );
    }
}
