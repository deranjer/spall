//! An in-process transport check: one server, N authenticated clients,
//! optionally behind per-client UDP proxies, exchanging every channel and then
//! shutting down under a hard deadline.
//!
//! It is the shared body of the `spall_net` integration tests and
//! `cargo xtask net-check`. Nothing here runs simulation or storage; the server
//! is a trivial echo.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;

use spall_core::{
    BrushPoint, CellSizeCode, EntityId, JournalSeq, Pose, QuantizedQuat, Revision, SphereBrush,
    Tick, WorldId,
};
use spall_protocol::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, AlgorithmVersions, ClaimedTarget,
    DurableThrough, Handshake, Hash32, InputFrame, InputSeq, MotionSnapshot, NegotiatedLimits,
    PROTOCOL_VERSION, RequestId, SnapshotSeq,
};

use crate::conn::{Connection, DatagramRecord};
use crate::endpoint::{NetServer, connect};
use crate::message::WireRecord;
use crate::proxy::{PacketFaultPlan, PacketStats, UdpProxy};
use crate::tls::{DevIdentity, JoinToken};
use crate::{Result, TransportConfig, TransportError};

/// Content-manifest tag the harness server and clients agree on.
pub const HARNESS_MANIFEST_TAG: &[u8] = b"spall-net-harness-manifest";

/// Inputs to [`run_transport_check`].
#[derive(Debug, Clone)]
pub struct TransportCheckParams {
    /// How many clients to connect.
    pub clients: usize,
    /// Reliable `ActionRequest` records each client sends (and expects echoed).
    pub records_per_client: usize,
    /// `InputFrame` datagrams each client sends.
    pub datagrams_per_client: usize,
    /// If set, every client runs through its own opaque UDP proxy with this
    /// impairment plan.
    pub proxy: Option<PacketFaultPlan>,
    /// If true the server pushes a small three-part baseline on a bulk stream.
    pub run_bulk_transfer: bool,
    /// Transport timers and limits.
    pub config: TransportConfig,
    /// Whole-run deadline. Exceeding it is a failure, never a hang.
    pub overall_timeout: Duration,
}

impl Default for TransportCheckParams {
    fn default() -> Self {
        Self {
            clients: 2,
            records_per_client: 8,
            datagrams_per_client: 16,
            proxy: None,
            run_bulk_transfer: true,
            config: TransportConfig::for_tests(),
            overall_timeout: Duration::from_secs(20),
        }
    }
}

/// Per-client result.
#[derive(Debug, Clone)]
pub struct ClientOutcome {
    pub session: String,
    pub connected: bool,
    pub records_sent: u64,
    pub records_recv: u64,
    pub datagrams_sent: u64,
    pub datagrams_recv: u64,
    pub bulk_parts_recv: u64,
    pub dedup_dropped: u64,
}

/// Aggregate result of a transport check.
#[derive(Debug, Clone)]
pub struct TransportCheckReport {
    pub clients_connected: usize,
    pub control_records_echoed: u64,
    pub datagrams_delivered: u64,
    pub bulk_parts_delivered: u64,
    pub dedup_dropped: u64,
    pub app_bytes_sent: u64,
    pub app_bytes_recv: u64,
    pub transport_bytes_sent: u64,
    pub transport_bytes_recv: u64,
    pub clients: Vec<ClientOutcome>,
    pub proxy_stats: Vec<PacketStats>,
}

impl TransportCheckReport {
    /// True when every client connected, every reliable record round-tripped,
    /// and (if requested) the bulk transfer arrived in full.
    pub fn all_reliable_delivered(&self, params: &TransportCheckParams) -> bool {
        let want_records = (params.clients * params.records_per_client) as u64;
        let want_parts = if params.run_bulk_transfer {
            (params.clients * 3) as u64
        } else {
            0
        };
        self.clients_connected == params.clients
            && self.control_records_echoed == want_records
            && self.bulk_parts_delivered == want_parts
    }
}

