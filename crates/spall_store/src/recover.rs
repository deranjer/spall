//! Recovery: the latest complete checkpoint plus the durable, CRC-verified
//! journal suffix after it.
//!
//! `docs/protocol.md`: "Recovery loads the latest complete checkpoint and
//! replays the durable ordered journal suffix. Ignore no interior corrupt
//! record: report corruption and offer the previous valid checkpoint as an
//! explicit recovery choice." So a gap or a CRC failure in the suffix truncates
//! the replay at that point *and* is reported, and the previous checkpoint is
//! always surfaced as a fallback.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::StoreError;
use crate::db::crc16;
use crate::dto::{
    self, BrickPayload, Checkpoint, JournalPayload, JournalRecord, STORE_SCHEMA_VERSION,
    StoredBody, StoredBrick, StoredPlayerHealth, StoredWorldItems, StoredWorldMeta,
};
use crate::schema::{read_user_version, validate_schema_layout};

fn decode_journal_payload(bytes: &[u8]) -> Result<JournalPayload, dto::DtoError> {
    let (payload, rest) = postcard::take_from_bytes(bytes)?;
    if !rest.is_empty() {
        return Err(dto::DtoError::Postcard(
            postcard::Error::DeserializeBadEncoding,
        ));
    }
    Ok(payload)
}

/// One non-fatal problem found during recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptionReport {
    pub detail: String,
}

impl CorruptionReport {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

/// The durable state to resume from.
#[derive(Debug, Clone)]
pub struct Recovery {
    /// The checkpoint recovery selected: the newest complete one whose rows all
    /// decoded.
    pub checkpoint: Checkpoint,
    /// The next-newest complete checkpoint, if any — the explicit fallback a
    /// corrupt suffix leaves the operator (`docs/protocol.md`).
    pub previous_checkpoint: Option<Checkpoint>,
    /// Journal records strictly after `checkpoint.journal_cursor`: contiguous,
    /// CRC-verified, ordered. Replaying them over the checkpoint yields the
    /// durable world.
    pub journal: Vec<JournalRecord>,
    /// Highest contiguous durable journal sequence. Equal to the checkpoint
    /// cursor when the suffix is empty; everything after it was lost.
    pub durable_through: u64,
    /// Non-fatal problems found. Empty on a clean recovery.
    pub corruption: Vec<CorruptionReport>,
}

impl Recovery {
    /// `true` when the durable prefix ends before the last stored journal row —
    /// i.e. a crash lost an unflushed suffix or an interior record is corrupt.
    pub fn is_truncated(&self) -> bool {
        !self.corruption.is_empty()
    }
}

/// Which complete checkpoint a recovery resumes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverBase {
    /// The newest complete checkpoint — normal resume (least journal replay).
    Latest,
    /// The **oldest** complete checkpoint, replaying the whole journal after it.
    /// Used to verify that the committed topology-event stream, replayed from
    /// the original baseline, reproduces the live world (T11 exact replay).
    Oldest,
    /// Explicit recovery choice; reloads the journal from this candidate's cursor.
    AtOrBefore(u64),
}

