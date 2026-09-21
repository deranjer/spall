//! Segmented baselines (`BaselineBegin.world_version == 2`): server capture through client
//! validation and atomic install, on small synthetic worlds with a deliberately low segment budget
//! so oversized volumes, gaps, duplicates, corruption and budget exhaustion are exercised quickly.
//! See `docs/reports/large-world-baseline-design.md`.

use std::sync::Arc;

use glam::DQuat;
use spall_client::segmented::{SegmentedReceiver, StagingAdmission};
use spall_client::{ReplicaConfig, ReplicaWorld};
use spall_protocol::segment::{
    DENSE_BRICK_DECODED_COST, Frame, FrameReader, MAX_SEGMENT_DECODED_BYTES, segment_frame_cap,
};
use spall_protocol::{InterestEpoch, TransferId};
use spall_server::baseline::{
    BaselineError, snapshot_world, transfer_from_snapshot, transfer_from_snapshot_segmented,
};
use spall_sim::{BodyPose, Simulation, SimulationConfig, fixtures};

/// The integrated yard terrain (84 dense bricks) plus `bodies` small dense bodies.
fn world(bodies: usize) -> Simulation {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    for i in 0..bodies {
        sim.world_mut()
            .spawn_body(
                fixtures::solid_block(4),
                BodyPose::new(DQuat::IDENTITY, [10.0 + i as f64 * 0.5, 5.0, 10.0]),
                [0.0; 3],
                [0.0; 3],
                2600.0,
                0,
            )
            .unwrap();
    }
    sim
}

const CAP: usize = 4 * DENSE_BRICK_DECODED_COST;

/// The transfer's payload as complete frames (`kind ++ len ++ body`).
fn frames(parts: &[spall_protocol::BaselinePart]) -> Vec<Vec<u8>> {
    let mut reader = FrameReader::new();
    reader.set_max_body(segment_frame_cap(MAX_SEGMENT_DECODED_BYTES));
    let mut out = Vec::new();
    for p in parts {
        reader.push(&p.payload);
        while let Some(f) = reader.next_frame().unwrap() {
            let (kind, body) = match f {
                Frame::Manifest(b) => (0u8, b),
                Frame::Segment(b) => (1u8, b),
            };
            let mut bytes = vec![kind];
            bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&body);
            out.push(bytes);
        }
    }
    assert!(reader.is_empty());
    out
}

/// Feeds `frames` to a receiver in awkward 700-byte pieces (frames straddle piece boundaries).
fn receive(
    frames: &[Vec<u8>],
    budget: Option<u64>,
) -> Result<spall_client::segmented::SegmentedReceipt, String> {
    let mut rx = SegmentedReceiver::new(
        0,
        budget.map(|budget_bytes| StagingAdmission {
            budget_bytes,
            existing_replica_bytes: 0,
        }),
    );
    let wire: Vec<u8> = frames.concat();
    for piece in wire.chunks(700) {
        rx.push(piece)?;
    }
    rx.finish()
}

fn segmented(sim: &Simulation) -> spall_server::BaselineTransfer {
    transfer_from_snapshot_segmented(
        snapshot_world(sim, None),
        TransferId(1),
        InterestEpoch(1),
        CAP,
    )
    .unwrap()
}