/// A minimal, self-consistent [`Handshake`] the harness and tests use as the
/// compatibility baseline. `manifest_tag` lets a test deliberately build a
/// mismatching client.
pub fn demo_handshake(manifest_tag: &[u8]) -> Handshake {
    Handshake {
        protocol_version: PROTOCOL_VERSION,
        content_manifest_hash: Hash32::of(manifest_tag),
        world_id: WorldId::from_u128(0x5A11_0000_0000_0001),
        generator_version: 1,
        algorithms: AlgorithmVersions {
            integer_brush: 1,
            structure_graph: 1,
            topology_hash: 1,
        },
        server_tick_hz: 60,
        motion_snapshot_hz: 20,
        cell_size_codes: vec![CellSizeCode::Quarter],
        session: spall_protocol::SessionId::from_parts(spall_protocol::SlotId(0), 1),
        limits: NegotiatedLimits::DEFAULT,
    }
}

/// Runs the check. Returns `Err(TransportError::Timeout)` if the whole run,
/// including teardown, does not finish inside `params.overall_timeout`.
pub async fn run_transport_check(params: TransportCheckParams) -> Result<TransportCheckReport> {
    let timeout = params.overall_timeout;
    tokio::time::timeout(timeout, run_inner(params))
        .await
        .map_err(|_| TransportError::Timeout(timeout))?
}

async fn run_inner(params: TransportCheckParams) -> Result<TransportCheckReport> {
    let identity = DevIdentity::generate()?;
    let token = JoinToken::generate()?;
    let fingerprint = identity.fingerprint();

    let server = Arc::new(
        NetServer::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            &identity,
            token,
            demo_handshake(HARNESS_MANIFEST_TAG),
            params.config,
        )
        .await?,
    );
    let server_addr = server.local_addr()?;

    // Server accept loop: authenticate, then echo.
    let (stop_tx, stop_rx) = watch::channel(false);
    let accept_server = server.clone();
    let accept_cfg = params.clone();
    let server_conns: Arc<tokio::sync::Mutex<Vec<Arc<Connection>>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let accept_conns = server_conns.clone();
    let accept_stop = stop_rx.clone();
    let accept_task = tokio::spawn(async move {
        for _ in 0..accept_cfg.clients {
            match accept_server.accept().await {
                Ok(conn) => {
                    let conn = Arc::new(conn);
                    accept_conns.lock().await.push(conn.clone());
                    spawn_server_side(conn, accept_cfg.clone(), accept_stop.clone());
                }
                Err(e) => {
                    tracing::warn!("harness server accept failed: {e}");
                    break;
                }
            }
        }
    });

    // Clients.
    let mut proxies: Vec<Arc<UdpProxy>> = Vec::new();
    let mut client_tasks = Vec::new();
    for i in 0..params.clients {
        let target = if let Some(plan) = params.proxy {
            let plan = PacketFaultPlan {
                seed: plan.seed ^ (i as u64 + 1),
                ..plan
            };
            let proxy = UdpProxy::spawn(server_addr, plan).await?;
            let addr = proxy.local_addr();
            proxies.push(proxy);
            addr
        } else {
            server_addr
        };
        let cfg = params.clone();
        let stop_rx = stop_rx.clone();
        client_tasks.push(tokio::spawn(async move {
            run_client(i, target, fingerprint, token, cfg, stop_rx).await
        }));
    }

    let mut clients = Vec::new();
    let mut clients_connected = 0;
    let mut transport_bytes_sent = 0u64;
    let mut transport_bytes_recv = 0u64;
    for task in client_tasks {
        match task.await {
            Ok(Ok((outcome, tx, rx))) => {
                if outcome.connected {
                    clients_connected += 1;
                }
                transport_bytes_sent += tx;
                transport_bytes_recv += rx;
                clients.push(outcome);
            }
            Ok(Err(e)) => return Err(e),
            Err(join) => {
                return Err(TransportError::Connect(format!(
                    "client task panic: {join}"
                )));
            }
        }
    }

    // Stop server-side pumps and tear down.
    let _ = stop_tx.send(true);
    let _ = accept_task.await;
    server.close();
    tokio::time::timeout(Duration::from_secs(2), server.wait_idle())
        .await
        .ok();
    for proxy in &proxies {
        proxy.shutdown().await;
    }

    let proxy_stats = proxies.iter().map(|p| p.stats()).collect();

    let control_records_echoed = clients.iter().map(|c| c.records_recv).sum();
    let datagrams_delivered = clients.iter().map(|c| c.datagrams_recv).sum();
    let bulk_parts_delivered = clients.iter().map(|c| c.bulk_parts_recv).sum();
    let dedup_dropped = {
        let server_side: u64 = {
            let conns = server_conns.lock().await;
            conns.iter().map(|c| c.stats().dedup_dropped).sum()
        };
        server_side + clients.iter().map(|c| c.dedup_dropped).sum::<u64>()
    };
    let app_bytes_sent = clients
        .iter()
        .map(|c| c.records_sent + c.datagrams_sent)
        .sum();

    Ok(TransportCheckReport {
        clients_connected,
        control_records_echoed,
        datagrams_delivered,
        bulk_parts_delivered,
        dedup_dropped,
        app_bytes_sent,
        app_bytes_recv: control_records_echoed,
        transport_bytes_sent,
        transport_bytes_recv,
        clients,
        proxy_stats,
    })
}

