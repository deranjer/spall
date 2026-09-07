//! Real OS-process acceptance: server, two clients, and two opaque UDP proxies.
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use spall_net::harness::{HARNESS_MANIFEST_TAG, TransportCheckParams, demo_handshake};
use spall_net::tls::{DevIdentity, Fingerprint, JoinToken};
use spall_net::{NetServer, PacketFaultPlan, UdpProxy};

#[derive(Serialize, Deserialize)]
struct Ready {
    pid: u32,
    addr: std::net::SocketAddr,
    fingerprint: [u8; 32],
    token: JoinToken,
}

fn write_ready(path: &Path, ready: &Ready) {
    let pending = path.with_extension("pending");
    std::fs::write(&pending, serde_json::to_vec(ready).unwrap()).unwrap();
    std::fs::rename(pending, path).unwrap();
}

/// Invoked only by the parent test with isolated per-run paths. Credentials are
/// exchanged through local temporary files and never printed or committed.
#[test]
#[ignore = "child process entry point; exercised by separate_process_transport"]
fn process_role() {
    let Ok(role) = std::env::var("SPALL_TEST_ROLE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("SPALL_TEST_ROOT").unwrap());
    let index = std::env::var("SPALL_TEST_INDEX").unwrap_or_default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let params = TransportCheckParams::default();
        match role.as_str() {
            "server" => {
                let identity = DevIdentity::generate().unwrap();
                let token = JoinToken::generate().unwrap();
                let server = NetServer::bind(
                    "127.0.0.1:0".parse().unwrap(),
                    &identity,
                    token,
                    demo_handshake(HARNESS_MANIFEST_TAG),
                    params.config,
                )
                .await
                .unwrap();
                write_ready(
                    &root.join("server.json"),
                    &Ready {
                        pid: std::process::id(),
                        addr: server.local_addr().unwrap(),
                        fingerprint: identity.fingerprint().0,
                        token,
                    },
                );
                let (stop, rx) = tokio::sync::watch::channel(false);
                let mut tasks = tokio::task::JoinSet::new();
                for _ in 0..2 {
                    let conn = server.accept().await.unwrap();
                    tasks.spawn(spall_net::harness::run_server_connection(
                        Arc::new(conn),
                        params.clone(),
                        rx.clone(),
                    ));
                }
                while let Some(done) = tasks.join_next().await {
                    done.unwrap();
                }
                let _ = stop.send(true);
                server.close();
                server.wait_idle().await;
            }
            "proxy" => {
                let upstream: Ready =
                    serde_json::from_slice(&std::fs::read(root.join("server.json")).unwrap())
                        .unwrap();
                let plan = PacketFaultPlan {
                    seed: 100 + index.parse::<u64>().unwrap(),
                    loss_ratio: 0.02,
                    delay: Duration::from_millis(15),
                    jitter: Duration::from_millis(5),
                    ..PacketFaultPlan::transparent(1)
                };
                let proxy = UdpProxy::spawn(upstream.addr, plan).await.unwrap();
                write_ready(
                    &root.join(format!("proxy{index}.json")),
                    &Ready {
                        pid: std::process::id(),
                        addr: proxy.local_addr(),
                        ..upstream
                    },
                );
                while !root.join("stop").exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                proxy.shutdown().await;
                let stats = proxy.stats();
                assert!(stats.c2s_forwarded > 0 && stats.s2c_forwarded > 0);
            }
            "client" => {
                let ready: Ready = serde_json::from_slice(
                    &std::fs::read(root.join(format!("proxy{index}.json"))).unwrap(),
                )
                .unwrap();
                let (_stop, rx) = tokio::sync::watch::channel(false);
                let (outcome, _, _) = spall_net::harness::run_client(
                    index.parse().unwrap(),
                    ready.addr,
                    Fingerprint(ready.fingerprint),
                    ready.token,
                    params,
                    rx,
                )
                .await
                .unwrap();
                assert!(outcome.connected);
                assert_eq!(outcome.records_recv, 8);
                assert_eq!(outcome.bulk_parts_recv, 3);
                assert!(outcome.datagrams_recv > 0);
                assert!(outcome.app_bytes_recv > outcome.records_recv);
            }
            _ => panic!("unknown child role"),
        }
    });
}

struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn spawn(root: &Path, role: &str, index: &str) -> Child {
    let log = std::fs::File::create(root.join(format!("{role}{index}.log"))).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "process_role", "--nocapture"])
        .env("SPALL_TEST_ROLE", role)
        .env("SPALL_TEST_ROOT", root)
        .env("SPALL_TEST_INDEX", index)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap()
}

fn wait_ready(path: &Path, child: &mut Child, deadline: Instant) {
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            let ready: Ready = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(ready.pid, child.id());
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before readiness: {}",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "child readiness deadline: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(child: &mut Child, deadline: Instant) {
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child {}: {status}", child.id());
            return;
        }
        assert!(Instant::now() < deadline, "child {} deadline", child.id());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn separate_process_transport() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("spall-net-process-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut children = Children(vec![spawn(&root, "server", "")]);
    wait_ready(&root.join("server.json"), &mut children.0[0], deadline);
    for index in 0..2 {
        children.0.push(spawn(&root, "proxy", &index.to_string()));
        wait_ready(
            &root.join(format!("proxy{index}.json")),
            children.0.last_mut().unwrap(),
            deadline,
        );
    }
    for index in 0..2 {
        children.0.push(spawn(&root, "client", &index.to_string()));
    }
    for client in &mut children.0[3..] {
        wait_exit(client, deadline);
    }
    wait_exit(&mut children.0[0], deadline);
    std::fs::write(root.join("stop"), b"stop").unwrap();
    for proxy in &mut children.0[1..3] {
        wait_exit(proxy, deadline);
    }
    drop(children);
    // Remove only files created by this run, with no recursive deletion.
    for entry in std::fs::read_dir(&root).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    std::fs::remove_dir(root).unwrap();
}
