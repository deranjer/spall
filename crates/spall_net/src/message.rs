//! The control-stream message envelope and the development authentication
//! records.
//!
//! Reliable application records keep the T01 frame exactly
//! (`u16 schema | u16 tag | postcard body`, see [`spall_protocol::codec`]). The
//! transport adds only a small envelope around them:
//!
//! * [`NetMessage`] — a per-stream sequence number plus one of: a heartbeat, a
//!   framed application record, or a goodbye.
//! * [`ClientHello`] / [`ServerAuthReply`] — the join-token + handshake
//!   exchange that runs once at the head of the control stream. These are a
//!   development mechanism (`docs/protocol.md`), not part of the frozen record
//!   set, so they use a plain length-checked postcard body.

use serde::{Deserialize, Serialize};
use spall_protocol::{
    ActionRequest, ActionStatus, BaselineAck, BaselineBegin, BaselineEnd, BaselinePart, CodecError,
    DurableThrough, Handshake, RepairRequest, TopologyTransaction, WireTag, decode_control,
    encode_control,
};

use crate::message::private::Sealed;
use crate::tls::JoinToken;

/// One reliable application record, tagged by its [`WireTag`]. Datagram-only
/// families (`InputFrame`, `MotionSnapshot`) are deliberately absent.
#[derive(Debug, Clone, PartialEq)]
pub enum WireRecord {
    ActionRequest(ActionRequest),
    ActionStatus(ActionStatus),
    TopologyTransaction(TopologyTransaction),
    BaselineBegin(BaselineBegin),
    BaselinePart(BaselinePart),
    BaselineEnd(BaselineEnd),
    BaselineAck(BaselineAck),
    RepairRequest(RepairRequest),
    DurableThrough(DurableThrough),
    Handshake(Handshake),
}

impl WireRecord {
    /// The wire tag of the contained record.
    pub fn tag(&self) -> WireTag {
        match self {
            Self::ActionRequest(_) => WireTag::ActionRequest,
            Self::ActionStatus(_) => WireTag::ActionStatus,
            Self::TopologyTransaction(_) => WireTag::TopologyTransaction,
            Self::BaselineBegin(_) => WireTag::BaselineBegin,
            Self::BaselinePart(_) => WireTag::BaselinePart,
            Self::BaselineEnd(_) => WireTag::BaselineEnd,
            Self::BaselineAck(_) => WireTag::BaselineAck,
            Self::RepairRequest(_) => WireTag::RepairRequest,
            Self::DurableThrough(_) => WireTag::DurableThrough,
            Self::Handshake(_) => WireTag::Handshake,
        }
    }

    /// Encodes to the frozen `schema | tag | body` frame, running the record's
    /// own `validate()` first.
    pub fn encode_framed(&self) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::ActionRequest(r) => encode_control(r),
            Self::ActionStatus(r) => encode_control(r),
            Self::TopologyTransaction(r) => encode_control(r),
            Self::BaselineBegin(r) => encode_control(r),
            Self::BaselinePart(r) => encode_control(r),
            Self::BaselineEnd(r) => encode_control(r),
            Self::BaselineAck(r) => encode_control(r),
            Self::RepairRequest(r) => encode_control(r),
            Self::DurableThrough(r) => encode_control(r),
            Self::Handshake(r) => encode_control(r),
        }
    }

    /// Decodes a `schema | tag | body` frame, dispatching on the tag. The
    /// contained record's `validate()` runs inside [`decode_control`].
    pub fn decode_framed(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() < spall_protocol::codec::HEADER_LEN {
            return Err(CodecError::Truncated(bytes.len()));
        }
        let tag = u16::from_le_bytes([bytes[2], bytes[3]]);
        let tag = WireTag::from_u16(tag).ok_or(CodecError::TagMismatch {
            expected: WireTag::Handshake,
            expected_num: tag,
            found: tag,
        })?;
        Ok(match tag {
            WireTag::ActionRequest => Self::ActionRequest(decode_control(bytes)?),
            WireTag::ActionStatus => Self::ActionStatus(decode_control(bytes)?),
            WireTag::TopologyTransaction => Self::TopologyTransaction(decode_control(bytes)?),
            WireTag::BaselineBegin => Self::BaselineBegin(decode_control(bytes)?),
            WireTag::BaselinePart => Self::BaselinePart(decode_control(bytes)?),
            WireTag::BaselineEnd => Self::BaselineEnd(decode_control(bytes)?),
            WireTag::BaselineAck => Self::BaselineAck(decode_control(bytes)?),
            WireTag::RepairRequest => Self::RepairRequest(decode_control(bytes)?),
            WireTag::DurableThrough => Self::DurableThrough(decode_control(bytes)?),
            WireTag::Handshake => Self::Handshake(decode_control(bytes)?),
            WireTag::InputFrame | WireTag::MotionSnapshot => {
                return Err(CodecError::TagMismatch {
                    expected: WireTag::Handshake,
                    expected_num: tag.to_u16(),
                    found: tag.to_u16(),
                });
            }
        })
    }
}