/// Opens `path` read/write (it must exist) and recovers the durable state from
/// the newest checkpoint.
pub fn recover(path: impl AsRef<Path>) -> Result<Recovery, StoreError> {
    let conn = Connection::open_with_flags(
        path.as_ref(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    recover_conn(&conn)
}

/// Like [`recover`], but resumes from the **oldest** complete checkpoint and
/// replays the entire durable journal after it — a full-history replay from the
/// original baseline.
pub fn recover_from_base(path: impl AsRef<Path>) -> Result<Recovery, StoreError> {
    let conn = Connection::open_with_flags(
        path.as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    recover_conn_from(&conn, RecoverBase::Oldest)
}

/// Recovers using an already-open connection (the live [`crate::Writer`] uses
/// this), resuming from the newest checkpoint.
pub fn recover_conn(conn: &Connection) -> Result<Recovery, StoreError> {
    recover_conn_from(conn, RecoverBase::Latest)
}

/// Recovers using an already-open connection, choosing which checkpoint to
/// resume from.
pub fn recover_conn_from(conn: &Connection, base: RecoverBase) -> Result<Recovery, StoreError> {
    let version = read_user_version(conn)?;
    if version > STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: version,
            supported: STORE_SCHEMA_VERSION,
        });
    }
    if version == 0 {
        return Err(StoreError::NoCheckpoint);
    }
    if version < STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooOld {
            found: version,
            supported: STORE_SCHEMA_VERSION,
        });
    }
    validate_schema_layout(conn)?;

    let complete_ticks: Vec<i64> = {
        let order = match base {
            RecoverBase::Latest | RecoverBase::AtOrBefore(_) => "DESC",
            RecoverBase::Oldest => "ASC",
        };
        let ceiling = match base {
            RecoverBase::AtOrBefore(tick) => Some(tick.min(i64::MAX as u64) as i64),
            _ => None,
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT tick FROM checkpoints WHERE complete = 1 AND (?1 IS NULL OR tick <= ?1) ORDER BY tick {order}"
        ))?;
        let rows = stmt.query_map([ceiling], |r| r.get::<_, i64>(0))?;
        rows.collect::<Result<_, _>>()?
    };

    let mut corruption = Vec::new();
    let mut selected: Option<Checkpoint> = None;
    let mut previous: Option<Checkpoint> = None;
    for tick in complete_ticks {
        match load_checkpoint(conn, tick) {
            Ok(cp) if selected.is_none() => selected = Some(cp),
            Ok(cp) => {
                previous = Some(cp);
                break;
            }
            Err(e) => corruption.push(CorruptionReport::new(format!(
                "checkpoint tick {tick} did not load: {e}"
            ))),
        }
    }
    // Distinguish a genuinely empty database (no complete checkpoint rows at all)
    // from a corrupt one (checkpoint rows exist but none decoded). The caller
    // must not treat the latter as a fresh DB and overwrite it.
    let checkpoint = match selected {
        Some(cp) => cp,
        None if corruption.is_empty() => return Err(StoreError::NoCheckpoint),
        None => {
            let details = corruption
                .iter()
                .map(|c| c.detail.clone())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(StoreError::CheckpointsUnrecoverable(details));
        }
    };

    // Walk the journal suffix, stopping at the first gap or CRC failure.
    let cursor = checkpoint.journal_cursor;
    let mut journal = Vec::new();
    let mut durable_through = cursor;
    let mut expected = cursor + 1;
    {
        let mut stmt =
            conn.prepare("SELECT seq, tick, payload, crc FROM journal WHERE seq > ? ORDER BY seq")?;
        let mut rows = stmt.query([cursor as i64])?;
        while let Some(row) = rows.next()? {
            let seq = row.get::<_, i64>(0)? as u64;
            let tick = row.get::<_, i64>(1)? as u64;
            // SQLite already owns this row buffer. Decode borrowed fields before
            // copying; typed attachment bytes enforce their cap before allocation.
            let payload = row
                .get_ref(2)?
                .as_blob()
                .map_err(|e| StoreError::Corrupt(format!("journal payload SQL type: {e}")))?;
            let crc = row
                .get_ref(3)?
                .as_blob()
                .map_err(|e| StoreError::Corrupt(format!("journal CRC SQL type: {e}")))?;

            if seq != expected {
                corruption.push(CorruptionReport::new(format!(
                    "journal gap: expected seq {expected}, found {seq}"
                )));
                break;
            }
            if crc16(payload) != crc {
                corruption.push(CorruptionReport::new(format!(
                    "journal seq {seq} failed CRC; durable prefix ends at {durable_through}"
                )));
                break;
            }
            let decoded: JournalPayload = match decode_journal_payload(payload) {
                Ok(p) => p,
                Err(e) => {
                    corruption.push(CorruptionReport::new(format!(
                        "journal seq {seq} did not decode: {e}"
                    )));
                    break;
                }
            };
            let record = JournalRecord {
                seq,
                tick,
                payload: decoded,
            };
            if let Err(error) = record.validate() {
                corruption.push(CorruptionReport::new(format!(
                    "journal seq {seq} has invalid auxiliary state: {error}"
                )));
                break;
            }
            journal.push(record);
            durable_through = seq;
            expected += 1;
        }
    }

    Ok(Recovery {
        checkpoint,
        previous_checkpoint: previous,
        journal,
        durable_through,
        corruption,
    })
}

