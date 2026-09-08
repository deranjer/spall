//! `spall_net` — Spall's QUIC transport adapter and fault harness.
//!
//! This crate is the only place Quinn/QUIC, rustls, and Tokio appear. It turns
//! the `spall_protocol` record families into authenticated connections:
//!
//! * **TLS-pinned dev sessions** ([`tls`]) — a self-signed development identity,
//!   server-certificate fingerprint pinning on the client, and a per-run join
//!   token. There is no global TLS-verification bypass.
//! * **Reliable control / bulk streams** ([`conn`]) — one ordered control stream
//!   per connection carrying length-framed [`spall_protocol`] records, plus a
//!   bounded set of bulk streams for baseline parts.
//! * **Motion / input datagrams** ([`conn`]) — unreliable, size-capped at
//!   [`spall_protocol::limits::MAX_DATAGRAM_PAYLOAD`].
//! * **Heartbeat and idle timeout** ([`config`], [`conn`]).
//! * **Deduplication plumbing** ([`dedup`]) — per-stream sequence gates so a
//!   replayed record cannot be applied twice.
//! * **Deterministic application-message fault injection** ([`fault`]) — seeded
//!   delay / drop / reorder of decoded messages, for tests that must be
//!   independent of QUIC's own loss recovery.
//! * **An opaque UDP proxy** ([`proxy`]) — drops / delays / reorders encrypted
//!   transport packets *without decoding them*, so tests exercise real QUIC
//!   retransmission and congestion behaviour.
//!
//! Nothing here runs simulation, storage, or rendering; `spall_server` and
//! `spall_client` compose this crate in T10.

pub mod config;
pub mod conn;
pub mod dedup;
pub mod endpoint;
pub mod fault;
pub mod framing;
pub mod harness;
pub mod message;
pub mod proxy;
pub mod tls;

use std::io;

pub use config::{TransportConfig, TransportLimits};
pub use conn::{
    BulkRecv, BulkSend, ConnStatsSnapshot, Connection, DatagramRecord, RawBulkStream, Role,
};
pub use dedup::{DedupVerdict, StreamDeduper};
pub use endpoint::{NetServer, connect};
pub use fault::{AppFaultPlan, FaultChannel, FaultStats};
pub use framing::{FrameError, read_framed, write_framed};
pub use harness::{ClientOutcome, TransportCheckParams, TransportCheckReport, run_transport_check};
pub use message::{AuthReject, ClientHello, NetMessage, ServerAccept, ServerAuthReply, WireRecord};
pub use proxy::{PacketFaultPlan, PacketStats, UdpProxy};
pub use tls::{DevIdentity, Fingerprint, JoinToken};

pub use spall_protocol;

/// The ALPN protocol identifier negotiated on every Spall QUIC connection.
pub const ALPN: &[u8] = b"spall/1";

/// Anything that can go wrong establishing or running a transport session.
///
/// Each variant is a distinct, actionable failure: a caller can tell a wrong
/// certificate (`Tls`) from a wrong token or manifest (`Auth`) from an
/// oversized frame (`Frame`).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Binding, connecting, or a socket-level I/O failure.
    #[error("transport i/o: {0}")]
    Io(#[from] io::Error),
    /// QUIC connection setup or teardown failure (includes a rejected /
    /// unpinned server certificate: the TLS handshake fails here).
    #[error("quic connection: {0}")]
    Connect(String),
    /// The peer closed or reset the connection.
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    /// A framed record violated a size or structural bound.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// Authentication was refused: wrong join token or an incompatible
    /// handshake (manifest / world / protocol mismatch).
    #[error("authentication rejected: {0}")]
    Auth(#[from] AuthReject),
    /// A rustls configuration was invalid.
    #[error("tls configuration: {0}")]
    Tls(String),
    /// A bounded operation (handshake, heartbeat, shutdown) exceeded its
    /// deadline.
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),
}

/// Convenience alias for this crate's fallible operations.
pub type Result<T> = std::result::Result<T, TransportError>;