/// A framing / decode failure for the control envelope.
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("control envelope is {got} bytes; need at least {need}")]
    Short { got: usize, need: usize },
    #[error("unknown control envelope kind {0}")]
    UnknownKind(u8),
    #[error("inner record frame declares {declared} bytes; cap is {cap}")]
    InnerOversize { declared: usize, cap: usize },
    #[error("goodbye reason is {got} bytes; cap is {cap}")]
    ReasonOversize { got: usize, cap: usize },
    #[error("inner record: {0}")]
    Record(#[from] CodecError),
    #[error("truncated {what}")]
    Truncated { what: &'static str },
}

/// The per-record envelope on the control stream.
#[derive(Debug, Clone, PartialEq)]
pub enum NetMessage {
    /// Liveness beacon. Carries the sender's current control sequence so the
    /// peer can detect a stall even with no application traffic.
    Heartbeat { seq: u64 },
    /// One application record with its per-stream sequence number.
    Record { seq: u64, record: WireRecord },
    /// Orderly shutdown notice. The sender will finish the stream after this.
    Bye { reason: String },
}

const KIND_HEARTBEAT: u8 = 0;
const KIND_RECORD: u8 = 1;
const KIND_BYE: u8 = 2;
const MAX_BYE_REASON: usize = 512;

impl NetMessage {
    /// The control sequence number attached to this envelope, if any.
    pub fn seq(&self) -> Option<u64> {
        match self {
            Self::Heartbeat { seq } | Self::Record { seq, .. } => Some(*seq),
            Self::Bye { .. } => None,
        }
    }

    /// Encodes the envelope. `max_record` bounds the inner frame.
    pub fn encode(&self, max_record: usize) -> Result<Vec<u8>, MessageError> {
        let mut out = Vec::new();
        match self {
            Self::Heartbeat { seq } => {
                out.push(KIND_HEARTBEAT);
                out.extend_from_slice(&seq.to_le_bytes());
            }
            Self::Record { seq, record } => {
                let inner = record.encode_framed()?;
                if inner.len() > max_record {
                    return Err(MessageError::InnerOversize {
                        declared: inner.len(),
                        cap: max_record,
                    });
                }
                out.push(KIND_RECORD);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
                out.extend_from_slice(&inner);
            }
            Self::Bye { reason } => {
                if reason.len() > MAX_BYE_REASON {
                    return Err(MessageError::ReasonOversize {
                        got: reason.len(),
                        cap: MAX_BYE_REASON,
                    });
                }
                out.push(KIND_BYE);
                out.extend_from_slice(&(reason.len() as u32).to_le_bytes());
                out.extend_from_slice(reason.as_bytes());
            }
        }
        Ok(out)
    }

    /// Decodes an envelope produced by [`Self::encode`]. Every length is checked
    /// against `max_record` before a buffer is sized from it.
    pub fn decode(bytes: &[u8], max_record: usize) -> Result<Self, MessageError> {
        let (&kind, rest) = bytes
            .split_first()
            .ok_or(MessageError::Short { got: 0, need: 1 })?;
        match kind {
            KIND_HEARTBEAT => {
                let seq = le_u64(rest, "heartbeat seq")?;
                Ok(Self::Heartbeat { seq })
            }
            KIND_RECORD => {
                if rest.len() < 12 {
                    return Err(MessageError::Short {
                        got: rest.len(),
                        need: 12,
                    });
                }
                let seq = u64::from_le_bytes(rest[..8].try_into().unwrap());
                let declared = u32::from_le_bytes(rest[8..12].try_into().unwrap()) as usize;
                if declared > max_record {
                    return Err(MessageError::InnerOversize {
                        declared,
                        cap: max_record,
                    });
                }
                let inner = rest.get(12..12 + declared).ok_or(MessageError::Truncated {
                    what: "inner record",
                })?;
                let record = WireRecord::decode_framed(inner)?;
                Ok(Self::Record { seq, record })
            }
            KIND_BYE => {
                if rest.len() < 4 {
                    return Err(MessageError::Short {
                        got: rest.len(),
                        need: 4,
                    });
                }
                let declared = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
                if declared > MAX_BYE_REASON {
                    return Err(MessageError::ReasonOversize {
                        got: declared,
                        cap: MAX_BYE_REASON,
                    });
                }
                let raw = rest.get(4..4 + declared).ok_or(MessageError::Truncated {
                    what: "goodbye reason",
                })?;
                Ok(Self::Bye {
                    reason: String::from_utf8_lossy(raw).into_owned(),
                })
            }
            other => Err(MessageError::UnknownKind(other)),
        }
    }
}

fn le_u64(bytes: &[u8], what: &'static str) -> Result<u64, MessageError> {
    let arr: [u8; 8] = bytes
        .get(..8)
        .ok_or(MessageError::Truncated { what })?
        .try_into()
        .unwrap();
    Ok(u64::from_le_bytes(arr))
}

// --- development authentication -------------------------------------------------

/// Sent by the client at the head of the control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientHello {
    /// Per-run join secret.
    pub token: JoinToken,
    /// The client's compatibility expectations.
    pub handshake: Handshake,
}

