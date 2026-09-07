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
    let token = JoinToken::generate();
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
    let token = JoinToken::generate();
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
        let (mut send, _r) = client.open_bi_raw().await.unwrap();
        send.write_all(&0xD000_0000u32.to_le_bytes()).await.unwrap();
        send.write_all(b"not that many bytes").await.unwrap();
        let (_s, mut recv) = server_conn.accept_bi_raw().await.unwrap();
        let err = read_framed(&mut recv, cfg.limits.max_bulk_part)
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
                part_hash: spall_net::spall_protocol::Hash32::of(b"p"),
                payload: vec![7u8; 3000],
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
