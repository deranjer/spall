use spall_core::{EntityId, Tick, TransactionId, VolumeId};
use spall_protocol::{
    BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume, BaselineWorld, ControlSeq,
    TopologyOp, TopologyTransaction, TransactionAttachmentError, TransactionAttachmentSetV1,
    TransactionAttachmentV1, ValidatedTransactionAttachmentV1, WIRE_SCHEMA_VERSION,
    decode_for_transaction,
};

fn volume(id: u64, owner: BaselineOwner) -> BaselineVolume {
    BaselineVolume {
        volume_id: VolumeId::new(id).unwrap(),
        cell_size_code: 0,
        owner,
        bounds: None,
        bricks: vec![BaselineBrick {
            coord: [0, 0, 0],
            revision: 1,
            edited: false,
            cells: BaselineCells::Uniform(0),
        }],
    }
}

fn split_transaction() -> TopologyTransaction {
    let transfer_id = 77;
    TopologyTransaction {
        transaction_id: TransactionId::new(41).unwrap(),
        server_tick: Tick(12),
        control_seq: ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops: vec![
            TopologyOp::SplitOffBulkBaseline {
                source: VolumeId::new(1).unwrap(),
                child: VolumeId::new(2).unwrap(),
                child_entity: EntityId::new(20).unwrap(),
                transfer_id,
            },
            TopologyOp::SplitOffBulkBaseline {
                source: VolumeId::new(1).unwrap(),
                child: VolumeId::new(3).unwrap(),
                child_entity: EntityId::new(30).unwrap(),
                transfer_id,
            },
            TopologyOp::SourcePatchBulkBaseline {
                source: VolumeId::new(1).unwrap(),
                transfer_id,
            },
        ],
        result_hashes: vec![],
    }
}

fn split_world() -> BaselineWorld {
    BaselineWorld {
        schema: WIRE_SCHEMA_VERSION,
        checkpoint_tick: 12,
        volumes: vec![
            volume(1, BaselineOwner::Terrain),
            volume(2, BaselineOwner::Body(EntityId::new(20).unwrap())),
            volume(3, BaselineOwner::Body(EntityId::new(30).unwrap())),
        ],
    }
}

#[test]
fn typed_attachment_roundtrip_preserves_shared_geometry_split_group() {
    let transaction = split_transaction();
    let set = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::Geometry {
            transfer_id: 77,
            world: split_world(),
        }],
    };
    let bytes = set.encode_for_transaction(&transaction).unwrap();
    let decoded = decode_for_transaction(&bytes, &transaction).unwrap();
    assert_eq!(decoded.transaction_id(), transaction.transaction_id);
    assert!(decoded.matches_transaction(&transaction));
    let mut divergent_same_id = transaction.clone();
    divergent_same_id.server_tick = Tick(13);
    assert!(!decoded.matches_transaction(&divergent_same_id));
    assert!(matches!(
        decoded.get(77),
        Some(ValidatedTransactionAttachmentV1::Geometry(world)) if *world == split_world()
    ));
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        decode_for_transaction(&trailing, &transaction),
        Err(TransactionAttachmentError::DuplicateOrExtra)
    ));
    let mut truncated = bytes;
    truncated.pop();
    assert!(matches!(
        decode_for_transaction(&truncated, &transaction),
        Err(TransactionAttachmentError::Encoding(_))
    ));
}

#[test]
fn geometry_attachment_rejects_missing_extra_and_wrong_owner_volumes() {
    let transaction = split_transaction();
    let empty = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![],
    };
    assert!(matches!(
        empty.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Count)
    ));

    let extra_id = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::Geometry {
            transfer_id: 999,
            world: split_world(),
        }],
    };
    assert!(matches!(
        extra_id.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::DuplicateOrExtra)
    ));

    let duplicate_id = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![
            TransactionAttachmentV1::Geometry {
                transfer_id: 77,
                world: split_world(),
            },
            TransactionAttachmentV1::Geometry {
                transfer_id: 77,
                world: split_world(),
            },
        ],
    };
    assert!(matches!(
        duplicate_id.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Count)
    ));

    let mut missing = split_world();
    missing.volumes.pop();
    let set = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::Geometry {
            transfer_id: 77,
            world: missing,
        }],
    };
    assert!(matches!(
        set.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Correlation)
    ));

    let mut wrong_owner = split_world();
    wrong_owner.volumes[1].owner = BaselineOwner::Body(EntityId::new(999).unwrap());
    let set = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::Geometry {
            transfer_id: 77,
            world: wrong_owner,
        }],
    };
    assert!(matches!(
        set.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Correlation)
    ));

    let mut extra = split_world();
    extra.volumes.push(volume(4, BaselineOwner::Terrain));
    let set = TransactionAttachmentSetV1 {
        schema: WIRE_SCHEMA_VERSION,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::Geometry {
            transfer_id: 77,
            world: extra,
        }],
    };
    assert!(matches!(
        set.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Correlation)
    ));
}
