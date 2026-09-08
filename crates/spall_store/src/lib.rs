//! `spall_store` — the durable world checkpoint and journal (T16).
//!
//! No GPU, window, network, or simulation dependency. This crate owns the save
//! **schema and durable I/O** only; `spall_sim::persist` owns the conversion
//! between authoritative runtime state and these records (`docs/architecture.md`:
//! "persistence does not own simulation objects").
//!
//! # What it provides
//!
//! * [`dto`] — the versioned save schema: [`StoredWorldMeta`], [`StoredBody`],
//!   [`StoredBrick`] (zstd-compressed, bounded decode via [`brick`]),
//!   [`Checkpoint`], [`JournalRecord`]. Bumped as one by
//!   [`dto::STORE_SCHEMA_VERSION`].
//! * [`Writer`] — the single WAL writer. Verifies `journal_mode=WAL` and
//!   `synchronous=FULL` on open; [`Writer::append_journal`] groups records into
//!   one transaction and returns a [`spall_protocol::DurableThrough`] only after
//!   a successful commit; [`Writer::publish_checkpoint`] writes every
//!   body/brick row and the journal cursor in one transaction.
//! * [`recover`] — loads the latest complete checkpoint and the contiguous,
//!   CRC-verified journal suffix, reporting interior corruption and offering the
//!   previous checkpoint as a fallback.
//! * [`fault`] — controlled [`CrashPoint`]s and disk-error injection for the
//!   persistence crash tests.

pub mod brick;
pub mod db;
pub mod dto;
pub mod fault;
pub mod metrics;
pub mod recover;
mod schema;

pub use brick::{BrickCodecError, DENSE_CELL_BYTES, decode_cells, encode_cells};
pub use db::{DEFAULT_MAX_BATCH_RECORDS, WalCheckpoint, Writer};
pub use dto::{
    BrickPayload, Checkpoint, DtoError, JournalPayload, JournalRecord, MAX_STORED_BRICK_BYTES,
    STORE_SCHEMA_VERSION, StoredBody, StoredBodyKind, StoredBrick, StoredPose, StoredWorldMeta,
};
pub use fault::{CrashPoint, FaultPlan};
pub use metrics::WriteMetrics;
pub use recover::{CorruptionReport, Recovery, recover, recover_conn};

/// Anything that can go wrong opening, writing, or recovering a world database.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("save DTO: {0}")]
    Dto(#[from] DtoError),
    #[error("brick codec: {0}")]
    Brick(#[from] BrickCodecError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error(
        "database schema version {found} is newer than supported {supported}; refusing to touch it"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(
        "database schema version {found} predates supported {supported}; a migration is required"
    )]
    SchemaTooOld { found: u32, supported: u32 },
    #[error("database did not enter WAL mode (got {0:?})")]
    NotWalMode(String),
    #[error("database synchronous level is {0}, need FULL (2) or stricter")]
    NotSynchronousFull(i64),

    #[error("no journal records to append")]
    Empty,
    #[error("journal batch not contiguous: expected seq {expected}, got {got}")]
    JournalGap { expected: u64, got: u64 },
    #[error("journal batch of {pending} records exceeds the {cap} cap")]
    QueueFull { pending: usize, cap: usize },

    #[error("writer stopped after an earlier failure: {0}")]
    Poisoned(String),
    #[error("crash injected at {0:?}")]
    CrashInjected(CrashPoint),
    #[error("disk write failed: {0}")]
    Disk(String),
    #[error("corrupt persistent data: {0}")]
    Corrupt(String),
    #[error("no complete checkpoint to recover from")]
    NoCheckpoint,
}
