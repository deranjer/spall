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
mod baseline_decode;
pub mod canonical;
pub mod codec;
pub mod column_water;
pub mod column_water_wire;
pub mod creature;
pub mod creature_motion;
pub use creature_motion::{CreatureMotionEntry, CreatureMotionFrame, MAX_CREATURE_MOTION_ENTRIES};
pub mod handshake;
pub mod input;
pub mod limits;
pub mod records;
pub mod segment;
pub mod session;
pub mod snow;
pub mod transaction_attachments;
pub mod water;
pub mod weather;
pub use weather::{
    SnowTier, WEATHER_PROFILE_MS, WEATHER_TRANSITION_MS, WeatherClock, WeatherClockSource,
    WeatherError, WeatherKind, WeatherSnapshot, WeatherState, Wind, wind_at,
};

pub use input::{frame_input, player_entity, recent_input, session_player_entity};

pub use baseline::{
    BaselineBrick, BaselineCells, BaselineDecodeError, BaselineOwner, BaselineVolume, BaselineWorld,
};
pub use canonical::{
    CanonicalBrick, CanonicalLayer, CanonicalOwner, CanonicalVolume, CanonicalWriter,
    ChunkDigestCache, ChunkKey, HASH_CHUNK_SHIFT, Hash32, HashedVolume, SingleLayerBrick,
    TOPOLOGY_CHUNK_DOMAIN, canonical_topology_hash, chunk_digest, chunk_digest_single_layer,
    chunk_of, content_manifest_hash, content_manifest_hash_with_assets, topology_hash_from_chunks,
};
pub use codec::{
    CodecError, decode_bulk, decode_control, decode_datagram, decode_topology, encode_bulk,
    encode_control, encode_datagram,
};
pub use column_water::{ColumnWaterDelta, ColumnWaterState};
pub use column_water_wire::{ColumnWaterReplica, ColumnWaterUpdate};
pub use creature::{
    CreatureDespawn, CreatureRecord, CreatureSpawn, MAX_CREATURE_SPECIES_KEY_BYTES,
};
pub use handshake::{
    AlgorithmVersions, Handshake, Incompatibility, NegotiatedLimits, PROTOCOL_VERSION,
    check_compatible,
};
pub use limits::SizeLimitError;
pub use records::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, BaselineAck, BaselineBegin,
    BaselineEnd, BaselinePart, BaselineRegion, BrickRevision, ClaimedTarget, ControlSeq,
    DurableThrough, InputFrame, InputSeq, InterestEpoch, InventoryEntry, MotionSnapshot,
    PlayerHealth, ProgressionOperation, ProgressionOutcome, ProgressionRejectCode,
    ProgressionRequest, ProgressionResponse, RecentInput, Record, RecordError, RepairKey,
    RepairRequest, RequestId, SPLIT_BULK_TRANSFER_ID_BIT, SnapshotSeq, TopologyOp,
    TopologyTransaction, TransferId, VolumeHash, WIRE_SCHEMA_VERSION, WORLD_RESET_TRANSFER_ID_BIT,
    WireTag,
};
pub use session::{
    GenerationExhausted, PlayerId, SeqVerdict, SequenceGate, SessionId, SessionRegistry, SlotId,
    StaleSession, StreamKind, StreamSeq,
};
pub use snow::{
    SNOW_FOOTPRINT_TTL_TICKS, SNOW_MAX_DEPTH_Q16, SNOW_MAX_FOOTPRINTS_PER_FRAME,
    SNOW_MAX_SURFACES_PER_FRAME, SNOW_MIN_FOOTPRINT_DEPTH_Q16, SNOW_TILE_CELLS, SnowFootprint,
    SnowSurface, SnowSurfaceFrame, SnowSurfaceId,
};
pub use transaction_attachments::{
    MAX_TRANSACTION_ATTACHMENT_BYTES, MAX_TRANSACTION_GEOMETRY_DECODED_BYTES,
    TransactionAttachmentError, TransactionAttachmentSetV1, TransactionAttachmentV1,
    ValidatedTransactionAttachmentSetV1, ValidatedTransactionAttachmentV1, decode_for_transaction,
};
pub use water::{AdminCommand, AdminRequest, AdminStatus, WaterCodecError};

pub mod vegetation;

pub mod catalogue;
pub use catalogue::{
    CATALOGUE_CHUNK_BYTES, CATALOGUE_WINDOW_CHUNKS, CatalogueAck, CatalogueChunk, ControlProbe,
    MAX_CATALOGUE_CHUNKS, chunk_catalogue,
};
