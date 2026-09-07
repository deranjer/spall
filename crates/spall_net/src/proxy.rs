//! An opaque UDP relay that impairs encrypted transport packets.
//!
//! `docs/tasks.md` (T09): "a separate UDP proxy that drops / delays encrypted
//! transport packets without decoding them ... application faults do not
//! reproduce QUIC retransmission / congestion behavior."
//!
//! The proxy never parses a QUIC header. It moves datagrams between a client
//! and the real server, and per datagram it may drop it, delay it, or (with
//! jitter) let it overtake another. Loss decisions are driven by a seeded PRNG
//! keyed per direction, so a failing run is reproducible from `(seed, plan)`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::fault::SplitMix64;

/// Per-datagram impairment applied in both directions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacketFaultPlan {
    /// PRNG seed. A run is fully reproducible from this plus the plan.
    pub seed: u64,
    /// Probability a datagram is dropped (`0.0..=1.0`).
    pub loss_ratio: f64,
    /// Probability a forwarded datagram is also sent a second time.
    pub duplicate_ratio: f64,
    /// Fixed forwarding delay added to every datagram.
    pub delay: Duration,
    /// Extra uniformly-random delay in `0..jitter` added when non-zero; this is
    /// what lets datagrams reorder.
    pub jitter: Duration,
}

impl PacketFaultPlan {
    /// A transparent relay: no loss, no delay.
    pub fn transparent(seed: u64) -> Self {
        Self {
            seed,
            loss_ratio: 0.0,
            duplicate_ratio: 0.0,
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
        }
    }

    /// The impaired-connection profile from `docs/validation.md` G4
    /// (100 ms RTT, ~2% loss).
    pub fn impaired(seed: u64) -> Self {
        Self {
            seed,
            loss_ratio: 0.02,
            duplicate_ratio: 0.0,
            delay: Duration::from_millis(50),
            jitter: Duration::from_millis(20),
        }
    }
}

/// Counters for one running proxy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketStats {
    pub c2s_forwarded: u64,
    pub c2s_dropped: u64,
    pub s2c_forwarded: u64,
    pub s2c_dropped: u64,
}

#[derive(Default)]
struct Counters {
    c2s_forwarded: AtomicU64,
    c2s_dropped: AtomicU64,
    s2c_forwarded: AtomicU64,
    s2c_dropped: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> PacketStats {
        PacketStats {
            c2s_forwarded: self.c2s_forwarded.load(Ordering::Relaxed),
            c2s_dropped: self.c2s_dropped.load(Ordering::Relaxed),
            s2c_forwarded: self.s2c_forwarded.load(Ordering::Relaxed),
            s2c_dropped: self.s2c_dropped.load(Ordering::Relaxed),
        }
    }
}

/// A running UDP proxy. Clients connect to [`Self::local_addr`]; traffic is
/// relayed to the server address given at construction.
pub struct UdpProxy {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

const MAX_DATAGRAM: usize = 2048;

impl UdpProxy {
    /// Binds a loopback socket and starts relaying to `upstream`.
    pub async fn spawn(upstream: SocketAddr, plan: PacketFaultPlan) -> std::io::Result<Arc<Self>> {
        let listen = UdpSocket::bind("127.0.0.1:0").await?;
        let local_addr = listen.local_addr()?;
        let listen = Arc::new(listen);
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());

        let proxy = Arc::new(Self {
            local_addr,
            stop: stop.clone(),
            counters: counters.clone(),
            tasks: Mutex::new(Vec::new()),
        });

        let handle = tokio::spawn(relay_loop(listen, upstream, plan, stop, counters));
        proxy.tasks.lock().await.push(handle);
        Ok(proxy)
    }

    /// The address clients should connect to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Snapshot of forwarded / dropped counts.
    pub fn stats(&self) -> PacketStats {
        self.counters.snapshot()
    }

