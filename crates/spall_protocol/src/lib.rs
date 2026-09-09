//! `spall_protocol` — explicit versioned DTOs and codecs for Spall's
//! replication, baseline, and recovery contract.
//!
//! This crate turns `docs/protocol.md` into tested types. It contains **only**
//! data definitions and pure codec/hash logic: no transport, no async, no
//! simulation. `spall_net` (T09) layers Quinn on top of these records;
//! `spall_store` (T16) reuses the canonical encoding for journal bytes.
//!
//! Conventions frozen here for every downstream task:
//!
//! * **Endianness** — all canonical hash bytes and frame headers are
//!   little-endian. See [`canonical::CanonicalWriter`].
//! * **Framing** — `u16` schema version, `u16` [`records::WireTag`], then a
//!   postcard body. See [`codec`].
//! * **Sort orders** — topology hashing sorts volumes by id, bricks by
//!   `(z, y, x)`, layers by `kind`. See [`canonical::canonical_topology_hash`].
//! * **Limits** — [`limits`] holds every byte/count ceiling; the decode path
//!   checks them before allocating.
//! * **Sessions** — a `u64` [`session::SessionId`] is `slot << 32 | generation`;
//!   reconnect bumps the generation and stales the old session.

pub mod baseline;
pub mod canonical;
pub mod codec;
pub mod handshake;
pub mod input;
pub mod limits;
pub mod records;
pub mod session;

pub use input::{frame_input, player_entity, recent_input, session_player_entity};

pub use baseline::{
    BASELINE_WORLD_SCHEMA, BaselineBrick, BaselineCells, BaselineDecodeError, BaselineOwner,
    BaselineVolume, BaselineWorld,
};
pub use canonical::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, CanonicalWriter, Hash32,
    canonical_topology_hash, content_manifest_hash,
};
pub use codec::{
    CodecError, decode_bulk, decode_control, decode_datagram, decode_topology, encode_bulk,
    encode_control, encode_datagram,
};
pub use handshake::{
    AlgorithmVersions, Handshake, Incompatibility, NegotiatedLimits, PROTOCOL_VERSION,
    check_compatible,
};
pub use limits::SizeLimitError;
pub use records::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, BaselineAck, BaselineBegin,
    BaselineEnd, BaselinePart, BaselineRegion, BrickRevision, ClaimedTarget, ControlSeq,
    DurableThrough, InputFrame, InputSeq, InterestEpoch, MotionSnapshot, RecentInput, Record,
    RecordError, RepairKey, RepairRequest, RequestId, SnapshotSeq, TopologyOp, TopologyTransaction,
    TransferId, VolumeHash, WIRE_SCHEMA_VERSION, WireTag,
};
pub use session::{
    GenerationExhausted, SeqVerdict, SequenceGate, SessionId, SessionRegistry, SlotId,
    StaleSession, StreamKind, StreamSeq,
};