fn load_checkpoint(conn: &Connection, tick: i64) -> Result<Checkpoint, StoreError> {
    let (journal_cursor, world_hash_blob, meta_blob): (i64, Vec<u8>, Vec<u8>) = conn.query_row(
        "SELECT journal_cursor, world_hash, meta FROM checkpoints WHERE tick = ? AND complete = 1",
        [tick],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let meta: StoredWorldMeta = dto::decode(&meta_blob)?;
    if meta.store_schema_version != STORE_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: meta.store_schema_version,
            supported: STORE_SCHEMA_VERSION,
        });
    }
    let world_hash: [u8; 32] = world_hash_blob.as_slice().try_into().map_err(|_| {
        StoreError::Corrupt(format!("checkpoint {tick} world_hash is not 32 bytes"))
    })?;

    let bodies: Vec<StoredBody> = {
        let mut stmt =
            conn.prepare("SELECT body FROM checkpoint_bodies WHERE tick = ? ORDER BY entity_id")?;
        let rows = stmt.query_map([tick], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for blob in rows {
            out.push(dto::decode::<StoredBody>(&blob?)?);
        }
        out
    };

    let bricks: Vec<StoredBrick> = {
        let mut stmt = conn.prepare(
            "SELECT volume_id, bx, by, bz, revision, edited, payload \
             FROM checkpoint_bricks WHERE tick = ? ORDER BY volume_id, bz, by, bx",
        )?;
        let rows = stmt.query_map([tick], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, Vec<u8>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (volume_id, bx, by, bz, revision, edited, payload) = row?;
            out.push(StoredBrick {
                volume_id: volume_id as u64,
                coord: [bx, by, bz],
                revision: revision as u64,
                edited,
                payload: dto::decode::<BrickPayload>(&payload)?,
            });
        }
        out
    };

    Ok(Checkpoint {
        vegetation: load_vegetation(conn, tick)?,
        column_water: load_column_water(conn, tick)?,
        weather: load_weather(conn, tick)?,
        players: load_players(conn, tick)?,
        world_items: load_world_items(conn, tick)?,
        snow: load_snow(conn, tick)?,
        tick: tick as u64,
        journal_cursor: journal_cursor as u64,
        world_hash,
        meta,
        bodies,
        bricks,
    })
}

fn load_column_water(
    conn: &Connection,
    tick: i64,
) -> Result<Option<spall_protocol::ColumnWaterState>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'checkpoint_column_water')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(StoreError::Corrupt(
            "missing the column water checkpoint table".into(),
        ));
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT state, crc FROM checkpoint_column_water WHERE tick = ? AND length(state) <= ?",
            rusqlite::params![tick, dto::MAX_STORED_COLUMN_WATER_BYTES as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (blob, crc) =
        row.ok_or_else(|| StoreError::Corrupt("missing column water checkpoint row".into()))?;
    if crc.as_slice() != crc16(&blob) {
        return Err(StoreError::Corrupt(
            "column water checkpoint checksum mismatch".into(),
        ));
    }
    let state: Option<spall_protocol::ColumnWaterState> = dto::decode(&blob)?;
    if let Some(state) = &state {
        state
            .validate()
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
    }
    Ok(state)
}

