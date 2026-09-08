//! T09 acceptance: authenticated multi-client exchange, clear auth failures,
//! bounded malformed input, and reliable-record recovery under real packet loss
//! without a shutdown deadlock.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use spall_net::conn::Connection;
use spall_net::endpoint::{NetServer, connect};
use spall_net::framing::{FrameError, read_framed};
use spall_net::harness::{
    HARNESS_MANIFEST_TAG, TransportCheckParams, demo_handshake, run_transport_check,
};
use spall_net::tls::{DevIdentity, JoinToken};
use spall_net::{AuthReject, PacketFaultPlan, TransportConfig, TransportError};

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// Bullet 1: server + two headless clients authenticate and exchange records
/// on every channel (reliable control, datagrams, and a bulk transfer).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_and_two_clients_authenticate_and_exchange_records() {
    let params = TransportCheckParams {
        clients: 2,
        records_per_client: 8,
        datagrams_per_client: 12,
        proxy: None,
        run_bulk_transfer: true,
        config: TransportConfig::for_tests(),
        overall_timeout: Duration::from_secs(20),
    };
    let report = run_transport_check(params.clone())
        .await
        .expect("transport check completed");

    assert_eq!(report.clients_connected, 2, "both clients authenticated");
    assert!(
        report.all_reliable_delivered(&params),
        "every reliable record and bulk part delivered: {report:?}"
    );
    assert_eq!(report.control_records_echoed, 16);
    assert_eq!(report.bulk_parts_delivered, 6);
    // Each client replays datagram seq 0 once; the server's dedup gate drops it.
    assert!(
        report.dedup_dropped >= 1,
        "datagram replay was deduplicated: {}",
        report.dedup_dropped
    );
    // Two distinct sessions were assigned.
    let sessions: std::collections::HashSet<_> =
        report.clients.iter().map(|c| c.session.clone()).collect();
    assert_eq!(sessions.len(), 2);
}

async fn spawn_server() -> (Arc<NetServer>, SocketAddr, DevIdentity, JoinToken) {
    let identity = DevIdentity::generate().unwrap();
    let token = JoinToken::generate().unwrap();
    let server = Arc::new(
        NetServer::bind(
            loopback(),
            &identity,
            token,
            demo_handshake(HARNESS_MANIFEST_TAG),
            TransportConfig::for_tests(),
        )
        .await
        .unwrap(),
    );
    let addr = server.local_addr().unwrap();
    (server, addr, identity, token)
}

/// Bullet 2: a wrong join token, a wrong content manifest, and a wrong pinned
/// certificate each fail with a distinct, actionable error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_token_manifest_or_certificate_fail_clearly() {
    let cfg = TransportConfig::for_tests();

    // --- wrong join token -------------------------------------------------
    {
        let (server, addr, identity, _token) = spawn_server().await;
        let s = server.clone();
        let accept = tokio::spawn(async move { s.accept().await });
        let err = connect(
            addr,
            identity.fingerprint(),
            JoinToken([0xAB; 32]),
            demo_handshake(HARNESS_MANIFEST_TAG),
            cfg,
        )
        .await
        .expect_err("bad token must fail");
        assert!(
            matches!(err, TransportError::Auth(AuthReject::BadToken)),
            "got {err:?}"
        );
        assert!(matches!(
            accept.await.unwrap(),
            Err(TransportError::Auth(AuthReject::BadToken))
        ));
        server.close();
    }

    // --- wrong content manifest ----------------------------------------
    {
        let (server, addr, identity, token) = spawn_server().await;
        let s = server.clone();
        let accept = tokio::spawn(async move { s.accept().await });
        let err = connect(
            addr,
            identity.fingerprint(),
            token,
            demo_handshake(b"a-different-content-manifest"),
            cfg,
        )
        .await
        .expect_err("manifest mismatch must fail");
        assert!(
            matches!(err, TransportError::Auth(AuthReject::Incompatible(ref m)) if m.contains("manifest")),
            "got {err:?}"
        );
        assert!(matches!(
            accept.await.unwrap(),
            Err(TransportError::Auth(AuthReject::Incompatible(_)))
        ));
        server.close();
    }

    // --- wrong pinned certificate ------------------------------------------
    {
        let (server, addr, _identity, token) = spawn_server().await;
        let s = server.clone();
        let accept = tokio::spawn(async move { s.accept().await });
        let impostor = DevIdentity::generate().unwrap();
        let err = connect(
            addr,
            impostor.fingerprint(),
            token,
            demo_handshake(HARNESS_MANIFEST_TAG),
            cfg,
        )
        .await
        .expect_err("cert pin mismatch must fail");
        assert!(
            matches!(err, TransportError::Connect(_)),
            "cert failure surfaces at the QUIC layer: {err:?}"
        );
        // The server side never produces an authenticated connection either.
        let server_side = tokio::time::timeout(Duration::from_secs(3), accept).await;
        assert!(
            server_side.is_err() || server_side.unwrap().unwrap().is_err(),
            "server did not accept the impostor"
        );
        server.close();
    }
}

