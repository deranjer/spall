//! T17 late-join baseline: a captured baseline, reassembled and installed into a
//! fresh replica, reproduces the server's canonical topology hash exactly — and
//! a targeted brick repair patch restores parity (revision included) after a
//! client-side divergence.
//!
//! The networked third-client-joins-mid-collapse acceptance is
//! `late_join_session.rs`; this file pins the conversion in isolation.

use spall_client::{ReplicaConfig, ReplicaWorld};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, EntityId, Revision, SphereBrush, VolumeId};
use spall_protocol::{
    BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume, BaselineWorld, Hash32, RepairKey,
    RepairRequest,
};
use spall_server::{brick_repair_patch, capture_transfer};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig, fixtures};

fn bridge_after_cut() -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup()))
        .expect("bridge scene is valid");
    let h = BRUSH_UNIT / 2;
    let brush = SphereBrush::new(
        BrushPoint::from_units(10 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
        2 * BRUSH_UNIT,
    )
    .expect("brush is valid");
    sim.submit(EditIntent::cut(
        spall_protocol::RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        brush,
    ))
    .expect("submit");
    sim.run_until_idle(24).expect("run to idle");
    sim
}

fn assemble(parts: &[spall_protocol::BaselinePart]) -> BaselineWorld {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(&p.payload);
    }
    BaselineWorld::decode_compressed(&bytes).expect("assembled baseline decompresses/decodes")
}

#[test]
fn a_reassembled_baseline_reproduces_the_server_hash_with_no_replay() {
    let sim = bridge_after_cut();
    assert!(sim.world().body_count() >= 1, "the cut detached a body");
    let server_hash = sim.world().world_hash();

    let transfer = capture_transfer(
        &sim,
        spall_protocol::TransferId(1),
        spall_protocol::InterestEpoch(1),
        spall_core::JournalSeq(sim.journal().entries().len() as u64),
    )
    .expect("capture");

    // Every part validates and reassembly matches the captured world.
    for part in transfer.parts.iter() {
        assert!(!part.payload.is_empty());
    }
    let world = assemble(&transfer.parts);
    assert_eq!(Hash32::of(&world.encode()), transfer.end.assembled_hash);

    let replica = ReplicaWorld::from_baseline_world(&world, ReplicaConfig::default())
        .expect("install baseline");

    assert_eq!(
        replica.world_hash(),
        server_hash,
        "late-join replica hash equals the server world hash — no edit replay from creation"
    );
    assert_eq!(replica.total_solid_cells(), sim.world().total_solid_cells());
    assert_eq!(replica.body_ids().count(), sim.world().body_count());
}

#[test]
fn a_brick_repair_patch_restores_exact_parity_after_a_client_divergence() {
    let sim = bridge_after_cut();
    let server_hash = sim.world().world_hash();

    let transfer = capture_transfer(
        &sim,
        spall_protocol::TransferId(2),
        spall_protocol::InterestEpoch(1),
        spall_core::JournalSeq(0),
    )
    .expect("capture");
    let world = assemble(&transfer.parts);
    let mut replica = ReplicaWorld::from_baseline_world(&world, ReplicaConfig::default())
        .expect("install baseline");
    assert_eq!(replica.world_hash(), server_hash);

    // Pick a real terrain brick and corrupt it in the replica: wrong revision
    // and wrong material, exactly the failure a `before`-revision check catches.
    let terrain = &world.volumes[0];
    assert!(matches!(terrain.owner, BaselineOwner::Terrain));
    let target = terrain.bricks[0].clone();
    let terrain_vid = terrain.volume_id;

    let corrupt = BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: world.checkpoint_tick,
        volumes: vec![BaselineVolume {
            volume_id: terrain_vid,
            cell_size_code: terrain.cell_size_code,
            owner: BaselineOwner::Terrain,
            bounds: terrain.bounds,
            bricks: vec![BaselineBrick {
                coord: target.coord,
                revision: target.revision + 1_000,
                edited: true,
                cells: BaselineCells::Uniform(0),
            }],
        }],
    };
    replica
        .apply_baseline_patch(&corrupt)
        .expect("apply corruption");
    assert_ne!(
        replica.world_hash(),
        server_hash,
        "the corruption actually diverged the replica"
    );

    // The server answers the client's RepairRequest with the authoritative
    // one-brick patch.
    let coord = BrickCoord::new(target.coord[0], target.coord[1], target.coord[2]);
    let request = RepairRequest {
        key: RepairKey::Brick {
            volume: terrain_vid,
            coord,
        },
        expected_revision: Revision(target.revision),
        current_revision: Revision(target.revision + 1_000),
        expected_hash: Hash32::ZERO,
        current_hash: Hash32::ZERO,
    };
    let patch = brick_repair_patch(&sim, &request).expect("server produces a brick patch");
    replica.apply_baseline_patch(&patch).expect("apply repair");

    assert_eq!(
        replica.world_hash(),
        server_hash,
        "the repair patch restored exact parity, revision included"
    );
}

#[test]
fn a_repair_patch_cannot_target_a_volume_the_replica_lacks() {
    let sim = bridge_after_cut();
    let transfer = capture_transfer(
        &sim,
        spall_protocol::TransferId(3),
        spall_protocol::InterestEpoch(1),
        spall_core::JournalSeq(0),
    )
    .expect("capture");
    let world = assemble(&transfer.parts);
    let mut replica = ReplicaWorld::from_baseline_world(&world, ReplicaConfig::default()).unwrap();

    let bogus = BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: 0,
        volumes: vec![BaselineVolume {
            volume_id: VolumeId::new(9_999).unwrap(),
            cell_size_code: world.volumes[0].cell_size_code,
            owner: BaselineOwner::Body(EntityId::new(9_999).unwrap()),
            bounds: Some([[0, 0, 0], [0, 0, 0]]),
            bricks: vec![BaselineBrick {
                coord: [0, 0, 0],
                revision: 1,
                edited: false,
                cells: BaselineCells::Uniform(1),
            }],
        }],
    };
    assert!(replica.apply_baseline_patch(&bogus).is_err());
}