fn load_vegetation(conn: &Connection, tick: i64) -> Result<Vec<u8>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'checkpoint_vegetation')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT state,crc FROM checkpoint_vegetation WHERE tick=?",
            [tick],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((bytes, crc)) = row else {
        return Err(StoreError::Corrupt(
            "vegetation checkpoint row missing".into(),
        ));
    };
    if bytes.len() > 16 * 1024 * 1024 || crc.as_slice() != crc16(&bytes).as_slice() {
        return Err(StoreError::Corrupt(
            "vegetation checkpoint checksum/limit".into(),
        ));
    }
    Ok(bytes)
}

fn load_snow(conn: &Connection, tick: i64) -> Result<Option<Vec<u8>>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='checkpoint_snow')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(StoreError::Corrupt(
            "current schema is missing required table checkpoint_snow".into(),
        ));
    }
    // Check the SQLite length before copying a potentially corrupt large BLOB.
    let len: Option<i64> = conn
        .query_row(
            "SELECT length(state) FROM checkpoint_snow WHERE tick=?",
            [tick],
            |r| r.get(0),
        )
        .optional()?;
    let Some(len) = len else {
        return Err(StoreError::Corrupt("snow checkpoint row missing".into()));
    };
    if !(0..=64 * 1024 * 1024 + 16).contains(&len) {
        return Err(StoreError::Corrupt("snow checkpoint byte limit".into()));
    }
    let (blob, crc): (Vec<u8>, Vec<u8>) = conn.query_row(
        "SELECT state,crc FROM checkpoint_snow WHERE tick=?",
        [tick],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if crc.as_slice() != crc16(&blob).as_slice() {
        return Err(StoreError::Corrupt(
            "snow checkpoint checksum mismatch".into(),
        ));
    }
    let state: Option<Vec<u8>> = dto::decode(&blob)?;
    if state
        .as_ref()
        .is_some_and(|bytes| bytes.len() > 64 * 1024 * 1024)
    {
        return Err(StoreError::Corrupt("snow checkpoint byte limit".into()));
    }
    Ok(state)
}

fn load_world_items(conn: &Connection, tick: i64) -> Result<StoredWorldItems, StoreError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT state,crc FROM checkpoint_world_items WHERE tick=?",
            [tick],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((blob, crc)) = row else {
        return Err(StoreError::Corrupt(
            "world items checkpoint row missing".into(),
        ));
    };
    if crc.as_slice() != crc16(&blob).as_slice() {
        return Err(StoreError::Corrupt(
            "world items checkpoint checksum mismatch".into(),
        ));
    }
    Ok(dto::decode(&blob)?)
}

fn load_players(conn: &Connection, tick: i64) -> Result<Vec<StoredPlayerHealth>, StoreError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT state,crc FROM checkpoint_players WHERE tick=?",
            [tick],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((blob, crc)) = row else {
        return Err(StoreError::Corrupt("players checkpoint row missing".into()));
    };
    if crc.as_slice() != crc16(&blob).as_slice() {
        return Err(StoreError::Corrupt(
            "players checkpoint checksum mismatch".into(),
        ));
    }
    Ok(dto::decode(&blob)?)
}

fn load_weather(
    conn: &Connection,
    tick: i64,
) -> Result<Option<spall_protocol::WeatherState>, StoreError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='checkpoint_weather')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(StoreError::Corrupt(
            "current schema is missing required table checkpoint_weather".into(),
        ));
    }
    let row: Option<(Vec<u8>, Vec<u8>)> = conn
        .query_row(
            "SELECT state,crc FROM checkpoint_weather WHERE tick=?",
            [tick],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((blob, crc)) = row else {
        return Err(StoreError::Corrupt("weather checkpoint row missing".into()));
    };
    if crc.as_slice() != crc16(&blob).as_slice() {
        return Err(StoreError::Corrupt(
            "weather checkpoint checksum mismatch".into(),
        ));
    }
    let state: Option<spall_protocol::WeatherState> = dto::decode(&blob)?;
    if let Some(state) = &state {
        state
            .validate()
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
    }
    Ok(state)
}
