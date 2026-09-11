//! An opaque UDP relay that impairs encrypted transport packets.
//!
//! `docs/tasks.md` (T09): "a separate UDP proxy that drops / delays encrypted
//! transport packets without decoding them ... application faults do not
//! reproduce QUIC retransmission / congestion behavior."
//!
//! The proxy never parses a QUIC header. It moves datagrams between a client
//! and the real server, and per datagram it may drop it, delay it, or let it
//! overtake another — either by chance (`jitter`) or deterministically for a
//! fixed fraction of server->client datagrams (`reorder_period`). Loss
//! decisions are driven by a seeded PRNG keyed per direction, so a failing run
//! is reproducible from `(seed, plan)`.

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
    /// Deterministic reordering: every `reorder_period`-th **server->client**
    /// datagram is held an extra [`REORDER_HOLD`] so a following datagram
    /// overtakes it. `0` disables it — reordering then depends only on `jitter`.
    /// Unlike `jitter`, this guarantees a bounded number of out-of-order
    /// deliveries per run, so a test can assert reordering happened without
    /// depending on RNG luck. It is not applied client->server: holding
    /// `ActionRequest`s / ACKs there would only provoke retransmit bursts.
    pub reorder_period: u32,
    /// Sustained byte-rate ceiling applied independently in each direction
    /// (bytes/sec of forwarded UDP payload; header overhead excluded). `0`
    /// (the default from [`Self::transparent`] / [`Self::impaired`]) disables
    /// rate shaping — every existing caller is unchanged. Modelled as a
    /// serialization-delay pacer: a packet cannot depart before
    /// `max(propagation_due, previous_departure) + packet_bytes / rate`, so a
    /// bulk transfer through this proxy is throughput-capped the way a real
    /// bandwidth-limited link is, layered on top of the fixed `delay` /
    /// `jitter` above (T23 / G3 row 11 join-budget shaping).
    pub rate_limit_bytes_per_sec: u64,
}

/// Extra hold applied to a "reorder tick" datagram. Comfortably above the 50 ms
/// (20 Hz) motion-snapshot spacing so the next snapshot overtakes it.
pub const REORDER_HOLD: Duration = Duration::from_millis(90);

impl PacketFaultPlan {
    /// A transparent relay: no loss, no delay.
    pub fn transparent(seed: u64) -> Self {
        Self {
            seed,
            loss_ratio: 0.0,
            duplicate_ratio: 0.0,
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            reorder_period: 0,
            rate_limit_bytes_per_sec: 0,
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
            reorder_period: 0,
            rate_limit_bytes_per_sec: 0,
        }
    }