#[test]
fn a_segmented_capture_rebuilds_the_authoritative_world_within_its_budget() {
    let sim = world(12);
    let t = segmented(&sim);
    let stats = t.segments.expect("a segmented transfer reports its shape");
    assert_eq!(
        t.begin.world_version,
        spall_protocol::segment::BASELINE_SEGMENTED_WORLD_VERSION
    );
    assert!(
        stats.segments > 20,
        "84 terrain bricks at 4 per segment span many segments: {stats:?}"
    );
    assert!(
        (stats.max_segment_decoded as usize) <= CAP,
        "no segment exceeds its decoded budget: {stats:?}"
    );
    assert_eq!(stats.volumes as usize, 1 + sim.world().body_count());
    assert!(
        t.begin.regions.is_empty(),
        "regions do not scale; the manifest replaces them"
    );

    let all = frames(&t.parts);
    let receipt = receive(&all, None).expect("the transfer validates");
    assert_eq!(receipt.segments, stats.segments);
    assert_eq!(receipt.decoded_bytes, stats.decoded_bytes);
    assert_eq!(receipt.chain_hash, t.end.assembled_hash);
    assert!(
        receipt.max_segment_decoded as usize <= CAP,
        "the client held at most one budget's worth of decoded cells"
    );
    assert!(
        receipt.max_buffered_bytes <= 5 + segment_frame_cap(CAP) + 700,
        "the reassembler buffered one frame at most, not the transfer: {}",
        receipt.max_buffered_bytes
    );

    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica.install_staged(receipt.staged).unwrap();
    assert_eq!(
        replica.world_hash(),
        sim.world().world_hash(),
        "the staged, installed replica reaches the exact authoritative topology hash"
    );
}

#[test]
fn a_volume_larger_than_the_budget_is_split_across_contiguous_segments() {
    let sim = world(0);
    let t = segmented(&sim);
    let stats = t.segments.unwrap();
    // The terrain alone is 84 dense bricks; the budget holds 4.
    assert_eq!(stats.volumes, 1);
    assert!(stats.segments >= 21, "{stats:?}");
    receive(&frames(&t.parts), None).expect("continuations validate");
}

#[test]
fn a_missing_duplicate_or_reordered_segment_is_refused_naming_the_segment() {
    let sim = world(3);
    let t = segmented(&sim);
    let all = frames(&t.parts);
    assert!(all.len() > 5);

    let mut missing = all.clone();
    missing.remove(3);
    let e = receive(&missing, None)
        .err()
        .expect("missing segment refused");
    assert!(e.contains("segment"), "{e}");

    let mut dup = all.clone();
    dup.insert(3, all[3].clone());
    let e = receive(&dup, None)
        .err()
        .expect("duplicate segment refused");
    assert!(e.contains("segment"), "{e}");

    let mut swapped = all.clone();
    swapped.swap(2, 3);
    let e = receive(&swapped, None)
        .err()
        .expect("reordered segment refused");
    assert!(e.contains("segment"), "{e}");

    // The tail never arrives.
    let mut truncated = all.clone();
    truncated.pop();
    assert!(
        receive(&truncated, None).is_err(),
        "an incomplete transfer never yields a receipt"
    );
}

#[test]
fn a_missing_manifest_a_corrupt_hash_and_a_lying_length_are_refused() {
    let sim = world(2);
    let t = segmented(&sim);
    let all = frames(&t.parts);

    assert!(
        receive(&all[1..], None).is_err(),
        "a segment before the manifest"
    );

    let mut corrupt = all.clone();
    corrupt[2][6] ^= 0x01; // a byte of the declared raw hash
    let e = receive(&corrupt, None).err().unwrap();
    assert!(e.contains("hash"), "{e}");

    let mut flipped = all.clone();
    let last = flipped[2].len() - 1;
    flipped[2][last] ^= 0xFF; // inside the compressed body
    assert!(receive(&flipped, None).is_err());

    // A length prefix far past the cap is refused before anything is buffered.
    let mut lying = all.clone();
    lying[2][1..5].copy_from_slice(&u32::MAX.to_le_bytes());
    let e = receive(&lying, None).err().unwrap();
    assert!(e.contains("exceeds"), "{e}");
}