fn spawn_server_side(
    conn: Arc<Connection>,
    params: TransportCheckParams,
    stop: watch::Receiver<bool>,
) {
    // Liveness.
    tokio::spawn(conn.clone().run_liveness(stop.clone()));

    // Control echo.
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(Some(record)) = conn.recv_record().await {
                let reply = match record {
                    WireRecord::ActionRequest(req) => WireRecord::ActionStatus(ActionStatus {
                        request_id: req.request_id,
                        outcome: ActionOutcome::Committed {
                            transaction: spall_core::TransactionId::new(req.request_id.0.max(1))
                                .unwrap(),
                        },
                    }),
                    WireRecord::DurableThrough(d) => WireRecord::DurableThrough(d),
                    _ => continue,
                };
                if conn.send_record(reply).await.is_err() {
                    break;
                }
            }
        });
    }

    // Datagram echo: reply to each InputFrame with a MotionSnapshot.
    {
        let conn = conn.clone();
        let ack = AtomicU64::new(0);
        tokio::spawn(async move {
            while let Ok(Some(DatagramRecord::Input(frame))) = conn.recv_datagram().await {
                let seq = ack.fetch_add(1, Ordering::Relaxed);
                let snap = MotionSnapshot {
                    server_tick: Tick(seq + 1),
                    snapshot_seq: SnapshotSeq(seq),
                    acked_input: frame.input_seq,
                    body: EntityId::new(1).unwrap(),
                    topology_revision: Revision(1),
                    pose: Pose {
                        translation_m: [0.0, 0.0, 0.0],
                        rotation: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
                    },
                    linear_velocity: [0.0; 3],
                    angular_velocity: [0.0; 3],
                    sleeping: false,
                };
                if conn.send_datagram(seq, &snap).await.is_err() {
                    break;
                }
            }
        });
    }

    // Optional bulk push.
    if params.run_bulk_transfer {
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Ok(mut bulk) = conn.open_bulk().await {
                for i in 0..3u32 {
                    let part = spall_protocol::BaselinePart {
                        transfer_id: spall_protocol::TransferId(1),
                        part_index: i,
                        payload: vec![i as u8 + 1; 512],
                        part_hash: Hash32::of(&vec![i as u8 + 1; 512]),
                    };
                    if bulk.send_part(&part).await.is_err() {
                        return;
                    }
                }
                let _ = bulk.finish();
            }
        });
    }
}

type ClientResult = Result<(ClientOutcome, u64, u64)>;