    /// The G3 join-budget network profile from `docs/validation.md`: a
    /// `rate_limit_bytes_per_sec`-capped link at 100 ms RTT / 2% loss
    /// (otherwise identical to [`Self::impaired`]).
    pub fn shaped(seed: u64, rate_limit_bytes_per_sec: u64) -> Self {
        Self {
            rate_limit_bytes_per_sec,
            ..Self::impaired(seed)
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

const MAX_DATAGRAM: usize = 65535;

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

// Both directions and every delayed packet belong to this one task. Dropping
// or aborting it drops the queue and sockets; no detached sends survive shutdown.
async fn relay_loop(
    listen: Arc<UdpSocket>,
    upstream_addr: SocketAddr,
    plan: PacketFaultPlan,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    const DEFAULT_MAX_PENDING_PACKETS: usize = 1024;
    const DEFAULT_MAX_PENDING_BYTES: usize = 2 * 1024 * 1024;
    // A rate-limited link needs enough buffering to smooth a bursty sender
    // down to the configured rate without the *queue itself* becoming the
    // dominant source of drops (which would silently inflate the intended
    // `loss_ratio`). Size it to ~5 seconds at the configured rate.
    let max_pending_bytes = if plan.rate_limit_bytes_per_sec > 0 {
        (plan.rate_limit_bytes_per_sec as usize)
            .saturating_mul(5)
            .max(DEFAULT_MAX_PENDING_BYTES)
    } else {
        DEFAULT_MAX_PENDING_BYTES
    };
    let max_pending_packets = if plan.rate_limit_bytes_per_sec > 0 {
        (max_pending_bytes / 1200).max(DEFAULT_MAX_PENDING_PACKETS)
    } else {
        DEFAULT_MAX_PENDING_PACKETS
    };
    let upstream = match UdpSocket::bind("127.0.0.1:0").await {
        Ok(s) => s,
        Err(_) => return,
    };
    if upstream.connect(upstream_addr).await.is_err() {
        return;
    }
    let mut client = None;
    let mut c2s_rng = SplitMix64::new(plan.seed ^ 0xC2C2_C2C2);
    let mut s2c_rng = SplitMix64::new(plan.seed ^ 0x5252_5252);
    let mut cbuf = vec![0; MAX_DATAGRAM];
    let mut sbuf = vec![0; MAX_DATAGRAM];
    // Instant and insertion order preserve deterministic ordering for equal deadlines.
    let mut queue = std::collections::BTreeMap::new();
    let mut queued_bytes = 0usize;
    let mut order = 0u64;
    // Per-direction count of datagrams accepted for forwarding, for the
    // deterministic `reorder_period` hold.
    let mut c2s_fwd = 0u64;
    let mut s2c_fwd = 0u64;
    // Per-direction leaky-bucket pacer state: the earliest instant the *next*
    // packet in that direction may depart, so aggregate throughput never
    // exceeds `plan.rate_limit_bytes_per_sec`. `None` until the first packet
    // sets a baseline (avoids depending on an arbitrary `Instant::now()` at
    // proxy start).
    let mut c2s_next_free: Option<tokio::time::Instant> = None;
    let mut s2c_next_free: Option<tokio::time::Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        let due = queue
            .first_key_value()
            .map(|((time, _), _)| *time)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_millis(20));
        let incoming = tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(20)) => { continue; }
            _ = tokio::time::sleep_until(due) => {
                if let Some((_, (toward_server, dst, packet))) = queue.pop_first() {
                    let packet: Vec<u8> = packet;
                    queued_bytes -= packet.len();
                    let sent = if toward_server { upstream.send(&packet).await }
                        else { listen.send_to(&packet, dst).await };
                    let counter = match (toward_server, sent.is_ok()) {
                        (true, true) => &counters.c2s_forwarded,
                        (true, false) => &counters.c2s_dropped,
                        (false, true) => &counters.s2c_forwarded,
                        (false, false) => &counters.s2c_dropped,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
            value = listen.recv_from(&mut cbuf) => {
                let Ok((n, from)) = value else { break; };
                // One client per relay: unrelated traffic cannot redirect replies.
                if client.is_some_and(|known| known != from) { continue; }
                client = Some(from);
                (true, upstream_addr, cbuf[..n].to_vec())
            }
            value = upstream.recv(&mut sbuf) => {
                let Ok(n) = value else { break; };
                let Some(dst) = client else { continue; };
                (false, dst, sbuf[..n].to_vec())
            }
        };
        let (toward_server, dst, packet) = incoming;
        let rng = if toward_server {
            &mut c2s_rng
        } else {
            &mut s2c_rng
        };
        let dropped = if toward_server {
            &counters.c2s_dropped
        } else {
            &counters.s2c_dropped
        };
        if rng.next_f64() < plan.loss_ratio {
            dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let copies = if rng.next_f64() < plan.duplicate_ratio {
            2
        } else {
            1
        };
        // Deterministic reorder: hold every Nth server->client datagram an extra
        // `REORDER_HOLD` so the following datagram is delivered first. Applied
        // only to s2c — that carries the motion snapshots whose reordering T11
        // exercises; holding client->server datagrams would just delay
        // `ActionRequest`s and ACKs and provoke retransmit bursts.
        let fwd_count = if toward_server {
            c2s_fwd += 1;
            c2s_fwd
        } else {
            s2c_fwd += 1;
            s2c_fwd
        };
        let reorder_hold = if !toward_server
            && plan.reorder_period > 0
            && fwd_count % u64::from(plan.reorder_period) == 0
        {
            REORDER_HOLD
        } else {
            Duration::ZERO
        };
        for _ in 0..copies {
            if queue.len() >= max_pending_packets || packet.len() > max_pending_bytes - queued_bytes
            {
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let delay = plan
                .delay
                .saturating_add(jitter(rng, plan.jitter))
                .saturating_add(reorder_hold);
            let Some(propagation_due) = tokio::time::Instant::now().checked_add(delay) else {
                dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            // Rate shaping: this direction's packets cannot depart faster than
            // `rate_limit_bytes_per_sec` in aggregate — a simple serialization
            // delay pacer layered on top of the fixed propagation `delay`.
            let due = if plan.rate_limit_bytes_per_sec > 0 {
                let next_free = if toward_server {
                    &mut c2s_next_free
                } else {
                    &mut s2c_next_free
                };
                let start = match *next_free {
                    Some(prev) => prev.max(propagation_due),
                    None => propagation_due,
                };
                let serialize = Duration::from_secs_f64(
                    packet.len() as f64 / plan.rate_limit_bytes_per_sec as f64,
                );
                *next_free = Some(start.checked_add(serialize).unwrap_or(start));
                start
            } else {
                propagation_due
            };
            queued_bytes += packet.len();
            queue.insert((due, order), (toward_server, dst, packet.clone()));
            order = order.wrapping_add(1);
        }
    }
}

fn jitter(rng: &mut SplitMix64, max: Duration) -> Duration {
    let max_micros = max.as_micros().min((u64::MAX - 1) as u128) as u64;
    Duration::from_micros(rng.next_u64() % (max_micros + 1))
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
            reorder_period: 0,
            rate_limit_bytes_per_sec: 0,
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

    #[tokio::test]
    async fn reorder_period_deterministically_delivers_datagrams_out_of_order() {
        // Echo sink: bounce every datagram straight back to the sender.
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, from)) = sink.recv_from(&mut buf).await else {
                    break;
                };
                let _ = sink.send_to(&buf[..n], from).await;
            }
        });

        let plan = PacketFaultPlan {
            seed: 1,
            loss_ratio: 0.0,
            duplicate_ratio: 0.0,
            delay: Duration::from_millis(2),
            jitter: Duration::ZERO,
            // Hold every 3rd datagram each way.
            reorder_period: 3,
            rate_limit_bytes_per_sec: 0,
        };
        let proxy = UdpProxy::spawn(sink_addr, plan).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(proxy.local_addr()).await.unwrap();

        for i in 0u8..12 {
            client.send(&[i]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut received = Vec::new();
        let mut buf = [0u8; 64];
        while received.len() < 12 {
            match tokio::time::timeout(Duration::from_millis(500), client.recv(&mut buf)).await {
                Ok(Ok(1)) => received.push(buf[0]),
                _ => break,
            }
        }
        proxy.shutdown().await;

        // Every held datagram (each way) shows up, but not in send order.
        assert_eq!(received.len(), 12, "no datagrams were lost");
        assert_ne!(
            received,
            (0u8..12).collect::<Vec<_>>(),
            "delivery was reordered"
        );
        let mut sorted = received.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0u8..12).collect::<Vec<_>>());
    }