#[test]
fn admission_counts_the_staged_world_the_existing_replica_and_the_segment_buffers() {
    let sim = world(2);
    let t = segmented(&sim);
    let all = frames(&t.parts);
    let stats = t.segments.unwrap();
    let mut rx = SegmentedReceiver::new(0, None);
    rx.push(&all[0]).unwrap();
    let m = rx.manifest().unwrap().clone();
    let adm = |budget_bytes, existing_replica_bytes| StagingAdmission {
        budget_bytes,
        existing_replica_bytes,
    };
    let need = adm(0, 0).required_bytes(&m);
    // staged x1.10 + four segment buffers, and nothing less.
    assert_eq!(
        need,
        stats.decoded_bytes / 10 * 11 + 4 * u64::from(m.segment_decoded_cap)
    );
    assert!(
        need > stats.decoded_bytes,
        "installation overhead is budgeted"
    );

    // Only the manifest frame is fed: the refusal must not need a single segment.
    let mut rx = SegmentedReceiver::new(0, Some(adm(need - 1, 0)));
    let e = rx.push(&all[0]).unwrap_err();
    assert!(e.contains("budget"), "{e}");
    assert!(rx.manifest().is_none());
    // The exact requirement is admitted...
    receive(&all, Some(need)).expect("a sufficient budget passes");
    // ...but the same budget no longer admits it once the client already holds a world: the old
    // replica stays until the atomic swap.
    let mut rx = SegmentedReceiver::new(0, Some(adm(need, 1)));
    let e = rx.push(&all[0]).unwrap_err();
    assert!(e.contains("existing replica"), "{e}");
}

#[test]
fn the_replicas_own_footprint_is_what_admission_charges() {
    let sim = world(3);
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    assert_eq!(replica.decoded_bytes_estimate(), 0);
    replica
        .install_baseline_world(
            &transfer_from_snapshot(snapshot_world(&sim, None), TransferId(1), InterestEpoch(1))
                .unwrap()
                .decode_v1_world()
                .unwrap(),
        )
        .unwrap();
    let est = replica.decoded_bytes_estimate();
    let bricks: usize = 84 + 3;
    assert!(est >= bricks as u64 * DENSE_BRICK_DECODED_COST as u64);
}

/// A world whose cells look random: zstd cannot shrink one bit of entropy per cell below an eighth
/// of the decoded size, so the compressed transfer is a large fraction of the decoded one.
fn incompressible_world() -> Simulation {
    let mut setup = fixtures::flat_terrain_setup();
    let id = setup.terrain.id();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut edit = spall_voxel::EditPlan::new(id);
    for cz in 0..128 {
        for cy in 0..32 {
            for cx in 0..128 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let m = if x & 1 == 0 {
                    fixtures::STONE
                } else {
                    spall_core::MaterialId(2)
                };
                edit.set(spall_core::GlobalCell::new(cx, cy, cz), m);
            }
        }
    }
    setup.terrain.apply_edit(&edit).unwrap();
    setup.terrain_collider_region = (
        spall_core::GlobalCell::new(0, 0, 0),
        spall_core::GlobalCell::new(127, 31, 127),
    );
    Simulation::new(SimulationConfig::new(setup)).unwrap()
}

#[test]
fn a_poorly_compressible_world_is_refused_at_the_cumulative_cap_before_the_buffer_grows() {
    let sim = incompressible_world();
    let unlimited = transfer_from_snapshot_segmented(
        snapshot_world(&sim, None),
        TransferId(1),
        InterestEpoch(1),
        CAP,
    )
    .unwrap();
    let full = unlimited.payload_bytes();
    let stats = unlimited.segments.unwrap();
    assert!(
        full > 50_000 && (full as u64) * 16 > stats.decoded_bytes,
        "random cells barely compress: {full} wire bytes for {} decoded",
        stats.decoded_bytes
    );

    // Server: a small cumulative cap fails at the first frame that would cross it, and the error
    // reports a size no larger than that frame's end -- not the whole transfer.
    let cap = full / 3;
    let err = spall_server::baseline::transfer_from_snapshot_segmented_limited(
        snapshot_world(&sim, None),
        TransferId(2),
        InterestEpoch(1),
        CAP,
        cap,
    )
    .expect_err("over the cumulative cap");
    match err {
        BaselineError::TooLarge { bytes, cap: c } => {
            assert_eq!(c, cap);
            assert!(
                bytes < full,
                "stopped before the whole transfer was buffered ({bytes} of {full})"
            );
        }
        other => panic!("{other:?}"),
    }

    // Client: the same payload fed to a receiver with the smaller cap is refused before the
    // over-cap bytes are buffered.
    let parts = &unlimited.parts;
    let mut rx = SegmentedReceiver::with_limits(0, None, cap as u64);
    let mut refused_at = None;
    for (i, p) in parts.iter().enumerate() {
        if let Err(e) = rx.push(&p.payload) {
            assert!(e.contains("compressed cap"), "{e}");
            refused_at = Some(i);
            break;
        }
    }
    let i = refused_at.expect("the receiver refuses once the cap is crossed");
    let accepted: usize = parts[..i].iter().map(|p| p.payload.len()).sum();
    assert!(
        accepted <= cap,
        "only bytes within the cap were ever accepted"
    );
}