/// Bullet 3: a hostile length prefix and an over-budget assembled transfer are
/// both refused within bounds — no huge allocation, connection stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_length_and_oversized_transfer_stay_bounded() {
    // Tight limits so the assembled-transfer ceiling is cheap to exceed.
    let mut cfg = TransportConfig::for_tests();
    cfg.limits.max_bulk_part = 4096;
    cfg.limits.max_assembled_transfer = 8192;

    let identity = DevIdentity::generate().unwrap();
    let token = JoinToken::generate().unwrap();
    let server = Arc::new(
        NetServer::bind(
            loopback(),
            &identity,
            token,
            demo_handshake(HARNESS_MANIFEST_TAG),
            cfg,
        )
        .await
        .unwrap(),
    );
    let addr = server.local_addr().unwrap();

    let s = server.clone();
    let server_conn: tokio::task::JoinHandle<Connection> =
        tokio::spawn(async move { s.accept().await.unwrap() });
    let client = connect(
        addr,
        identity.fingerprint(),
        token,
        demo_handshake(HARNESS_MANIFEST_TAG),
        cfg,
    )
    .await
    .unwrap();
    let server_conn = Arc::new(server_conn.await.unwrap());

    // (a) hostile length prefix: client writes a 3.5 GiB declared frame.
    {
        let mut raw = client.open_bi_raw().await.unwrap();
        raw.send_mut()
            .write_all(&0xD000_0000u32.to_le_bytes())
            .await
            .unwrap();
        raw.send_mut()
            .write_all(b"not that many bytes")
            .await
            .unwrap();
        let mut srv_raw = server_conn.accept_bi_raw().await.unwrap();
        let err = read_framed(srv_raw.recv_mut(), cfg.limits.max_bulk_part)
            .await
            .expect_err("declared length far above the cap is rejected");
        assert!(
            matches!(err, FrameError::Oversize { declared, limit } if declared == 0xD000_0000 && limit == 4096),
            "got {err:?}"
        );
    }

    // (b) over-budget assembled transfer: parts within per-part cap, sum over
    // the assembled cap.
    {
        let mut bulk = client.open_bulk().await.unwrap();
        for i in 0..4u32 {
            let part = spall_net::spall_protocol::BaselinePart {
                transfer_id: spall_net::spall_protocol::TransferId(1),
                part_index: i,
                payload: vec![7u8; 3000],
                part_hash: spall_net::spall_protocol::Hash32::of(&vec![7u8; 3000]),
            };
            // Some sends may fail once the peer resets the stream; that is fine.
            let _ = bulk.send_part(&part).await;
        }
        let _ = bulk.finish();

        let recv = server_conn.accept_bulk().await.unwrap();
        let err = recv
            .collect_parts()
            .await
            .expect_err("assembled transfer above the ceiling is refused");
        assert!(
            matches!(err, TransportError::Frame(FrameError::Oversize { limit, .. }) if limit == 8192),
            "got {err:?}"
        );
    }

    // The control stream still works after both hostile attempts.
    client
        .send_record(spall_net::WireRecord::DurableThrough(
            spall_net::spall_protocol::DurableThrough {
                journal_seq: spall_core::JournalSeq(3),
            },
        ))
        .await
        .expect("control stream survived the malformed input");
    let got = tokio::time::timeout(Duration::from_secs(3), server_conn.recv_record())
        .await
        .expect("server read did not hang")
        .expect("server read ok");
    assert!(matches!(
        got,
        Some(spall_net::WireRecord::DurableThrough(_))
    ));

    client.close("done");
    server.close();
}

