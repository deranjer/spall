use super::*;

/// One reliable/repair message from a client to the sim bridge.
#[derive(Debug)]
pub(super) enum Inbound {
    /// A connection was accepted; the sim loop learns its session and any
    /// server-authenticated stable player principal.
    Joined(SessionId, Option<spall_protocol::PlayerId>),
    Action(SessionId, ActionRequest),
    Progression(SessionId, spall_protocol::ProgressionRequest),
    Repair(SessionId, RepairRequest),
    /// T19: a player movement input frame (datagram). At most one is applied per
    /// player per tick; redundant `recent` copies recover a dropped frame.
    Input(SessionId, InputFrame),
    /// A `BaselineAck`: the sentinel (`transfer_id == 0`) asks for a late-join
    /// baseline; a real id confirms one is installed.
    Baseline(SessionId, BaselineAck),
    /// A development admin command (world reset); gated by
    /// [`ServeConfig::admin_commands`].
    Admin(SessionId, spall_protocol::AdminRequest),
    Gone(SessionId),
}

pub(super) struct ProgressionWork {
    pub(super) session: SessionId,
    pub(super) player_id: Option<spall_protocol::PlayerId>,
    pub(super) request: spall_protocol::ProgressionRequest,
}

pub(super) struct ProgressionDone {
    pub(super) session: SessionId,
    pub(super) response: spall_protocol::ProgressionResponse,
}

pub(super) struct ProgressionWorker {
    pub(super) submit: std::sync::mpsc::SyncSender<ProgressionWork>,
    pub(super) completed: std::sync::mpsc::Receiver<ProgressionDone>,
}

pub(super) struct CommittedEditWork {
    pub(super) session: SessionId,
    pub(super) player_id: Option<spall_protocol::PlayerId>,
    pub(super) request_id: RequestId,
    pub(super) removed: std::collections::BTreeMap<spall_core::MaterialId, u64>,
}
pub(super) struct CommittedEditWorker {
    pub(super) submit: std::sync::mpsc::SyncSender<CommittedEditWork>,
}

impl CommittedEditWorker {
    pub(super) fn spawn(mut handler: CommittedEditHandler) -> Self {
        let (submit, work) = std::sync::mpsc::sync_channel::<CommittedEditWork>(64);
        std::thread::Builder::new()
            .name("spall-committed-game-events".into())
            .spawn(move || {
                while let Ok(work) = work.recv() {
                    handler(work.session, work.player_id, work.request_id, &work.removed);
                }
            })
            .expect("committed-edit worker thread must start");
        Self { submit }
    }
}

pub(super) struct OutboxWork(pub(super) spall_store::OutboxRecord);
pub(super) struct OutboxDone {
    pub(super) event_id: [u8; 32],
    pub(super) result: Result<(), String>,
}
pub(super) struct OutboxWorker {
    pub(super) submit: std::sync::mpsc::SyncSender<OutboxWork>,
    pub(super) completed: std::sync::mpsc::Receiver<OutboxDone>,
}

impl OutboxWorker {
    pub(super) fn spawn(mut processor: OutboxProcessor) -> Self {
        let (submit, work) = std::sync::mpsc::sync_channel::<OutboxWork>(1);
        let (done_tx, completed) = std::sync::mpsc::sync_channel::<OutboxDone>(1);
        std::thread::Builder::new()
            .name("spall-game-outbox".into())
            .spawn(move || {
                while let Ok(OutboxWork(event)) = work.recv() {
                    let done = OutboxDone {
                        event_id: event.event_id,
                        result: processor(&event),
                    };
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            })
            .expect("outbox worker thread must start");
        Self { submit, completed }
    }
}

impl ProgressionWorker {
    pub(super) fn spawn(mut handler: ProgressionHandler) -> Self {
        const CAPACITY: usize = 64;
        let (submit, work) = std::sync::mpsc::sync_channel::<ProgressionWork>(CAPACITY);
        let (done_tx, completed) = std::sync::mpsc::sync_channel::<ProgressionDone>(CAPACITY);
        std::thread::Builder::new()
            .name("spall-progression-requests".into())
            .spawn(move || {
                while let Ok(work) = work.recv() {
                    let response = handler(work.session, work.player_id, work.request);
                    if done_tx
                        .send(ProgressionDone {
                            session: work.session,
                            response,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("progression worker thread must start");
        Self { submit, completed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progression_queue_is_bounded_and_returns_in_serial_order() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = ProgressionWorker::spawn(Box::new(move |_session, _, request| {
            if request.request_id == 1 {
                let _ = started_tx.send(());
                std::thread::sleep(std::time::Duration::from_millis(75));
            }
            spall_protocol::ProgressionResponse {
                request_id: request.request_id,
                catalog_version: 1,
                inventory_revision: request.request_id,
                outcome: spall_protocol::ProgressionOutcome::Inventory,
                inventory: Vec::new(),
            }
        }));
        let session = sess(1, 1);
        let started = std::time::Instant::now();
        worker
            .submit
            .try_send(ProgressionWork {
                session,
                player_id: None,
                request: spall_protocol::ProgressionRequest {
                    request_id: 1,
                    catalog_version: 1,
                    expected_inventory_revision: 0,
                    operation: spall_protocol::ProgressionOperation::InspectInventory,
                },
            })
            .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        for request_id in 2..=65 {
            worker
                .submit
                .try_send(ProgressionWork {
                    session,
                    player_id: None,
                    request: spall_protocol::ProgressionRequest {
                        request_id,
                        catalog_version: 1,
                        expected_inventory_revision: 0,
                        operation: spall_protocol::ProgressionOperation::InspectInventory,
                    },
                })
                .expect("one active and 64 queued requests fit");
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert!(matches!(
            worker.submit.try_send(ProgressionWork {
                session,
                player_id: None,
                request: spall_protocol::ProgressionRequest {
                    request_id: 66,
                    catalog_version: 1,
                    expected_inventory_revision: 0,
                    operation: spall_protocol::ProgressionOperation::InspectInventory,
                },
            }),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
        for expected in 1..=65 {
            let done = worker
                .completed
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert_eq!(done.session, session);
            assert_eq!(done.response.request_id, expected);
        }
    }
}