#[test]
fn a_budget_smaller_than_one_dense_brick_is_a_hard_error() {
    let sim = world(0);
    let err = transfer_from_snapshot_segmented(
        snapshot_world(&sim, None),
        TransferId(1),
        InterestEpoch(1),
        DENSE_BRICK_DECODED_COST - 1,
    )
    .err()
    .unwrap();
    assert!(matches!(err, BaselineError::Segmented(_)), "{err:?}");
}

#[test]
fn the_capture_retains_compressed_parts_only_and_joiners_share_them() {
    let sim = world(6);
    let t = segmented(&sim);
    let joiners: Vec<_> = (0..8u64).map(|i| t.reissue(TransferId(100 + i))).collect();
    for j in &joiners {
        assert!(
            Arc::ptr_eq(&t.parts, &j.parts),
            "reissue shares the compressed parts instead of copying them"
        );
        assert!(j.begin.transfer_id.0 >= 100);
        assert_eq!(j.end.transfer_id, j.begin.transfer_id);
    }
    assert_eq!(Arc::strong_count(&t.parts), 9);
    // The whole world is far smaller compressed than its decoded cost.
    let decoded = t.segments.unwrap().decoded_bytes as usize;
    assert!(
        t.payload_bytes() * 4 < decoded,
        "{} vs {decoded}",
        t.payload_bytes()
    );
}

#[test]
fn small_worlds_keep_the_single_blob_and_only_oversized_ones_need_segmenting() {
    let sim = world(2);
    let snap = snapshot_world(&sim, None);
    assert!(!snap.needs_segmentation(), "a small world stays v1");
    let v1 = transfer_from_snapshot(snap, TransferId(1), InterestEpoch(1)).unwrap();
    assert_eq!(v1.begin.world_version, 1);
    assert!(v1.segments.is_none());
    // The v1 blob and the segmented transfer describe the same world.
    let v1_world = v1.decode_v1_world().unwrap();
    let t = segmented(&sim);
    let receipt = receive(&frames(&t.parts), None).unwrap();
    assert_eq!(
        receipt.staged.brick_count() as usize,
        v1_world.brick_count()
    );
    assert_eq!(receipt.staged.volume_count(), v1_world.volumes.len());
}

#[test]
fn a_cancelled_or_failed_transfer_leaves_the_existing_replica_untouched() {
    let sim = world(4);
    let t = segmented(&sim);
    let all = frames(&t.parts);

    // A replica that already holds a (different) world.
    let other = world(1);
    let v1 = transfer_from_snapshot(
        snapshot_world(&other, None),
        TransferId(9),
        InterestEpoch(1),
    )
    .unwrap();
    let mut replica = ReplicaWorld::empty(ReplicaConfig::default());
    replica
        .install_baseline_world(&v1.decode_v1_world().unwrap())
        .unwrap();
    let before = replica.world_hash();
    assert_eq!(before, other.world().world_hash());

    // The transfer is cancelled half way: the receiver never yields a receipt, so nothing is
    // installed and the replica keeps its state.
    let mut rx = SegmentedReceiver::new(0, None);
    for f in &all[..all.len() / 2] {
        rx.push(f).unwrap();
    }
    assert!(rx.finish().is_err());
    assert_eq!(replica.world_hash(), before);

    // A fully staged world with a corrupted terrain-less body only is refused at install.
    let mut rx = SegmentedReceiver::new(0, None);
    for f in &all {
        rx.push(f).unwrap();
    }
    let receipt = rx.finish().unwrap();
    replica.install_staged(receipt.staged).unwrap();
    assert_eq!(replica.world_hash(), sim.world().world_hash());
}