/// The server's answer to a [`ClientHello`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerAuthReply {
    Accepted(ServerAccept),
    Rejected(AuthReject),
}

/// A successful authentication: the server's assigned session and its own
/// handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerAccept {
    pub session: spall_protocol::SessionId,
    pub server_handshake: Handshake,
}

/// Why authentication was refused. Each variant is a clear, distinct cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum AuthReject {
    #[error("join token mismatch")]
    BadToken,
    #[error("incompatible handshake: {0}")]
    Incompatible(String),
    #[error("malformed authentication message: {0}")]
    Malformed(String),
    #[error("server is at capacity")]
    Busy,
}

/// Largest accepted `ClientHello` / `ServerAuthReply` postcard body. The
/// handshake is small and bounded; this is generous headroom.
pub const MAX_AUTH_MESSAGE: usize = 8 * 1024;

mod private {
    pub trait Sealed {}
}

impl Sealed for ClientHello {}
impl Sealed for ServerAuthReply {}

/// Postcard-encode a bounded auth message.
pub(crate) fn encode_auth<T: Serialize + Sealed>(value: &T) -> Result<Vec<u8>, AuthReject> {
    let bytes =
        postcard::to_stdvec(value).map_err(|e| AuthReject::Malformed(format!("encode: {e}")))?;
    if bytes.len() > MAX_AUTH_MESSAGE {
        return Err(AuthReject::Malformed(format!(
            "auth message is {} bytes; cap is {MAX_AUTH_MESSAGE}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Postcard-decode a bounded auth message. `bytes` must already be
/// length-limited by the framing layer.
pub(crate) fn decode_auth<T: for<'de> Deserialize<'de> + Sealed>(
    bytes: &[u8],
) -> Result<T, AuthReject> {
    if bytes.len() > MAX_AUTH_MESSAGE {
        return Err(AuthReject::Malformed(format!(
            "auth message is {} bytes; cap is {MAX_AUTH_MESSAGE}",
            bytes.len()
        )));
    }
    postcard::from_bytes(bytes).map_err(|e| AuthReject::Malformed(format!("decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::{ActionOutcome, RequestId, limits};

    fn a_status() -> WireRecord {
        WireRecord::ActionStatus(ActionStatus {
            request_id: RequestId(42),
            outcome: ActionOutcome::Queued,
        })
    }

    #[test]
    fn record_envelope_round_trips_and_keeps_the_t01_frame() {
        let msg = NetMessage::Record {
            seq: 7,
            record: a_status(),
        };
        let bytes = msg.encode(limits::MAX_CONTROL_RECORD).unwrap();
        assert_eq!(
            NetMessage::decode(&bytes, limits::MAX_CONTROL_RECORD).unwrap(),
            msg
        );
    }

    #[test]
    fn heartbeat_and_bye_round_trip() {
        for msg in [
            NetMessage::Heartbeat { seq: 99 },
            NetMessage::Bye {
                reason: "shutdown".into(),
            },
        ] {
            let bytes = msg.encode(limits::MAX_CONTROL_RECORD).unwrap();
            assert_eq!(
                NetMessage::decode(&bytes, limits::MAX_CONTROL_RECORD).unwrap(),
                msg
            );
        }
    }

    #[test]
    fn oversize_inner_length_is_refused_before_allocation() {
        // Hand-craft a RECORD envelope claiming a 2 GiB inner frame.
        let mut bytes = vec![KIND_RECORD];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&2_000_000_000u32.to_le_bytes());
        let err = NetMessage::decode(&bytes, limits::MAX_CONTROL_RECORD).unwrap_err();
        assert!(matches!(err, MessageError::InnerOversize { .. }));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = NetMessage::decode(&[9, 0, 0], limits::MAX_CONTROL_RECORD).unwrap_err();
        assert!(matches!(err, MessageError::UnknownKind(9)));
    }

    #[test]
    fn datagram_only_tags_cannot_ride_the_control_stream() {
        // schema=1, tag=5 (MotionSnapshot), empty body.
        let framed = [0x01, 0x00, 0x05, 0x00];
        assert!(WireRecord::decode_framed(&framed).is_err());
    }
}
