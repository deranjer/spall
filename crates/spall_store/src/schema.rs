//! The SQLite schema and its version guard.
//!
//! `PRAGMA user_version` is the authoritative on-disk schema version. A fresh
//! database (`user_version == 0`) is stamped and populated; a database at the
//! current version opens as-is; a **newer** version is rejected without any
//! modification, and an older non-zero version is rejected pending a migration
//! path (`docs/protocol.md`: "Use versioned DTO migrations with fixture worlds
//! before changing save formats").

use rusqlite::Connection;

use crate::StoreError;
use crate::dto::STORE_SCHEMA_VERSION;

/// The complete schema, applied in one transaction on a fresh database.
///
/// * `world` — one row (`id = 1`) holding the postcard [`crate::StoredWorldMeta`].
/// * `checkpoints` — one row per published engine checkpoint; `complete = 1`
///   only after every body/brick row and the journal cursor are in the same
///   transaction.
/// * `checkpoint_bodies` / `checkpoint_bricks` — the checkpoint's payload,
///   keyed by tick so an older checkpoint is retained intact until pruned.
/// * `journal` — the ordered authoritative journal; `crc` is a BLAKE3-16 of
///   `payload` for interior-corruption detection.
const SCHEMA_SQL: &str = r#"
CREATE TABLE world (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    meta            BLOB NOT NULL
);

CREATE TABLE checkpoints (
    tick            INTEGER PRIMARY KEY,
    journal_cursor  INTEGER NOT NULL,
    world_hash      BLOB NOT NULL,
    meta            BLOB NOT NULL,
    complete        INTEGER NOT NULL DEFAULT 0,
    created_unix_ms INTEGER NOT NULL
);

CREATE TABLE checkpoint_bodies (
    tick            INTEGER NOT NULL REFERENCES checkpoints(tick) ON DELETE CASCADE,
    entity_id       INTEGER NOT NULL,
    body            BLOB NOT NULL,
    PRIMARY KEY (tick, entity_id)
) WITHOUT ROWID;

CREATE TABLE checkpoint_bricks (
    tick            INTEGER NOT NULL REFERENCES checkpoints(tick) ON DELETE CASCADE,
    volume_id       INTEGER NOT NULL,
    bx              INTEGER NOT NULL,
    by              INTEGER NOT NULL,
    bz              INTEGER NOT NULL,
    revision        INTEGER NOT NULL,
    edited          INTEGER NOT NULL,
    payload         BLOB NOT NULL,
    PRIMARY KEY (tick, volume_id, bx, by, bz)
) WITHOUT ROWID;

CREATE TABLE journal (
    seq             INTEGER PRIMARY KEY,
    tick            INTEGER NOT NULL,
    payload         BLOB NOT NULL,
    crc             BLOB NOT NULL
);
"#;

/// Reads `PRAGMA user_version`.
pub(crate) fn read_user_version(conn: &Connection) -> Result<u32, StoreError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(u32::try_from(v).unwrap_or(u32::MAX))
}

/// Ensures the connection carries the current schema. Applies it on a fresh
/// database; rejects any other non-matching version.
pub(crate) fn ensure_schema(conn: &Connection) -> Result<(), StoreError> {
    let found = read_user_version(conn)?;
    match found {
        0 => {
            conn.execute_batch(&format!(
                "BEGIN;\n{SCHEMA_SQL}\nPRAGMA user_version = {STORE_SCHEMA_VERSION};\nCOMMIT;"
            ))?;
            Ok(())
        }
        v if v == STORE_SCHEMA_VERSION => Ok(()),
        v if v > STORE_SCHEMA_VERSION => Err(StoreError::SchemaTooNew {
            found: v,
            supported: STORE_SCHEMA_VERSION,
        }),
        v => Err(StoreError::SchemaTooOld {
            found: v,
            supported: STORE_SCHEMA_VERSION,
        }),
    }
}