/// Bulk streams reject inconsistent transfer metadata and the protocol's fixed
/// part-count ceiling before retaining an unbounded vector of tiny parts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_transfer_metadata_and_part_count_are_bounded() {
    let (server, addr, identity, token) = spawn_server().await;
    let s = server.clone();
    let server_conn = tokio::spawn(async move { s.accept().await.unwrap() });
    let client = connect(
        addr,
        identity.fingerprint(),
        token,
        demo_handshake(HARNESS_MANIFEST_TAG),
        TransportConfig::for_tests(),
    )
    .await
    .unwrap();
    let server_conn = server_conn.await.unwrap();

    // A skipped part index cannot be interpreted as a complete transfer.
    let mut inconsistent = client.open_bulk().await.unwrap();
    for index in [0, 2] {
        let payload = vec![index as u8 + 1];
        inconsistent
            .send_part(&spall_net::spall_protocol::BaselinePart {
                transfer_id: spall_net::spall_protocol::TransferId(7),
                part_index: index,
                part_hash: spall_net::spall_protocol::Hash32::of(&payload),
                payload,
            })
            .await
            .unwrap();
    }
    inconsistent.finish().unwrap();
    let err = server_conn
        .accept_bulk()
        .await
        .unwrap()
        .collect_parts()
        .await
        .expect_err("skipped bulk part index is rejected");
    assert!(matches!(
        err,
        TransportError::Frame(FrameError::PartOrder {
            expected: 1,
            found: 2
        })
    ));

    // The count ceiling is distinct from the assembled-byte ceiling: all of
    // these parts are one byte and remain far below 64 MiB.
    let mut many = client.open_bulk().await.unwrap();
    for index in 0..=spall_net::spall_protocol::limits::MAX_BASELINE_PARTS as u32 {
        let payload = vec![1];
        many.send_part(&spall_net::spall_protocol::BaselinePart {
            transfer_id: spall_net::spall_protocol::TransferId(8),
            part_index: index,
            part_hash: spall_net::spall_protocol::Hash32::of(&payload),
            payload,
        })
        .await
        .unwrap();
    }
    many.finish().unwrap();
    let err = server_conn
        .accept_bulk()
        .await
        .unwrap()
        .collect_parts()
        .await
        .expect_err("more than MAX_BASELINE_PARTS is rejected");
    assert!(matches!(
        err,
        TransportError::Frame(FrameError::BulkPartCount { limit })
            if limit == spall_net::spall_protocol::limits::MAX_BASELINE_PARTS
    ));

    client.close("done");
    server.close();
}

/// ENG-52: the outbound bulk-stream ceiling is one shared budget. Filling it
/// with any mix of `open_bulk` and the raw `open_bi_raw` bypass makes a fifth
/// stream fail through *either* entry point, and every handle returns its
/// permit the moment it is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_bulk_and_open_bi_raw_share_the_bulk_stream_cap() {
    let cfg = TransportConfig::for_tests();
    assert_eq!(
        cfg.limits.max_bulk_streams, 4,
        "test assumes the four-stream cap"
    );
    let (server, _accepted, client) = pair(cfg, cfg).await;

    let b1 = client.open_bulk().await.unwrap();
    let b2 = client.open_bulk().await.unwrap();
    let r1 = client.open_bi_raw().await.unwrap();
    let r2 = client.open_bi_raw().await.unwrap();
    assert_eq!(client.bulk_stream_count(), 4);

    // A fifth stream is refused whether the caller uses the framed or the raw
    // door, and the rejected reservation is handed straight back.
    assert!(
        matches!(
            client.open_bulk().await,
            Err(TransportError::Frame(FrameError::WriteOversize {
                actual: 5,
                limit: 4
            }))
        ),
        "fifth open_bulk refused"
    );
    assert!(
        matches!(
            client.open_bi_raw().await,
            Err(TransportError::Frame(FrameError::WriteOversize {
                actual: 5,
                limit: 4
            }))
        ),
        "fifth open_bi_raw refused"
    );
    assert_eq!(
        client.bulk_stream_count(),
        4,
        "refused opens leak no permit"
    );

    // Dropping a handle frees exactly one permit for the next open.
    drop(r2);
    assert_eq!(client.bulk_stream_count(), 3);
    let r3 = client.open_bi_raw().await.unwrap();
    assert_eq!(client.bulk_stream_count(), 4);

    drop((b1, b2, r1, r3));
    assert_eq!(
        client.bulk_stream_count(),
        0,
        "every handle released its permit"
    );

    client.close("done");
    server.close();
}