    /// Stops the proxy and waits for its tasks to finish. Idempotent. Tasks
    /// poll `stop` on a sub-second timeout, so a clean exit gets a short grace
    /// period before the handle is aborted.
    pub async fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut tasks = self.tasks.lock().await;
        for mut task in tasks.drain(..) {
            if tokio::time::timeout(Duration::from_millis(500), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for UdpProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(tasks) = self.tasks.try_lock() {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}

async fn relay_loop(
    listen: Arc<UdpSocket>,
    upstream_addr: SocketAddr,
    plan: PacketFaultPlan,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    // One upstream socket, and the most recent client address. QUIC on
    // loopback keeps a stable 4-tuple per connection; a second client gets its
    // own proxy in the harness.
    let upstream = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::warn!("proxy upstream bind failed: {e}");
            return;
        }
    };
    if upstream.connect(upstream_addr).await.is_err() {
        return;
    }

    let client_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let mut c2s_rng = SplitMix64::new(plan.seed ^ 0xC2C2_C2C2);
    let s2c_rng = Arc::new(Mutex::new(SplitMix64::new(plan.seed ^ 0x5252_5252)));

    // Server -> client pump.
    let s2c = {
        let listen = listen.clone();
        let upstream = upstream.clone();
        let client_addr = client_addr.clone();
        let counters = counters.clone();
        let stop = stop.clone();
        let s2c_rng = s2c_rng.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            while !stop.load(Ordering::SeqCst) {
                let n =
                    match tokio::time::timeout(Duration::from_millis(200), upstream.recv(&mut buf))
                        .await
                    {
                        Ok(Ok(n)) => n,
                        Ok(Err(_)) => break,
                        Err(_) => continue,
                    };
                let Some(dst) = *client_addr.lock().await else {
                    continue;
                };
                let drop = s2c_rng.lock().await.next_f64() < plan.loss_ratio;
                if drop {
                    counters.s2c_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let dup = plan.duplicate_ratio > 0.0
                    && s2c_rng.lock().await.next_f64() < plan.duplicate_ratio;
                let packet = buf[..n].to_vec();
                counters.s2c_forwarded.fetch_add(1, Ordering::Relaxed);
                spawn_send(
                    listen.clone(),
                    packet.clone(),
                    dst,
                    sample_delay(&plan, &s2c_rng).await,
                );
                if dup {
                    spawn_send(
                        listen.clone(),
                        packet,
                        dst,
                        sample_delay(&plan, &s2c_rng).await,
                    );
                }
            }
        })
    };

    // Client -> server pump.
    let mut buf = vec![0u8; MAX_DATAGRAM];
    while !stop.load(Ordering::SeqCst) {
        let (n, from) = match tokio::time::timeout(
            Duration::from_millis(200),
            listen.recv_from(&mut buf),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        *client_addr.lock().await = Some(from);
        if c2s_rng.next_f64() < plan.loss_ratio {
            counters.c2s_dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let dup = plan.duplicate_ratio > 0.0 && c2s_rng.next_f64() < plan.duplicate_ratio;
        let delay = plan.delay + jitter(&mut c2s_rng, plan.jitter);
        let packet = buf[..n].to_vec();
        counters.c2s_forwarded.fetch_add(1, Ordering::Relaxed);
        spawn_send_connected(upstream.clone(), packet.clone(), delay);
        if dup {
            let d = plan.delay + jitter(&mut c2s_rng, plan.jitter);
            spawn_send_connected(upstream.clone(), packet, d);
        }
    }

    s2c.abort();
    let _ = s2c.await;
}

async fn sample_delay(plan: &PacketFaultPlan, rng: &Arc<Mutex<SplitMix64>>) -> Duration {
    let mut guard = rng.lock().await;
    plan.delay + jitter(&mut guard, plan.jitter)
}

fn jitter(rng: &mut SplitMix64, max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let micros = rng.next_u64() % (max.as_micros() as u64 + 1);
    Duration::from_micros(micros)
}

fn spawn_send(sock: Arc<UdpSocket>, packet: Vec<u8>, dst: SocketAddr, delay: Duration) {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let _ = sock.send_to(&packet, dst).await;
    });
}

fn spawn_send_connected(sock: Arc<UdpSocket>, packet: Vec<u8>, delay: Duration) {
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let _ = sock.send(&packet).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transparent_proxy_forwards_datagrams_both_ways() {
        // Stand-in "server": an echo socket.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            for _ in 0..4 {
                if let Ok((n, from)) = server.recv_from(&mut buf).await {
                    let _ = server.send_to(&buf[..n], from).await;
                }
            }
        });

        let proxy = UdpProxy::spawn(server_addr, PacketFaultPlan::transparent(1))
            .await
            .unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(proxy.local_addr()).await.unwrap();

        client.send(b"ping").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf))
            .await
            .expect("echo did not return")
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert!(proxy.stats().c2s_forwarded >= 1);

        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn lossy_proxy_drops_a_reproducible_subset() {
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink.local_addr().unwrap();

        let plan = PacketFaultPlan {
            seed: 0xFEED,
            loss_ratio: 0.5,
            duplicate_ratio: 0.0,
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
        };

        let run = || async {
            let proxy = UdpProxy::spawn(sink_addr, plan).await.unwrap();
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            client.connect(proxy.local_addr()).await.unwrap();
            for i in 0u8..40 {
                client.send(&[i]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let stats = proxy.stats();
            proxy.shutdown().await;
            stats
        };

        let a = run().await;
        let b = run().await;
        assert_eq!(a.c2s_forwarded, b.c2s_forwarded);
        assert_eq!(a.c2s_dropped, b.c2s_dropped);
        assert!(a.c2s_dropped > 0 && a.c2s_forwarded > 0);
        assert_eq!(a.c2s_forwarded + a.c2s_dropped, 40);
    }
}