async fn run_client(
    index: usize,
    target: SocketAddr,
    fingerprint: crate::tls::Fingerprint,
    token: JoinToken,
    params: TransportCheckParams,
    stop: watch::Receiver<bool>,
) -> ClientResult {
    let conn = match connect(
        target,
        fingerprint,
        token,
        demo_handshake(HARNESS_MANIFEST_TAG),
        params.config,
    )
    .await
    {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::warn!("harness client {index} could not connect: {e}");
            return Ok((
                ClientOutcome {
                    session: format!("client{index}:unconnected"),
                    connected: false,
                    records_sent: 0,
                    records_recv: 0,
                    datagrams_sent: 0,
                    datagrams_recv: 0,
                    bulk_parts_recv: 0,
                    dedup_dropped: 0,
                },
                0,
                0,
            ));
        }
    };
    tokio::spawn(conn.clone().run_liveness(stop));

    // Reader task: count reliable echoes until we have all of them.
    let want = params.records_per_client as u64;
    let reader = {
        let conn = conn.clone();
        tokio::spawn(async move {
            let mut got = 0u64;
            while got < want {
                match conn.recv_record().await {
                    Ok(Some(_)) => got += 1,
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            got
        })
    };

    // Bulk receiver.
    let bulk_reader = if params.run_bulk_transfer {
        let conn = conn.clone();
        Some(tokio::spawn(async move {
            match conn.accept_bulk().await {
                Ok(bulk) => bulk
                    .collect_parts()
                    .await
                    .map(|p| p.len() as u64)
                    .unwrap_or(0),
                Err(_) => 0,
            }
        }))
    } else {
        None
    };

    // Datagram reply reader (best-effort: datagrams are lossy).
    let dgram_reader = {
        let conn = conn.clone();
        let want = params.datagrams_per_client as u64;
        tokio::spawn(async move {
            let mut got = 0u64;
            while got < want {
                match tokio::time::timeout(Duration::from_millis(800), conn.recv_datagram()).await {
                    Ok(Ok(Some(_))) => got += 1,
                    _ => break,
                }
            }
            got
        })
    };

    // Send reliable records.
    let mut records_sent = 0u64;
    for r in 0..params.records_per_client {
        let req = ActionRequest {
            request_id: RequestId(((index as u64) << 32) | (r as u64 + 1)),
            input_seq: InputSeq(r as u64),
            action: ActionKind::Cut,
            tool: 1,
            aim_origin_m: [0.0, 1.0, 0.0],
            aim_dir: [0.0, 0.0, 1.0],
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 4).unwrap(),
        };
        conn.send_record(WireRecord::ActionRequest(req)).await?;
        records_sent += 1;
    }
    // One reliable DurableThrough for good measure.
    conn.send_record(WireRecord::DurableThrough(DurableThrough {
        journal_seq: JournalSeq(7),
    }))
    .await
    .ok();

    // Send datagrams, deliberately replaying seq 0 once to exercise dedup.
    let mut datagrams_sent = 0u64;
    for d in 0..params.datagrams_per_client {
        let frame = InputFrame {
            session: conn.session(),
            player: EntityId::new(1).unwrap(),
            input_seq: InputSeq(d as u64),
            intended_tick: Tick(d as u64),
            movement: [0.0, 0.0, 0.0],
            view_dir: [0.0, 0.0, 1.0],
            buttons: 0,
            recent: vec![],
        };
        conn.send_datagram(d as u64, &frame).await.ok();
        datagrams_sent += 1;
        if d == 0 {
            // exact replay of seq 0
            conn.send_datagram(0, &frame).await.ok();
        }
    }

    let records_recv = tokio::time::timeout(Duration::from_secs(8), reader)
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    let bulk_parts_recv = match bulk_reader {
        Some(h) => tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .map(|r| r.unwrap_or(0))
            .unwrap_or(0),
        None => 0,
    };
    let datagrams_recv = tokio::time::timeout(Duration::from_secs(5), dgram_reader)
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);

    let stats = conn.stats();
    let tstats = conn.transport_stats();
    conn.say_bye("done").await.ok();
    conn.close("client complete");
    tokio::time::timeout(Duration::from_secs(2), conn.closed())
        .await
        .ok();

    Ok((
        ClientOutcome {
            session: conn.session().to_string(),
            connected: true,
            records_sent,
            records_recv,
            datagrams_sent,
            datagrams_recv,
            bulk_parts_recv,
            dedup_dropped: stats.dedup_dropped,
        },
        tstats.udp_tx.bytes,
        tstats.udp_rx.bytes,
    ))
}
