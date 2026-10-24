//! The SQLite schema and its version guard.
//!
//! `PRAGMA user_version` is the authoritative on-disk schema version. A fresh
//! database (`user_version == 0`) is stamped and populated; a database at the
//! current version opens as-is; a **newer** version is rejected without any
//! modification, and an older non-zero version is rejected without modification.

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
CREATE TABLE IF NOT EXISTS checkpoint_vegetation (
    tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE,
    state BLOB NOT NULL,
    crc BLOB NOT NULL
);
CREATE TABLE checkpoint_weather (
    tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE,
    state BLOB NOT NULL,
    crc BLOB NOT NULL
);
CREATE TABLE checkpoint_players (
    tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE,
    state BLOB NOT NULL,
    crc BLOB NOT NULL
);
CREATE TABLE checkpoint_snow (
    tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE,
    state BLOB NOT NULL,
    crc BLOB NOT NULL
);
CREATE TABLE checkpoint_column_water (
    tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE,
    state BLOB NOT NULL,
    crc BLOB NOT NULL
);
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

CREATE TABLE IF NOT EXISTS game_outbox (
    event_id        BLOB PRIMARY KEY CHECK (length(event_id) = 32),
    journal_seq     INTEGER NOT NULL,
    payload         BLOB NOT NULL
) WITHOUT ROWID;
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
            let user_objects: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','view') AND substr(name,1,7) <> 'sqlite_'",
                [],
                |row| row.get(0),
            )?;
            if user_objects != 0 {
                return Err(StoreError::SchemaTooOld {
                    found: 0,
                    supported: STORE_SCHEMA_VERSION,
                });
            }
            conn.execute_batch(&format!(
                "BEGIN;\n{SCHEMA_SQL}\nPRAGMA user_version = {STORE_SCHEMA_VERSION};\nCOMMIT;"
            ))?;
            Ok(())
        }
        v if v == STORE_SCHEMA_VERSION => validate_schema_layout(conn),
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

/// Refuses incomplete schemas without attempting an implicit migration.
pub(crate) fn validate_schema_layout(conn: &Connection) -> Result<(), StoreError> {
    const REQUIRED: &[(&str, &[&str])] = &[
        ("checkpoint_vegetation", &["tick", "state", "crc"]),
        ("checkpoint_weather", &["tick", "state", "crc"]),
        ("checkpoint_players", &["tick", "state", "crc"]),
        ("checkpoint_snow", &["tick", "state", "crc"]),
        ("checkpoint_column_water", &["tick", "state", "crc"]),
        ("world", &["id", "meta"]),
        (
            "checkpoints",
            &[
                "tick",
                "journal_cursor",
                "world_hash",
                "meta",
                "complete",
                "created_unix_ms",
            ],
        ),
        ("checkpoint_bodies", &["tick", "entity_id", "body"]),
        (
            "checkpoint_bricks",
            &[
                "tick",
                "volume_id",
                "bx",
                "by",
                "bz",
                "revision",
                "edited",
                "payload",
            ],
        ),
        ("journal", &["seq", "tick", "payload", "crc"]),
        ("game_outbox", &["event_id", "journal_seq", "payload"]),
    ];
    for (name, expected_columns) in REQUIRED {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({name})"))?;
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))?
            .collect::<Result<_, _>>()?;
        if columns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
            != *expected_columns
        {
            return Err(StoreError::Corrupt(format!(
                "current schema has missing or unexpected layout for table {name}"
            )));
        }
    }
    Ok(())
}
