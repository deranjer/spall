use super::*;
use spall_store::{JournalPayload, JournalRecord};

fn attached(
    mixed: bool,
) -> (
    TopologyTransaction,
    TransactionAttachmentSetV1,
    CreatureOwnershipRecordV4,
) {
    let (sim, cat) = fixture(false);
    let saved = sim.capture_creature_snapshot(&cat.view()).unwrap().unwrap();
    let after = saved.owners.records[0].clone();
    let id = TransactionId::new(1).unwrap();
    let ordinal = if mixed { 1 } else { 0 };
    let commit = CreatureLifecycleCommitV4 {
        schema: CREATURE_LIFECYCLE_COMMIT_SCHEMA,
        change: CreatureLifecycleChangeV4 {
            schema: CREATURE_CHANGE_SCHEMA,
            transaction_id: id,
            operation_ordinal: ordinal,
            expected_before_hash: Hash32::ZERO,
            expected_after_hash: after.canonical_hash(),
            operation: CreatureLifecycleOperationV4::Birth {
                after: after.clone(),
            },
        },
        expected_before_topology_hash: Hash32::ZERO,
        pre_motion: None,
        death_witness: None,
    };
    let payload = CreatureLifecycleBulkPayloadV4::new(id, ordinal, commit).unwrap();
    let actor_id = (1 << 61) | 17;
    let geometry_id = SPLIT_BULK_TRANSFER_ID_BIT | 18;
    let mut ops = Vec::new();
    let mut attachments = Vec::new();
    if mixed {
        ops.push(TopologyOp::SplitOffBulkBaseline {
            source: VolumeId::new(1).unwrap(),
            child: VolumeId::new(900).unwrap(),
            child_entity: EntityId::new(900).unwrap(),
            transfer_id: geometry_id,
        });
        attachments.push(TransactionAttachmentV1::Geometry {
            transfer_id: geometry_id,
            world: BaselineWorld {
                schema: BASELINE_WORLD_SCHEMA,
                checkpoint_tick: 1,
                volumes: vec![BaselineVolume {
                    volume_id: VolumeId::new(900).unwrap(),
                    cell_size_code: 0,
                    owner: BaselineOwner::Body(EntityId::new(900).unwrap()),
                    bounds: None,
                    bricks: vec![BaselineBrick {
                        coord: [0, 0, 0],
                        revision: 1,
                        edited: false,
                        cells: BaselineCells::Uniform(1),
                    }],
                }],
            },
        });
    }
    ops.push(TopologyOp::CreatureLifecycleBulk {
        transfer_id: actor_id,
        operation_ordinal: ordinal,
        payload_schema: CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA,
        commit_hash: payload.commit_hash,
    });
    attachments.push(TransactionAttachmentV1::CreatureLifecycle {
        transfer_id: actor_id,
        payload: Box::new(payload),
    });
    let tx = TopologyTransaction {
        transaction_id: id,
        server_tick: Tick(1),
        control_seq: ControlSeq(1),
        algorithm_version: 1,
        dependencies: vec![],
        before: vec![],
        after: vec![],
        ops,
        result_hashes: vec![],
    };
    (
        tx,
        TransactionAttachmentSetV1 {
            schema: TRANSACTION_ATTACHMENT_SET_SCHEMA,
            transaction_id: id,
            attachments,
        },
        after,
    )
}

fn base() -> spall_store::Checkpoint {
    let sim = Simulation::new(SimulationConfig::new(
        spall_sim::fixtures::flat_terrain_setup(),
    ))
    .unwrap();
    persist::capture(&sim, &cfg(), 0).unwrap()
}

#[test]
fn animal_and_mixed_typed_journal_payloads_survive_sqlite_with_exact_sources_and_motion() {
    for mixed in [false, true] {
        let (tx, set, after) = attached(mixed);
        // This actual three-part source record exceeds the inline record ceiling.
        let mut inline = tx.clone();
        let TransactionAttachmentV1::CreatureLifecycle { payload, .. } =
            set.attachments.last().unwrap()
        else {
            unreachable!()
        };
        *inline.ops.last_mut().unwrap() = TopologyOp::CreatureLifecycle {
            commit: Box::new(payload.commit.clone()),
        };
        inline.validate().unwrap();
        assert!(matches!(
            spall_protocol::encode_control(&inline),
            Err(spall_protocol::CodecError::Size(_))
        ));
        let row = JournalRecord {
            seq: 1,
            tick: 1,
            payload: JournalPayload::topology_with_attachments(&tx, &[], &set).unwrap(),
        };
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&base()).unwrap();
        writer.append_journal(std::slice::from_ref(&row)).unwrap();
        drop(writer);
        let recovery = spall_store::recover(scratch.db()).unwrap();
        assert!(recovery.corruption.is_empty());
        assert_eq!(recovery.journal, vec![row]);
        assert_eq!(recovery.durable_through, 1);
        let (parent, parts, decoded) = recovery.journal[0]
            .payload
            .as_topology_with_attachments()
            .unwrap()
            .unwrap();
        assert_eq!(parent, tx);
        assert!(parts.is_empty());
        assert!(decoded.matches_transaction(&tx));
        assert_eq!(decoded.iter().count(), if mixed { 2 } else { 1 });
        let Some(ValidatedTransactionAttachmentV1::CreatureLifecycle(payload)) =
            decoded.get((1 << 61) | 17)
        else {
            panic!("missing animal attachment")
        };
        let CreatureLifecycleOperationV4::Birth { after: actual } =
            &payload.commit.change.operation
        else {
            unreachable!()
        };
        assert_eq!(actual, &after);
        assert_eq!(
            payload.commit.change.expected_after_hash,
            after.canonical_hash()
        );
        // Codec retention is not runtime replay: neither family may be partially applied.
        let cat = Catalog::new(spall_sim::fixtures::stone_manifest());
        assert!(matches!(
            restore(&recovery, &cat),
            Err(persist::PersistError::CreatureJournalUnsupported)
        ));
        assert_eq!(
            persist::retained_transfer_ids(&recovery).unwrap().len(),
            if mixed { 2 } else { 1 }
        );
    }
}

