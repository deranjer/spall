//! Transport tunables: stream ceilings, frame sizes, and liveness timers.
//!
//! Byte and count limits default to `docs/protocol.md`. Every value that bounds
//! an allocation or a wait is here so a test can shrink it without touching the
//! protocol crate.

use std::time::Duration;

use spall_protocol::limits;

/// Byte / count ceilings applied to one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportLimits {
    /// Largest reliable control-stream record, header included.
    pub max_control_record: usize,
    /// Largest single bulk-stream part payload.
    pub max_bulk_part: usize,
    /// Largest fully assembled baseline transfer.
    pub max_assembled_transfer: usize,
    /// Largest datagram payload after our envelope.
    pub max_datagram_payload: usize,
    /// Concurrent bulk streams a connection may carry
    /// (`docs/protocol.md`: "Limit concurrent bulk streams to four initially").
    pub max_bulk_streams: u32,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_control_record: limits::MAX_CONTROL_RECORD,
            max_bulk_part: limits::MAX_BULK_PART,
            max_assembled_transfer: limits::MAX_ASSEMBLED_TRANSFER,
            max_datagram_payload: limits::MAX_DATAGRAM_PAYLOAD,
            max_bulk_streams: 4,
        }
    }
}

/// Full transport configuration shared by [`crate::conn`] and the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportConfig {
    pub limits: TransportLimits,
    /// How long to wait for the authentication exchange before giving up.
    pub handshake_timeout: Duration,
    /// Interval between application-level [`crate::message::NetMessage::Heartbeat`]
    /// records on the control stream.
    pub heartbeat_interval: Duration,
    /// If no control traffic (of any kind) is seen for this long, the
    /// connection is considered dead. Also configured as the QUIC idle timeout.
    pub idle_timeout: Duration,
    /// QUIC keep-alive PING interval. Kept below `idle_timeout`.
    pub keep_alive_interval: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            limits: TransportLimits::default(),
            handshake_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(500),
            idle_timeout: Duration::from_secs(10),
            keep_alive_interval: Duration::from_secs(2),
        }
    }
}

impl TransportConfig {
    /// A configuration with tighter timers, for CPU-bound CI tests.
    pub fn for_tests() -> Self {
        Self {
            limits: TransportLimits::default(),
            handshake_timeout: Duration::from_secs(5),
            heartbeat_interval: Duration::from_millis(100),
            idle_timeout: Duration::from_secs(6),
            keep_alive_interval: Duration::from_millis(500),
        }
    }
}
