//! An authenticated Spall connection: one reliable control stream, bounded bulk
//! streams, motion / input datagrams, an application heartbeat, and dedup gates.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use spall_protocol::{
    InputFrame, MotionSnapshot, Record, SessionId, WireTag, decode_datagram, encode_datagram,
};

use crate::config::TransportConfig;
use crate::dedup::{DedupVerdict, StreamDeduper};
use crate::framing::{read_framed, write_framed};
use crate::message::{NetMessage, WireRecord};
use crate::{Result, TransportError};

/// Which end of the connection this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// Live byte / record counters for one connection.
#[derive(Debug, Default)]
pub struct ConnStats {
    pub app_bytes_sent: AtomicU64,
    pub app_bytes_recv: AtomicU64,
    pub records_sent: AtomicU64,
    pub records_recv: AtomicU64,
    pub datagrams_sent: AtomicU64,
    pub datagrams_recv: AtomicU64,
    pub dedup_dropped: AtomicU64,
    pub heartbeats_sent: AtomicU64,
}

/// A plain snapshot of [`ConnStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnStatsSnapshot {
    pub app_bytes_sent: u64,
    pub app_bytes_recv: u64,
    pub records_sent: u64,
    pub records_recv: u64,
    pub datagrams_sent: u64,
    pub datagrams_recv: u64,
    pub dedup_dropped: u64,
    pub heartbeats_sent: u64,
}

impl ConnStats {
    fn snapshot(&self) -> ConnStatsSnapshot {
        ConnStatsSnapshot {
            app_bytes_sent: self.app_bytes_sent.load(Ordering::Relaxed),
            app_bytes_recv: self.app_bytes_recv.load(Ordering::Relaxed),
            records_sent: self.records_sent.load(Ordering::Relaxed),
            records_recv: self.records_recv.load(Ordering::Relaxed),
            datagrams_sent: self.datagrams_sent.load(Ordering::Relaxed),
            datagrams_recv: self.datagrams_recv.load(Ordering::Relaxed),
            dedup_dropped: self.dedup_dropped.load(Ordering::Relaxed),
            heartbeats_sent: self.heartbeats_sent.load(Ordering::Relaxed),
        }
    }
}

/// A decoded datagram: exactly the two unreliable families.
#[derive(Debug, Clone, PartialEq)]
pub enum DatagramRecord {
    Input(InputFrame),
    Motion(MotionSnapshot),
}

struct CtrlRecv {
    stream: quinn::RecvStream,
    peer_heartbeat_seq: u64,
}

/// An authenticated connection. Cheap to wrap in an `Arc`; every method takes
/// `&self` so a heartbeat task and a receive loop can share one.
pub struct Connection {
    quic: quinn::Connection,
    cfg: TransportConfig,
    role: Role,
    session: SessionId,

    ctrl_send: Mutex<quinn::SendStream>,
    ctrl_recv: Mutex<CtrlRecv>,
    out_seq: AtomicU64,

    ctrl_dedup: Mutex<StreamDeduper>,
    dgram_dedup: Mutex<StreamDeduper>,

    last_seen: Mutex<Instant>,
    bulk_open: Arc<AtomicU32>,

    stats: Arc<ConnStats>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("role", &self.role)
            .field("session", &self.session)
            .field("peer", &self.quic.remote_address())
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl Connection {
    /// Wraps an already-authenticated QUIC connection and its control stream.
    /// Fails if the peer cannot carry datagrams (`docs/protocol.md`: "Fail the
    /// connection setup clearly if required datagrams are unavailable.").
    pub(crate) fn from_parts(
        quic: quinn::Connection,
        ctrl_send: quinn::SendStream,
        ctrl_recv: quinn::RecvStream,
        cfg: TransportConfig,
        session: SessionId,
        role: Role,
    ) -> Result<Self> {
        if quic.max_datagram_size().is_none() {
            return Err(TransportError::Connect(
                "peer does not support QUIC datagrams; motion/input channel unavailable".into(),
            ));
        }
        Ok(Self {
            quic,
            cfg,
            role,
            session,
            ctrl_send: Mutex::new(ctrl_send),
            ctrl_recv: Mutex::new(CtrlRecv {
                stream: ctrl_recv,
                peer_heartbeat_seq: 0,
            }),
            out_seq: AtomicU64::new(1),
            ctrl_dedup: Mutex::new(StreamDeduper::strict()),
            dgram_dedup: Mutex::new(StreamDeduper::with_window(256)),
            last_seen: Mutex::new(Instant::now()),
            bulk_open: Arc::new(AtomicU32::new(0)),
            stats: Arc::new(ConnStats::default()),
        })
    }

    /// The session id assigned by the server during authentication.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Server or client end.
    pub fn role(&self) -> Role {
        self.role
    }

    /// The peer's address.
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        self.quic.remote_address()
    }