    /// T23 / G3 row 11: a `rate_limit_bytes_per_sec` proxy paces a burst down to
    /// (approximately) the configured throughput instead of forwarding it at
    /// loopback speed — the join-budget scenario's bandwidth shaping depends on
    /// this to keep a 1 MiB/s-capped baseline transfer from completing in
    /// milliseconds.
    #[tokio::test]
    async fn rate_limited_proxy_paces_a_burst_to_the_configured_throughput() {
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sink_addr = sink.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                if sink.recv_from(&mut buf).await.is_err() {
                    break;
                }
            }
        });

        const RATE_BYTES_PER_SEC: u64 = 5_000;
        const PACKET_BYTES: usize = 1_000;
        const PACKET_COUNT: usize = 10; // 10,000 bytes / 5,000 B/s = 2.0 s minimum.
        let plan = PacketFaultPlan {
            rate_limit_bytes_per_sec: RATE_BYTES_PER_SEC,
            ..PacketFaultPlan::transparent(7)
        };
        let proxy = UdpProxy::spawn(sink_addr, plan).await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(proxy.local_addr()).await.unwrap();

        let started = std::time::Instant::now();
        let packet = vec![0xAB; PACKET_BYTES];
        for _ in 0..PACKET_COUNT {
            client.send(&packet).await.unwrap();
        }
        // Wait until every packet has been forwarded (or a generous deadline
        // passes), then measure how long the whole burst took to drain.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while proxy.stats().c2s_forwarded < PACKET_COUNT as u64
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let elapsed = started.elapsed();
        let stats = proxy.stats();
        proxy.shutdown().await;

        assert_eq!(
            stats.c2s_forwarded, PACKET_COUNT as u64,
            "every packet should eventually be forwarded, just paced"
        );
        let expected_min = Duration::from_secs_f64(
            (PACKET_BYTES * PACKET_COUNT) as f64 / RATE_BYTES_PER_SEC as f64,
        );
        assert!(
            elapsed >= expected_min.mul_f64(0.8),
            "burst drained in {elapsed:?}, expected at least ~{expected_min:?} at the \
             configured rate (pacing did not apply)"
        );
        assert!(
            elapsed <= expected_min + Duration::from_secs(3),
            "burst took {elapsed:?}, far longer than the ~{expected_min:?} the rate should allow"
        );
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_cancels_delayed_packets_and_releases_the_socket() {
        let sink = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let plan = PacketFaultPlan {
            delay: Duration::from_millis(250),
            ..PacketFaultPlan::transparent(9)
        };
        let proxy = UdpProxy::spawn(sink.local_addr().unwrap(), plan)
            .await
            .unwrap();
        let address = proxy.local_addr();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"must never arrive", address).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        proxy.shutdown().await;
        let _rebound = UdpSocket::bind(address).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), sink.recv(&mut [0; 64]))
                .await
                .is_err()
        );
        assert_eq!(proxy.stats().c2s_forwarded, 0);
    }
}
