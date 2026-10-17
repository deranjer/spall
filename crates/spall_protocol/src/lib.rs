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
pub mod column_water;
pub mod column_water_wire;
pub mod creature;
pub mod creature_owner;
pub mod handshake;
pub mod input;
pub mod lifecycle_bulk;
pub mod limits;
pub mod records;
pub mod segment;
pub mod session;
pub mod snow;
pub mod water;
pub mod weather;
pub use weather::{
    WEATHER_PROFILE_MS, WEATHER_SCHEMA, WEATHER_TRANSITION_MS, WeatherClock, WeatherClockSource,
    WeatherError, WeatherKind, WeatherSnapshot, WeatherState,
};

pub use input::{frame_input, player_entity, recent_input, session_player_entity};

pub use baseline::{
    BASELINE_WORLD_SCHEMA, BaselineBrick, BaselineCells, BaselineDecodeError, BaselineOwner,
    BaselineVolume, BaselineWorld,
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
    CREATURE_MATERIAL_MAPPING_FORMAT, CREATURE_STATE_SCHEMA, CreatureAssetCatalogEntry,
    CreatureAssetIdentity, CreatureClipMode, CreatureLifecycleState, CreatureMaterialMappingEntry,
    CreaturePartBinding, CreatureSpeciesCatalogEntry, CreatureStateError, CreatureStateV1,
    CreatureValidationCatalog, MAX_CREATURE_ANGULAR_VELOCITY_RAD_S, MAX_CREATURE_ASSET_CLIPS,
    MAX_CREATURE_ASSET_MATERIAL_KEYS, MAX_CREATURE_ASSET_PARTS, MAX_CREATURE_CATALOG_ASSETS,
    MAX_CREATURE_CATALOG_SPECIES, MAX_CREATURE_CLIP_NAME_BYTES, MAX_CREATURE_LINEAR_VELOCITY_M_S,
    MAX_CREATURE_POSITION_M, MAX_CREATURE_SOURCE_PARTS, MAX_CREATURE_SPECIES_KEY_BYTES,
    MAX_CREATURE_STATE_BYTES, creature_material_mapping_hash,
};
pub use creature_owner::{
    CREATURE_CHANGE_DOMAIN, CREATURE_CHANGE_SCHEMA, CREATURE_LIFECYCLE_COMMIT_DOMAIN,
    CREATURE_LIFECYCLE_COMMIT_SCHEMA, CREATURE_MOTION_DOMAIN, CREATURE_OWNER_DOMAIN,
    CREATURE_OWNER_SCHEMA, CREATURE_OWNER_SET_DOMAIN, CREATURE_TOPOLOGY_DOMAIN,
    CreatureComponentMotionV1, CreatureDeathWitnessV1, CreatureLifecycleCandidateV1,
    CreatureLifecycleChangeV1, CreatureLifecycleCommitReplayReceiptV1, CreatureLifecycleCommitV1,
    CreatureLifecycleContextV1, CreatureLifecycleOperationV1, CreatureLocalPoseV1,
    CreatureMaskCellV1, CreatureMotionOverlayV1, CreatureOwnedComponentV1, CreatureOwnershipError,
    CreatureOwnershipRecordV1, CreatureOwnershipSetV1, CreaturePartMotionV1,
    CreatureReductionOutcome, CreatureSourceMaskBrickV1, CreatureSourcePartV1,
    MAX_CREATURE_OWNER_BRICKS, MAX_CREATURE_OWNER_COMPONENTS, MAX_CREATURE_OWNER_DENSE_CELLS,
    MAX_CREATURE_OWNER_MASK_BRICKS, MAX_CREATURE_OWNER_MASK_CELLS, MAX_CREATURE_OWNER_PARTS,
    MAX_CREATURE_OWNER_RECORD_BYTES, MAX_CREATURE_OWNER_SOLID_CELLS,
    reduce_creature_lifecycle_candidate,
};
pub use handshake::{
    AlgorithmVersions, Handshake, Incompatibility, NegotiatedLimits, PROTOCOL_VERSION,
    check_compatible,
};
pub use lifecycle_bulk::{
    CREATURE_LIFECYCLE_BULK_PAYLOAD_SCHEMA, CreatureLifecycleBulkError,
    CreatureLifecycleBulkPayloadV1, MAX_CREATURE_LIFECYCLE_BULK_DECOMPRESSED,
};
pub use limits::SizeLimitError;
pub use records::{
    ActionKind, ActionOutcome, ActionRequest, ActionStatus, BaselineAck, BaselineBegin,
    BaselineEnd, BaselinePart, BaselineRegion, BrickRevision, ClaimedTarget, ControlSeq,
    DurableThrough, InputFrame, InputSeq, InterestEpoch, InventoryEntry, MotionSnapshot,
    ProgressionOperation, ProgressionOutcome, ProgressionRejectCode, ProgressionRequest,
    ProgressionResponse, RecentInput, Record, RecordError, RepairKey, RepairRequest, RequestId,
    SPLIT_BULK_TRANSFER_ID_BIT, SnapshotSeq, TopologyOp, TopologyTransaction, TransferId,
    VolumeHash, WIRE_SCHEMA_VERSION, WORLD_RESET_TRANSFER_ID_BIT, WireTag,
};
pub use segment::BASELINE_CREATURE_LIFECYCLE_WORLD_VERSION;
pub use session::{
    GenerationExhausted, PlayerId, SeqVerdict, SequenceGate, SessionId, SessionRegistry, SlotId,
    StaleSession, StreamKind, StreamSeq,
};
pub use snow::{
    SNOW_FOOTPRINT_TTL_TICKS, SNOW_MAX_DEPTH_Q16, SNOW_MAX_FOOTPRINTS_PER_FRAME,
    SNOW_MAX_SURFACES_PER_FRAME, SNOW_MIN_FOOTPRINT_DEPTH_Q16, SNOW_SCHEMA, SNOW_TILE_CELLS,
    SnowFootprint, SnowSurface, SnowSurfaceFrame, SnowSurfaceId,
};
pub use water::{AdminCommand, AdminRequest, AdminStatus, WaterCodecError};

pub mod vegetation;

pub mod catalogue;
pub use catalogue::{
    CATALOGUE_CHUNK_BYTES, CATALOGUE_WINDOW_CHUNKS, CatalogueAck, CatalogueChunk, ControlProbe,
    MAX_CATALOGUE_CHUNKS, chunk_catalogue,
};
