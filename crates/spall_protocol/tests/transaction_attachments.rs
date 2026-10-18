use spall_core::{
    CreatureGeneration, CreatureId, CreatureKey, EntityId, Tick, TransactionId, VolumeId,
    WorldEpoch, WorldId,
};
use spall_protocol::{
    BASELINE_WORLD_SCHEMA, BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume,
    BaselineWorld, CREATURE_CHANGE_SCHEMA, CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
    CREATURE_LIFECYCLE_COMMIT_SCHEMA, CREATURE_OWNER_SCHEMA, CREATURE_STATE_SCHEMA, ControlSeq,
    CreatureAssetIdentity, CreatureLifecycleBulkPayloadV3, CreatureLifecycleChangeV3,
    CreatureLifecycleCommitV3, CreatureLifecycleOperationV3, CreatureLocalPoseV1,
    CreatureOwnershipRecordV3, CreatureSourcePartV1, CreatureStateV1, Hash32,
    TRANSACTION_ATTACHMENT_SET_SCHEMA, TopologyOp, TopologyTransaction, TransactionAttachmentError,
    TransactionAttachmentSetV1, TransactionAttachmentV1, ValidatedTransactionAttachmentV1,
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
        schema: BASELINE_WORLD_SCHEMA,
        checkpoint_tick: 12,
        volumes: vec![
            volume(1, BaselineOwner::Terrain),
            volume(2, BaselineOwner::Body(EntityId::new(20).unwrap())),
            volume(3, BaselineOwner::Body(EntityId::new(30).unwrap())),
        ],
    }
}

fn lifecycle_commit(transaction_id: TransactionId) -> CreatureLifecycleCommitV3 {
    let after = CreatureOwnershipRecordV3 {
        primary_component_serial: None,
        part_velocities: vec![],
        schema: CREATURE_OWNER_SCHEMA,
        state: CreatureStateV1 {
            schema: CREATURE_STATE_SCHEMA,
            key: CreatureKey::new(
                WorldId::from_u128(9),
                WorldEpoch::INITIAL,
                CreatureId::new(1).unwrap(),
                CreatureGeneration::INITIAL,
            )
            .unwrap(),
            state_revision: 1,
            lifecycle: spall_protocol::CreatureLifecycleState::Alive,
            species_key: "deer".into(),
            species_version: 1,
            asset: CreatureAssetIdentity {
                uuid: [1; 16],
                logical_hash: Hash32([2; 32]),
            },
            material_manifest_hash: Hash32([3; 32]),
            material_mapping_version: 1,
            material_mapping_hash: Hash32([4; 32]),
            clip_name: "idle".into(),
            clip_mode: spall_protocol::CreatureClipMode::Playing,
            clip_phase_tick: 0,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            source_bindings: vec![],
            aggregate_topology_revision: 1,
            pose_epoch: 1,
            root_translation_m: [0.0; 3],
            root_rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
        },
        owner_key: 1,
        source_generation: 1,
        next_component_serial: 1,
        parts: vec![],
        components: vec![],
    };
    let change = CreatureLifecycleChangeV3 {
        schema: CREATURE_CHANGE_SCHEMA,
        transaction_id,
        operation_ordinal: 0,
        expected_before_hash: Hash32::ZERO,
        expected_after_hash: after.canonical_hash(),
        operation: CreatureLifecycleOperationV3::Birth { after },
    };
    CreatureLifecycleCommitV3 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change,
        expected_before_topology_hash: Hash32::ZERO,
        pre_motion: None,
        death_witness: None,
    }
}

fn compact_uniform_commit(
    transaction_id: TransactionId,
    ordinal: u32,
) -> CreatureLifecycleCommitV3 {
    let mut commit = lifecycle_commit(transaction_id);
    let CreatureLifecycleOperationV3::Birth { after } = &mut commit.change.operation else {
        unreachable!();
    };
    let bricks: Vec<_> = (0..30)
        .map(|x| BaselineBrick {
            coord: [x, 0, 0],
            revision: 1,
            edited: false,
            cells: BaselineCells::Uniform(1),
        })
        .collect();
    after.parts.push(CreatureSourcePartV1 {
        part_id: 1,
        source_volume_id: VolumeId::new(1).unwrap(),
        source_generation: 1,
        source_revision: 1,
        cell_size_code: 0,
        bounds: None,
        source_content_hash: Hash32([5; 32]),
        parent_id: None,
        initial_local_pose: CreatureLocalPoseV1 {
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
        frozen_local_pose: CreatureLocalPoseV1 {
            translation_m: [0.0; 3],
            rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
        },
        bricks,
    });
    commit.change.transaction_id = transaction_id;
    commit.change.operation_ordinal = ordinal;
    commit.change.expected_after_hash = after.canonical_hash();
    commit
}

fn lifecycle_set_with_five_compact_uniform_records()
-> (TopologyTransaction, TransactionAttachmentSetV1) {
    let transaction_id = TransactionId::new(71).unwrap();
    let mut ops = Vec::new();
    let mut attachments = Vec::new();
    for ordinal in 0..5_u32 {
        let transfer_id = 500 + u64::from(ordinal);
        let payload = CreatureLifecycleBulkPayloadV3::new(
            transaction_id,
            ordinal,
            compact_uniform_commit(transaction_id, ordinal),
        )
        .unwrap();
        ops.push(TopologyOp::CreatureLifecycleBulk {
            operation_ordinal: ordinal,
            payload_schema: CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
            commit_hash: payload.commit_hash,
            transfer_id,
        });
        attachments.push(TransactionAttachmentV1::CreatureLifecycle {
            transfer_id,
            payload: Box::new(payload),
        });
    }
    let transaction = TopologyTransaction {
        transaction_id,
        server_tick: Tick(12),
        control_seq: ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops,
        result_hashes: vec![],
    };
    let set = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
        transaction_id,
        attachments,
    };
    (transaction, set)
}

