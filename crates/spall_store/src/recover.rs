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

use rusqlite::{Connection, OpenFlags};

use crate::StoreError;
use crate::db::crc16;
use crate::dto::{
    self, BrickPayload, Checkpoint, JournalPayload, JournalRecord, STORE_SCHEMA_VERSION,
    StoredBody, StoredBrick, StoredWorldMeta,
};
use crate::schema::read_user_version;

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

/// Opens `path` read/write (it must exist) and recovers the durable state.
pub fn recover(path: impl AsRef<Path>) -> Result<Recovery, StoreError> {
    let conn = Connection::open_with_flags(
        path.as_ref(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    recover_conn(&conn)
}

/// Recovers using an already-open connection (the live [`crate::Writer`] uses
/// this).
pub fn recover_conn(conn: &Connection) -> Result<Recovery, StoreError> {
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

    let complete_ticks: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT tick FROM checkpoints WHERE complete = 1 ORDER BY tick DESC")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
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
            let payload: Vec<u8> = row.get(2)?;
            let crc: Vec<u8> = row.get(3)?;

            if seq != expected {
                corruption.push(CorruptionReport::new(format!(
                    "journal gap: expected seq {expected}, found {seq}"
                )));
                break;
            }
            if crc16(&payload) != crc {
                corruption.push(CorruptionReport::new(format!(
                    "journal seq {seq} failed CRC; durable prefix ends at {durable_through}"
                )));
                break;
            }
            let decoded: JournalPayload = match dto::decode(&payload) {
                Ok(p) => p,
                Err(e) => {
                    corruption.push(CorruptionReport::new(format!(
                        "journal seq {seq} did not decode: {e}"
                    )));
                    break;
                }
            };
            journal.push(JournalRecord {
                seq,
                tick,
                payload: decoded,
            });
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
        tick: tick as u64,
        journal_cursor: journal_cursor as u64,
        world_hash,
        meta,
        bodies,
        bricks,
    })
}