#[test]
fn typed_animal_journal_fault_cuts_recover_only_complete_records() {
    for point in [
        CrashPoint::BeforeJournalCommit,
        CrashPoint::AfterJournalCommit,
    ] {
        let (tx, set, _) = attached(true);
        let row = JournalRecord {
            seq: 1,
            tick: 1,
            payload: JournalPayload::topology_with_attachments(&tx, &[], &set).unwrap(),
        };
        let scratch = Scratch::new();
        Writer::open(scratch.db())
            .unwrap()
            .publish_checkpoint(&base())
            .unwrap();
        let mut writer = Writer::open_with_faults(
            scratch.db(),
            FaultPlan {
                crash_at: Some(point),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(writer.append_journal(std::slice::from_ref(&row)).is_err());
        assert!(writer.is_poisoned());
        drop(writer);
        let recovery = spall_store::recover(scratch.db()).unwrap();
        assert!(recovery.corruption.is_empty());
        if point == CrashPoint::BeforeJournalCommit {
            assert!(recovery.journal.is_empty());
            assert_eq!(recovery.durable_through, 0);
        } else {
            assert_eq!(recovery.journal, vec![row]);
            assert_eq!(recovery.durable_through, 1);
        }
    }
}

#[test]
fn wrong_parent_ordinal_hash_family_and_crc_valid_trailing_bytes_refuse() {
    let (tx, set, _) = attached(true);
    for fault in 0..5 {
        let mut wrong = set.clone();
        match fault {
            0 => wrong.transaction_id = TransactionId::new(2).unwrap(),
            1 | 2 => {
                let TransactionAttachmentV1::CreatureLifecycle { payload, .. } =
                    wrong.attachments.last_mut().unwrap()
                else {
                    unreachable!()
                };
                if fault == 1 {
                    payload.operation_ordinal += 1;
                } else {
                    payload.commit_hash = Hash32::ZERO;
                }
            }
            3 => {
                wrong.attachments.pop();
            }
            _ => wrong.attachments.swap(0, 1),
        }
        if fault == 4 {
            // Order is not correlation: swapping complete typed entries is valid.
            assert!(JournalPayload::topology_with_attachments(&tx, &[], &wrong).is_ok());
        } else {
            assert!(JournalPayload::topology_with_attachments(&tx, &[], &wrong).is_err());
        }
    }
    let mut wrong_family = set.clone();
    if let TransactionAttachmentV1::Geometry { transfer_id, .. } = &mut wrong_family.attachments[0]
    {
        *transfer_id = (1 << 61) | 17;
    }
    assert!(JournalPayload::topology_with_attachments(&tx, &[], &wrong_family).is_err());
    for outer in [false, true] {
        let scratch = Scratch::new();
        let payload = JournalPayload::topology_with_attachments(&tx, &[], &set).unwrap();
        let row = JournalRecord {
            seq: 1,
            tick: 1,
            payload: payload.clone(),
        };
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&base()).unwrap();
        writer.append_journal(&[row]).unwrap();
        drop(writer);
        let mut bad = payload;
        if !outer {
            let JournalPayload::TopologyAttachments { attachments, .. } = &mut bad else {
                unreachable!()
            };
            attachments.push(0);
            assert!(bad.validate_auxiliary_bounds().is_err());
        }
        let mut bytes = spall_store::dto::encode(&bad).unwrap();
        if outer {
            bytes.push(0);
        }
        let crc = Hash32::of(&bytes).0[..16].to_vec();
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        conn.execute(
            "UPDATE journal SET payload=?1,crc=?2 WHERE seq=1",
            rusqlite::params![bytes, crc],
        )
        .unwrap();
        drop(conn);
        let recovered = spall_store::recover(scratch.db()).unwrap();
        assert_eq!(recovered.durable_through, 0);
        assert!(recovered.journal.is_empty());
        assert!(!recovered.corruption.is_empty());
    }
}

#[test]
fn invalid_typed_row_cannot_publish_a_valid_prefix_or_outbox() {
    for fault in 0..3 {
        let (tx, set, _) = attached(true);
        let good = JournalRecord {
            seq: 1,
            tick: 1,
            payload: JournalPayload::topology_with_attachments(&tx, &[], &set).unwrap(),
        };
        let mut bad = good.clone();
        bad.seq = 2;
        if fault == 0 {
            bad.tick = 2;
        } else {
            let JournalPayload::TopologyAttachments {
                attachments,
                participants,
                ..
            } = &mut bad.payload
            else {
                unreachable!()
            };
            if fault == 1 {
                attachments.push(0);
            } else {
                *participants = vec![vec![]; spall_protocol::limits::MAX_TRANSACTION_REFS + 1];
            }
        }
        let scratch = Scratch::new();
        let mut writer = Writer::open(scratch.db()).unwrap();
        writer.publish_checkpoint(&base()).unwrap();
        let outbox = spall_store::OutboxRecord {
            event_id: [1; 32],
            journal_seq: 1,
            payload: vec![7],
        };
        assert!(
            writer
                .append_journal_with_outbox(&[good, bad], &[outbox])
                .is_err()
        );
        assert!(writer.is_poisoned());
        drop(writer);
        let recovery = spall_store::recover(scratch.db()).unwrap();
        assert!(recovery.journal.is_empty());
        assert_eq!(recovery.durable_through, 0);
        let conn = rusqlite::Connection::open(scratch.db()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM game_outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