#[test]
fn typed_attachment_roundtrip_preserves_shared_geometry_split_group() {
    let transaction = split_transaction();
    let set = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
        transaction_id: transaction.transaction_id,
        attachments: vec![],
    };
    assert!(matches!(
        empty.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Count)
    ));

    let extra_id = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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

    let wrong_family_payload = CreatureLifecycleBulkPayloadV3::new(
        transaction.transaction_id,
        0,
        lifecycle_commit(transaction.transaction_id),
    )
    .unwrap();
    let wrong_family = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
        transaction_id: transaction.transaction_id,
        attachments: vec![TransactionAttachmentV1::CreatureLifecycle {
            transfer_id: 77,
            payload: Box::new(wrong_family_payload),
        }],
    };
    assert!(matches!(
        wrong_family.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Correlation)
    ));

    let mut missing = split_world();
    missing.volumes.pop();
    let set = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
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

#[test]
fn lifecycle_attachment_is_correlated_to_marker_before_candidate_acceptance() {
    let transaction_id = TransactionId::new(52).unwrap();
    let commit = lifecycle_commit(transaction_id);
    let payload = CreatureLifecycleBulkPayloadV3::new(transaction_id, 0, commit).unwrap();
    let transaction = TopologyTransaction {
        transaction_id,
        server_tick: Tick(12),
        control_seq: ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops: vec![TopologyOp::CreatureLifecycleBulk {
            operation_ordinal: 0,
            payload_schema: CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
            commit_hash: payload.commit_hash,
            transfer_id: 88,
        }],
        result_hashes: vec![],
    };
    let set = TransactionAttachmentSetV1 {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
        transaction_id,
        attachments: vec![TransactionAttachmentV1::CreatureLifecycle {
            transfer_id: 88,
            payload: Box::new(payload.clone()),
        }],
    };
    let bytes = set.encode_for_transaction(&transaction).unwrap();
    let decoded = decode_for_transaction(&bytes, &transaction).unwrap();
    assert!(matches!(
        decoded.get(88),
        Some(ValidatedTransactionAttachmentV1::CreatureLifecycle(decoded))
            if decoded.as_ref() == &payload
    ));

    let mut wrong_marker = transaction.clone();
    let TopologyOp::CreatureLifecycleBulk { commit_hash, .. } = &mut wrong_marker.ops[0] else {
        unreachable!();
    };
    *commit_hash = Hash32([9; 32]);
    assert!(matches!(
        set.encode_for_transaction(&wrong_marker),
        Err(TransactionAttachmentError::Correlation)
    ));
}

#[test]
fn compact_uniform_lifecycle_work_is_cumulative_across_attachments() {
    let (transaction, set) = lifecycle_set_with_five_compact_uniform_records();
    assert!(matches!(
        set.encode_for_transaction(&transaction),
        Err(TransactionAttachmentError::Lifecycle(_))
    ));

    // Build the bounded outer framing from individually valid payloads so the
    // decoder must enforce the same aggregate owner-work budget.
    #[derive(serde::Serialize)]
    struct Prefix {
        schema: u16,
        transaction_id: TransactionId,
        count: u32,
    }
    let mut bytes = postcard::to_stdvec(&Prefix {
        schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
        transaction_id: transaction.transaction_id,
        count: set.attachments.len() as u32,
    })
    .unwrap();
    for attachment in &set.attachments {
        let TransactionAttachmentV1::CreatureLifecycle {
            transfer_id,
            payload,
        } = attachment
        else {
            unreachable!();
        };
        let compressed = payload.encode_compressed().unwrap();
        bytes.extend(postcard::to_stdvec(transfer_id).unwrap());
        bytes.push(1);
        bytes.extend(postcard::to_stdvec(&(compressed.len() as u32)).unwrap());
        bytes.extend(compressed);
    }
    assert!(matches!(
        decode_for_transaction(&bytes, &transaction),
        Err(TransactionAttachmentError::Lifecycle(_))
    ));
}