    /// Application-level counters.
    pub fn stats(&self) -> ConnStatsSnapshot {
        self.stats.snapshot()
    }

    /// Quinn's own byte / loss counters for this connection.
    pub fn transport_stats(&self) -> quinn::ConnectionStats {
        self.quic.stats()
    }

    /// Shared counter handle, for a spawned pump that wants to record bytes.
    pub fn stats_handle(&self) -> Arc<ConnStats> {
        self.stats.clone()
    }

    // --- control stream ----------------------------------------------------

    /// Sends one reliable record on the control stream, returning its assigned
    /// per-stream sequence number.
    pub async fn send_record(&self, record: WireRecord) -> Result<u64> {
        let seq = self.out_seq.fetch_add(1, Ordering::Relaxed);
        let bytes = NetMessage::Record { seq, record }
            .encode(self.cfg.limits.max_control_record)
            .map_err(|e| {
                TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
            })?;
        let mut send = self.ctrl_send.lock().await;
        write_framed(&mut send, &bytes, self.cfg.limits.max_control_record).await?;
        self.stats.records_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .app_bytes_sent
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(seq)
    }

    /// Receives the next *fresh* reliable record. Heartbeats refresh liveness
    /// and are consumed internally; duplicates are dropped and counted;
    /// `Ok(None)` means the peer sent `Bye` or finished the stream.
    pub async fn recv_record(&self) -> Result<Option<WireRecord>> {
        loop {
            let raw = {
                let mut recv = self.ctrl_recv.lock().await;
                match read_framed(&mut recv.stream, self.cfg.limits.max_control_record).await? {
                    Some(bytes) => bytes,
                    None => return Ok(None),
                }
            };
            self.stats
                .app_bytes_recv
                .fetch_add(raw.len() as u64, Ordering::Relaxed);
            let msg =
                NetMessage::decode(&raw, self.cfg.limits.max_control_record).map_err(|e| {
                    TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
                })?;
            self.touch().await;
            match msg {
                NetMessage::Heartbeat { seq } => {
                    self.ctrl_recv.lock().await.peer_heartbeat_seq = seq;
                }
                NetMessage::Bye { .. } => return Ok(None),
                NetMessage::Record { seq, record } => {
                    match self.ctrl_dedup.lock().await.admit(seq) {
                        DedupVerdict::Accept { .. } => {
                            self.stats.records_recv.fetch_add(1, Ordering::Relaxed);
                            return Ok(Some(record));
                        }
                        DedupVerdict::Duplicate => {
                            self.stats.dedup_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }

    /// Sends a `Bye` then finishes the control send stream.
    pub async fn say_bye(&self, reason: &str) -> Result<()> {
        let bytes = NetMessage::Bye {
            reason: reason.to_string(),
        }
        .encode(self.cfg.limits.max_control_record)
        .map_err(|e| TransportError::Frame(crate::framing::FrameError::Stream(e.to_string())))?;
        let mut send = self.ctrl_send.lock().await;
        write_framed(&mut send, &bytes, self.cfg.limits.max_control_record).await?;
        let _ = send.finish();
        Ok(())
    }

    // --- datagrams -------------------------------------------------------------

    /// Sends one datagram record (`InputFrame` or `MotionSnapshot`). The framed
    /// payload plus an 8-byte sequence envelope must fit the datagram cap.
    pub async fn send_datagram<R: Record>(&self, seq: u64, record: &R) -> Result<()> {
        if !matches!(R::TAG, WireTag::InputFrame | WireTag::MotionSnapshot) {
            return Err(TransportError::Connect(format!(
                "record {:?} is not a datagram family",
                R::TAG
            )));
        }
        let framed = encode_datagram(record).map_err(|e| {
            TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
        })?;
        let mut buf = Vec::with_capacity(8 + framed.len());
        buf.extend_from_slice(&seq.to_le_bytes());
        buf.extend_from_slice(&framed);

        let cap = self
            .quic
            .max_datagram_size()
            .unwrap_or(0)
            .min(self.cfg.limits.max_datagram_payload);
        if buf.len() > cap {
            return Err(TransportError::Frame(
                crate::framing::FrameError::WriteOversize {
                    actual: buf.len(),
                    limit: cap,
                },
            ));
        }
        self.quic
            .send_datagram(buf.clone().into())
            .map_err(|e| TransportError::Connect(format!("send_datagram: {e}")))?;
        self.stats.datagrams_sent.fetch_add(1, Ordering::Relaxed);
        self.stats
            .app_bytes_sent
            .fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    /// Receives the next *fresh* datagram record. Late-but-new datagrams are
    /// accepted (motion may arrive out of order); exact replays are dropped.
    /// `Ok(None)` on connection close.
    pub async fn recv_datagram(&self) -> Result<Option<DatagramRecord>> {
        loop {
            let bytes = match self.quic.read_datagram().await {
                Ok(b) => b,
                Err(quinn::ConnectionError::LocallyClosed)
                | Err(quinn::ConnectionError::ApplicationClosed(_)) => return Ok(None),
                Err(e) => return Err(TransportError::ConnectionLost(e.to_string())),
            };
            self.stats
                .app_bytes_recv
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            if bytes.len() < 8 {
                // Too short to carry our envelope; ignore rather than trust it.
                continue;
            }
            let seq = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let framed = &bytes[8..];
            let record = decode_datagram_record(framed)?;
            self.touch().await;
            match self.dgram_dedup.lock().await.admit(seq) {
                DedupVerdict::Accept { .. } => {
                    self.stats.datagrams_recv.fetch_add(1, Ordering::Relaxed);
                    return Ok(Some(record));
                }
                DedupVerdict::Duplicate => {
                    self.stats.dedup_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // --- bulk streams -------------------------------------------------------

    /// Opens a new outbound bulk stream, refusing to exceed
    /// [`TransportConfig::limits`]`.max_bulk_streams`.
    pub async fn open_bulk(&self) -> Result<BulkSend> {
        let guard = self.reserve_bulk()?;
        let (send, _recv) = self
            .quic
            .open_bi()
            .await
            .map_err(|e| TransportError::ConnectionLost(e.to_string()))?;
        Ok(BulkSend {
            stream: send,
            cap: self.cfg.limits.max_bulk_part,
            _open: guard,
        })
    }

    /// Opens a raw bidirectional stream with no framing wrapper. For callers
    /// that drive their own framed reads (baseline plumbing in later tasks, and
    /// the malformed-input tests here).
    pub async fn open_bi_raw(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.quic
            .open_bi()
            .await
            .map_err(|e| TransportError::ConnectionLost(e.to_string()))
    }

    /// Accepts a raw bidirectional stream with no framing wrapper.
    pub async fn accept_bi_raw(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        self.quic
            .accept_bi()
            .await
            .map_err(|e| TransportError::ConnectionLost(e.to_string()))
    }

    /// Accepts the next inbound bulk stream, applying the same ceiling.
    pub async fn accept_bulk(&self) -> Result<BulkRecv> {
        let (_send, recv) = self
            .quic
            .accept_bi()
            .await
            .map_err(|e| TransportError::ConnectionLost(e.to_string()))?;
        let guard = self.reserve_bulk()?;
        Ok(BulkRecv {
            stream: recv,
            cap: self.cfg.limits.max_bulk_part,
            assembled_cap: self.cfg.limits.max_assembled_transfer,
            _open: guard,
        })
    }

    fn reserve_bulk(&self) -> Result<BulkGuard> {
        let max = self.cfg.limits.max_bulk_streams;
        let prev = self.bulk_open.fetch_add(1, Ordering::SeqCst);
        if prev >= max {
            self.bulk_open.fetch_sub(1, Ordering::SeqCst);
            return Err(TransportError::Frame(
                crate::framing::FrameError::WriteOversize {
                    actual: (prev + 1) as usize,
                    limit: max as usize,
                },
            ));
        }
        Ok(BulkGuard(self.bulk_open.clone()))
    }

    // --- liveness ----------------------------------------------------------

    /// Runs the heartbeat + idle watchdog until the connection dies or `stop`
    /// resolves. Spawn this once per connection.
    pub async fn run_liveness(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        let mut beat = tokio::time::interval(self.cfg.heartbeat_interval);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = beat.tick() => {
                    let seq = self.out_seq.load(Ordering::Relaxed);
                    let beat_msg = NetMessage::Heartbeat { seq };
                    let bytes = match beat_msg.encode(self.cfg.limits.max_control_record) {
                        Ok(b) => b,
                        Err(_) => break,
                    };
                    let sent = {
                        let mut s = self.ctrl_send.lock().await;
                        write_framed(&mut s, &bytes, self.cfg.limits.max_control_record).await
                    };
                    if sent.is_err() {
                        break;
                    }
                    self.stats.heartbeats_sent.fetch_add(1, Ordering::Relaxed);
                    if self.since_last_seen().await > self.cfg.idle_timeout {
                        self.quic.close(1u32.into(), b"idle timeout");
                        break;
                    }
                }
                _ = stop.changed() => {
                    if *stop.borrow() {
                        break;
                    }
                }
            }
        }
    }

    /// Time since the last inbound control traffic of any kind.
    pub async fn since_last_seen(&self) -> Duration {
        self.last_seen.lock().await.elapsed()
    }

    async fn touch(&self) {
        *self.last_seen.lock().await = Instant::now();
    }

    /// True until the QUIC connection has closed.
    pub fn is_alive(&self) -> bool {
        self.quic.close_reason().is_none()
    }

    /// Closes the connection with an application code.
    pub fn close(&self, reason: &str) {
        self.quic.close(0u32.into(), reason.as_bytes());
    }

    /// Awaits full connection teardown (both peers acknowledged close).
    pub async fn closed(&self) {
        let _ = self.quic.closed().await;
    }
}

fn decode_datagram_record(framed: &[u8]) -> Result<DatagramRecord> {
    if framed.len() < spall_protocol::codec::HEADER_LEN {
        return Err(TransportError::Frame(
            crate::framing::FrameError::Incomplete {
                got: framed.len(),
                want: spall_protocol::codec::HEADER_LEN,
            },
        ));
    }
    let tag = u16::from_le_bytes([framed[2], framed[3]]);
    let map_err = |e: spall_protocol::CodecError| {
        TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
    };
    match WireTag::from_u16(tag) {
        Some(WireTag::InputFrame) => Ok(DatagramRecord::Input(
            decode_datagram(framed).map_err(map_err)?,
        )),
        Some(WireTag::MotionSnapshot) => Ok(DatagramRecord::Motion(
            decode_datagram(framed).map_err(map_err)?,
        )),
        _ => Err(TransportError::Connect(format!(
            "datagram carried non-datagram tag {tag}"
        ))),
    }
}

/// Guard that decrements the connection's live-bulk-stream counter on drop.
struct BulkGuard(Arc<AtomicU32>);

impl Drop for BulkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An outbound bulk stream for baseline parts.
pub struct BulkSend {
    stream: quinn::SendStream,
    cap: usize,
    _open: BulkGuard,
}

impl BulkSend {
    /// Writes one framed record (expected: `BaselinePart`), bounded by
    /// `max_bulk_part`.
    pub async fn send_part(&mut self, record: &spall_protocol::BaselinePart) -> Result<()> {
        let framed = spall_protocol::encode_control(record).map_err(|e| {
            TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
        })?;
        write_framed(&mut self.stream, &framed, self.cap).await?;
        Ok(())
    }

    /// Marks the stream finished (sends FIN). Buffered parts are still flushed
    /// by the connection; this does not wait for the peer to drain them.
    pub fn finish(mut self) -> Result<()> {
        self.stream
            .finish()
            .map_err(|e| TransportError::ConnectionLost(e.to_string()))
    }
}

/// An inbound bulk stream, with an assembled-size ceiling.
pub struct BulkRecv {
    stream: quinn::RecvStream,
    cap: usize,
    assembled_cap: usize,
    _open: BulkGuard,
}

impl BulkRecv {
    /// Reads framed parts until the stream ends, enforcing both the per-part
    /// and the assembled-transfer limits. A transfer that would exceed
    /// `max_assembled_transfer` is refused without buffering the overflow.
    pub async fn collect_parts(mut self) -> Result<Vec<spall_protocol::BaselinePart>> {
        let mut parts = Vec::new();
        let mut assembled = 0usize;
        let mut transfer = None;
        let mut next_part_index = 0u32;
        while let Some(bytes) = read_framed(&mut self.stream, self.cap).await? {
            if parts.len() >= spall_protocol::limits::MAX_BASELINE_PARTS {
                return Err(TransportError::Frame(
                    crate::framing::FrameError::BulkPartCount {
                        limit: spall_protocol::limits::MAX_BASELINE_PARTS,
                    },
                ));
            }
            assembled = assembled.saturating_add(bytes.len());
            if assembled > self.assembled_cap {
                return Err(TransportError::Frame(
                    crate::framing::FrameError::Oversize {
                        declared: assembled,
                        limit: self.assembled_cap,
                    },
                ));
            }
            let part: spall_protocol::BaselinePart = spall_protocol::decode_control(&bytes)
                .map_err(|e| {
                    TransportError::Frame(crate::framing::FrameError::Stream(e.to_string()))
                })?;
            if let Some(expected) = transfer {
                if part.transfer_id != expected {
                    return Err(TransportError::Frame(
                        crate::framing::FrameError::TransferMismatch,
                    ));
                }
            } else {
                transfer = Some(part.transfer_id);
            }
            if part.part_index != next_part_index {
                return Err(TransportError::Frame(
                    crate::framing::FrameError::PartOrder {
                        expected: next_part_index,
                        found: part.part_index,
                    },
                ));
            }
            if part.part_hash != spall_protocol::Hash32::of(&part.payload) {
                return Err(TransportError::Frame(
                    crate::framing::FrameError::PartHashMismatch {
                        index: part.part_index,
                    },
                ));
            }
            next_part_index = next_part_index.checked_add(1).ok_or_else(|| {
                TransportError::Frame(crate::framing::FrameError::BulkPartCount {
                    limit: spall_protocol::limits::MAX_BASELINE_PARTS,
                })
            })?;
            parts.push(part);
        }
        Ok(parts)
    }
}