/// ENG-52 (inbound): `accept_bulk` and `accept_bi_raw` draw on the same shared
/// budget. With the cap already full, a fifth inbound stream is refused (and
/// its transport stream reset) through the given entry point, without leaking
/// the reservation; dropping the accepted handles frees every permit.
async fn fifth_inbound_bulk_stream_is_refused(reject_via_raw: bool) {
    let cfg = TransportConfig::for_tests();
    let (server, accepted, client) = pair(cfg, cfg).await;

    // The client opens five wire streams; the unguarded helper lets it exceed
    // its own local budget so all five reach the server.
    let mut wire = Vec::new();
    for _ in 0..5 {
        let (mut send, recv) = client.open_bi_unguarded().await.unwrap();
        send.write_all(b"x").await.unwrap();
        wire.push((send, recv));
    }

    // Fill the server's cap with a mix of both accept entry points.
    let mut framed = Vec::new();
    let mut raw = Vec::new();
    for i in 0..4 {
        if i % 2 == 0 {
            framed.push(accepted.accept_bulk().await.expect("under the cap"));
        } else {
            raw.push(accepted.accept_bi_raw().await.expect("under the cap"));
        }
    }
    assert_eq!(accepted.bulk_stream_count(), 4);

    let err = if reject_via_raw {
        accepted.accept_bi_raw().await.err()
    } else {
        accepted.accept_bulk().await.err()
    };
    assert!(
        matches!(
            err,
            Some(TransportError::Frame(FrameError::WriteOversize {
                actual: 5,
                limit: 4
            }))
        ),
        "fifth inbound stream refused, got {err:?}"
    );
    assert_eq!(
        accepted.bulk_stream_count(),
        4,
        "the refused accept released its reservation"
    );

    framed.clear();
    raw.clear();
    assert_eq!(
        accepted.bulk_stream_count(),
        0,
        "dropping every accepted handle frees every permit"
    );

    drop(wire);
    client.close("done");
    server.close();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_bulk_enforces_the_shared_bulk_stream_cap() {
    fifth_inbound_bulk_stream_is_refused(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accept_bi_raw_enforces_the_shared_bulk_stream_cap() {
    fifth_inbound_bulk_stream_is_refused(true).await;
}

/// Bullet 4: with a real lossy UDP proxy in front of the server, every reliable
/// record is still recovered by QUIC, and the whole run — including teardown —
/// finishes well inside a bounded deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn packet_loss_recovers_reliable_records_and_shutdown_never_deadlocks() {
    let params = TransportCheckParams {
        clients: 2,
        records_per_client: 10,
        datagrams_per_client: 20,
        proxy: Some(PacketFaultPlan {
            seed: 0x0D15EA5E,
            loss_ratio: 0.10,
            duplicate_ratio: 0.02,
            delay: Duration::from_millis(15),
            jitter: Duration::from_millis(10),
        }),
        run_bulk_transfer: true,
        config: TransportConfig::for_tests(),
        overall_timeout: Duration::from_secs(30),
    };

    // Belt-and-suspenders: the whole test cannot exceed this even on a bug.
    let report = tokio::time::timeout(Duration::from_secs(45), run_transport_check(params.clone()))
        .await
        .expect("run_transport_check returned before the outer deadline")
        .expect("transport check completed without an internal timeout");

    assert_eq!(report.clients_connected, 2);
    assert!(
        report.all_reliable_delivered(&params),
        "reliable records + bulk parts all recovered despite loss: {report:?}"
    );

    let dropped: u64 = report
        .proxy_stats
        .iter()
        .map(|s| s.c2s_dropped + s.s2c_dropped)
        .sum();
    assert!(
        dropped > 0,
        "the proxy actually dropped packets: {report:?}"
    );
    let forwarded: u64 = report
        .proxy_stats
        .iter()
        .map(|s| s.c2s_forwarded + s.s2c_forwarded)
        .sum();
    assert!(forwarded > dropped, "most packets still went through");
}

async fn pair(
    server_cfg: TransportConfig,
    client_cfg: TransportConfig,
) -> (Arc<NetServer>, Connection, Connection) {
    let identity = DevIdentity::generate().unwrap();
    let token = JoinToken::generate().unwrap();
    let server = Arc::new(
        NetServer::bind(
            loopback(),
            &identity,
            token,
            demo_handshake(HARNESS_MANIFEST_TAG),
            server_cfg,
        )
        .await
        .unwrap(),
    );
    let s = server.clone();
    let accepting = tokio::spawn(async move { s.accept().await.unwrap() });
    let client = connect(
        server.local_addr().unwrap(),
        identity.fingerprint(),
        token,
        demo_handshake(HARNESS_MANIFEST_TAG),
        client_cfg,
    )
    .await
    .unwrap();
    (server, accepting.await.unwrap(), client)
}

#[tokio::test]
async fn negotiated_limits_are_installed_on_both_ends() {
    let mut client_cfg = TransportConfig::for_tests();
    client_cfg.limits.max_control_record = 2048;
    client_cfg.limits.max_bulk_part = 8192;
    client_cfg.limits.max_assembled_transfer = 16384;
    client_cfg.limits.max_datagram_payload = 512;
    let (server, accepted, client) = pair(TransportConfig::for_tests(), client_cfg).await;
    assert_eq!(accepted.limits(), client_cfg.limits);
    assert_eq!(client.limits(), client_cfg.limits);
    server.close();
}

#[tokio::test]
async fn handshake_deadline_includes_quic_establishment() {
    let sink = tokio::net::UdpSocket::bind(loopback()).await.unwrap();
    let identity = DevIdentity::generate().unwrap();
    let mut cfg = TransportConfig::for_tests();
    cfg.handshake_timeout = Duration::from_millis(80);
    let start = std::time::Instant::now();
    let result = connect(
        sink.local_addr().unwrap(),
        identity.fingerprint(),
        JoinToken::generate().unwrap(),
        demo_handshake(HARNESS_MANIFEST_TAG),
        cfg,
    )
    .await;
    assert!(matches!(result, Err(TransportError::Timeout(_))));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn full_one_mib_bulk_payload_round_trips() {
    let cfg = TransportConfig::for_tests();
    let (server, accepted, client) = pair(cfg, cfg).await;
    let sender = tokio::spawn(async move {
        let payload = vec![42; spall_protocol::limits::MAX_BULK_PART];
        let part = spall_protocol::BaselinePart {
            transfer_id: spall_protocol::TransferId(1),
            part_index: 0,
            part_hash: spall_protocol::Hash32::of(&payload),
            payload,
        };
        let mut bulk = client.open_bulk().await.unwrap();
        bulk.send_part(&part).await.unwrap();
        bulk.finish().unwrap();
        client
    });
    let parts = tokio::time::timeout(Duration::from_secs(5), async {
        accepted
            .accept_bulk()
            .await
            .unwrap()
            .collect_parts()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        parts[0].payload.len(),
        spall_protocol::limits::MAX_BULK_PART
    );
    let client = sender.await.unwrap();
    client.close("done");
    server.close();
}

#[tokio::test]
async fn liveness_stops_when_its_owner_disappears() {
    let cfg = TransportConfig::for_tests();
    let (server, accepted, client) = pair(cfg, cfg).await;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(Arc::new(accepted).run_liveness(rx));
    drop(stop);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    client.close("done");
    server.close();
}

#[tokio::test]
async fn decoded_records_exercise_application_faults_independently_of_quic() {
    use spall_net::fault::{AppFaultPlan, FaultChannel};
    let cfg = TransportConfig::for_tests();
    let (server, accepted, client) = pair(cfg, cfg).await;
    let plan = AppFaultPlan {
        seed: 42,
        drop_ratio: 0.2,
        duplicate_ratio: 0.3,
        max_delay_ticks: 8,
        reorder: true,
    };
    let mut fault = FaultChannel::new(plan);
    let mut replay = FaultChannel::new(plan);
    for seq in 1..=64 {
        let record = spall_net::WireRecord::DurableThrough(spall_protocol::DurableThrough {
            journal_seq: spall_core::JournalSeq(seq),
        });
        client.send_record(record).await.unwrap();
        let decoded = accepted.recv_record().await.unwrap().unwrap();
        fault.push(decoded.clone());
        replay.push(decoded);
    }
    assert_eq!(fault.flush(), replay.flush());
    let stats = fault.stats();
    assert!(stats.dropped > 0 && stats.duplicated > 0 && stats.max_reorder_distance > 0);
    assert_eq!(
        stats.delivered,
        stats.pushed - stats.dropped + stats.duplicated
    );
    assert!(client.stats().app_bytes_sent > client.stats().records_sent);
    client.close("done");
    server.close();
}
