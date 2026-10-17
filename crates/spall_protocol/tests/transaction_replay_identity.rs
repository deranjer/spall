use spall_core::{GlobalCell, MaterialId, Tick, TransactionId, VolumeId};
use spall_protocol::{ControlSeq, TopologyOp, TopologyTransaction};

fn transaction() -> TopologyTransaction {
    TopologyTransaction {
        transaction_id: TransactionId::new(1).unwrap(),
        server_tick: Tick(1),
        control_seq: ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        result_hashes: vec![],
        ops: vec![TopologyOp::CellRun {
            volume: VolumeId::new(1).unwrap(),
            start: GlobalCell::new(0, 0, 0),
            len: 1,
            material: MaterialId::AIR,
        }],
    }
}

#[test]
fn replay_identity_ignores_transport_sequence_but_binds_ordered_payload() {
    let tx = transaction();
    let expected = tx.replay_identity_hash().unwrap();
    let mut resend = tx.clone();
    resend.control_seq = ControlSeq(u64::MAX);
    assert_eq!(resend.replay_identity_hash().unwrap(), expected);
    let mut changed = tx.clone();
    changed.ops.push(TopologyOp::CellRun {
        volume: VolumeId::new(2).unwrap(),
        start: GlobalCell::new(1, 0, 0),
        len: 1,
        material: MaterialId::AIR,
    });
    let ordered = changed.replay_identity_hash().unwrap();
    assert_ne!(ordered, expected);
    changed.ops.reverse();
    assert_ne!(changed.replay_identity_hash().unwrap(), ordered);
    changed = tx.clone();
    changed.transaction_id = TransactionId::new(2).unwrap();
    assert_ne!(changed.replay_identity_hash().unwrap(), expected);
}

#[test]
fn replay_identity_rejects_oversized_payload_before_encoding() {
    let mut tx = transaction();
    tx.ops = vec![
        TopologyOp::CellRun {
            volume: VolumeId::new(u64::MAX).unwrap(),
            start: GlobalCell::new(i64::MAX - 1, i64::MAX, i64::MAX),
            len: 1,
            material: MaterialId::AIR,
        };
        spall_protocol::limits::MAX_TRANSACTION_OPS
    ];
    assert!(matches!(
        tx.replay_identity_hash(),
        Err(spall_protocol::CodecError::FrameTooLarge { .. })
    ));
}
