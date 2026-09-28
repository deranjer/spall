//! An on-disk cache for individually captured/reloaded brick geometry (T23 /
//! G3 row 7, item 2 — "a real disk-backed `BrickBacking`").
//!
//! This is deliberately **not** part of the versioned save schema
//! ([`crate::dto::STORE_SCHEMA_VERSION`] / [`crate::schema`]): the checkpoint
//! and journal tables are the durable, authoritative save format recovery
//! depends on, published atomically per `docs/protocol.md`'s checkpoint
//! contract. This store is a rebuildable *cache* the residency subsystem
//! reads and writes far more often — on every commit that touches a resident
//! brick, and again, gated, immediately before every eviction (the
//! ack-before-evict contract; see `spall_sim::backing::BrickBackingWriter`
//! and `docs/reports/G3.md` increment 27). Losing this database costs
//! nothing but a full-world re-baseline the next time a reload actually
//! misses it — increment 22 already established that journal replay from a
//! checkpoint reconstructs the world independently of any residency backing
//! (`docs/reports/G3.md`, row 7: "not unsafe for crash recovery ... but not
//! durable in its own right"). Keeping it in its own table, in its own
//! database file, means it can be written on every eviction without touching
//! `STORE_SCHEMA_VERSION` or the checkpoint atomicity contract at all.
//!
//! Same low-level conventions as the rest of `spall_store`: one SQLite file,
//! WAL mode, zstd-compressed brick payloads via [`crate::brick`], bounded
//! one-brick decode via [`crate::dto::BrickPayload`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags, params};

use crate::StoreError;
use crate::dto::{self, BrickPayload};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS residency_bricks (
    volume_id   INTEGER NOT NULL,
    bx          INTEGER NOT NULL,
    by          INTEGER NOT NULL,
    bz          INTEGER NOT NULL,
    revision    INTEGER NOT NULL,
    edited      INTEGER NOT NULL,
    known_empty INTEGER NOT NULL,
    payload     BLOB,
    PRIMARY KEY (volume_id, bx, by, bz)
) WITHOUT ROWID;
"#;

/// One durable record as read back from the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidencyRecord {
    /// Real geometry, ready to reinstall.
    Brick {
        revision: u64,
        edited: bool,
        payload: BrickPayload,
    },
    /// A durably-known modified-air tombstone: no payload is stored because
    /// "air" needs none, but the revision/edited flag must still round-trip.
    KnownEmpty { revision: u64, edited: bool },
}

/// A durable, on-disk brick cache for the residency subsystem. One SQLite
/// file, its own schema, entirely independent of the checkpoint/journal
/// database (a caller may point both at files in the same world directory;
/// they never share a table or a schema version). Interior-mutable so callers
/// can share one instance behind an `Arc`, the same way
/// `spall_sim::MemoryBacking` does for the in-process default.
pub struct ResidencyStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl ResidencyStore {
    /// Opens (creating if absent) the residency cache database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Best-effort durability pragmas. Unlike `Writer` (the authoritative
        // checkpoint/journal writer), a pragma mismatch here is not a hard
        // error -- this store is a rebuildable cache, not a contract callers
        // depend on for crash recovery. WAL still matters even so: it keeps a
        // residency read (`get`) from blocking behind a concurrent write.
        let _: rusqlite::Result<String> =
            conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0));
        let _ = conn.execute_batch("PRAGMA synchronous=NORMAL");
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Upserts a brick's current geometry.
    pub fn put_brick(
        &self,
        volume_id: u64,
        coord: [i64; 3],
        revision: u64,
        edited: bool,
        payload: &BrickPayload,
    ) -> Result<(), StoreError> {
        let blob = dto::encode(payload)?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO residency_bricks \
             (volume_id, bx, by, bz, revision, edited, known_empty, payload) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?) \
             ON CONFLICT (volume_id, bx, by, bz) DO UPDATE SET \
             revision = excluded.revision, edited = excluded.edited, \
             known_empty = 0, payload = excluded.payload",
            params![
                volume_id as i64,
                coord[0],
                coord[1],
                coord[2],
                revision as i64,
                edited,
                &blob,
            ],
        )?;
        Ok(())
    }

    /// Upserts a known-empty (modified-air tombstone) record. No payload is
    /// stored; `get`/`records` reconstruct it as `BrickPayload::Uniform(0)`.
    pub fn put_known_empty(
        &self,
        volume_id: u64,
        coord: [i64; 3],
        revision: u64,
        edited: bool,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO residency_bricks \
             (volume_id, bx, by, bz, revision, edited, known_empty, payload) \
             VALUES (?, ?, ?, ?, ?, ?, 1, NULL) \
             ON CONFLICT (volume_id, bx, by, bz) DO UPDATE SET \
             revision = excluded.revision, edited = excluded.edited, \
             known_empty = 1, payload = NULL",
            params![
                volume_id as i64,
                coord[0],
                coord[1],
                coord[2],
                revision as i64,
                edited,
            ],
        )?;
        Ok(())
    }

    /// Removes any record for `(volume_id, coord)` -- a lost/corrupt durable
    /// record test hook, mirroring `MemoryBacking::mark_unavailable`.
    pub fn remove(&self, volume_id: u64, coord: [i64; 3]) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "DELETE FROM residency_bricks WHERE volume_id = ? AND bx = ? AND by = ? AND bz = ?",
            params![volume_id as i64, coord[0], coord[1], coord[2]],
        )?;
        Ok(())
    }

    /// Reads back one record, or `None` if nothing is stored for that key
    /// (an `Unavailable` load, in `BrickBacking` terms).
    pub fn get(
        &self,
        volume_id: u64,
        coord: [i64; 3],
    ) -> Result<Option<ResidencyRecord>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let row: Option<(i64, bool, bool, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT revision, edited, known_empty, payload FROM residency_bricks \
                 WHERE volume_id = ? AND bx = ? AND by = ? AND bz = ?",
                params![volume_id as i64, coord[0], coord[1], coord[2]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok();
        let Some((revision, edited, known_empty, payload)) = row else {
            return Ok(None);
        };
        let revision = revision as u64;
        if known_empty {
            return Ok(Some(ResidencyRecord::KnownEmpty { revision, edited }));
        }
        let payload = dto::decode::<BrickPayload>(&payload.unwrap_or_default())?;
        Ok(Some(ResidencyRecord::Brick {
            revision,
            edited,
            payload,
        }))
    }

    /// A bounded snapshot of every record, `KnownEmpty` folded to
    /// `BrickPayload::Uniform(0)` -- for a checkpoint's evicted-brick fold,
    /// mirroring `spall_server::residency::ResidencyBacking::records`.
    pub fn records(&self) -> Result<Vec<crate::StoredBrick>, StoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT volume_id, bx, by, bz, revision, edited, known_empty, payload \
             FROM residency_bricks",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (volume_id, bx, by, bz, revision, edited, known_empty, payload) = row?;
            let payload = if known_empty {
                BrickPayload::Uniform(0)
            } else {
                dto::decode::<BrickPayload>(&payload.unwrap_or_default())?
            };
            out.push(crate::StoredBrick {
                volume_id: volume_id as u64,
                coord: [bx, by, bz],
                revision: revision as u64,
                edited,
                payload,
            });
        }
        Ok(out)
    }

    /// The database's current on-disk footprint in bytes (main file + WAL +
    /// shared-memory index, whichever exist) -- the durable-side counterpart
    /// to live resident RAM for T23/G3 row 7 item 3's total-retained-memory
    /// evidence.
    pub fn disk_bytes(&self) -> u64 {
        [
            self.path.clone(),
            append_ext(&self.path, "-wal"),
            append_ext(&self.path, "-shm"),
        ]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
    }
}

fn append_ext(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spall_residency_store_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("residency.db")
    }

    #[test]
    fn a_stored_brick_round_trips() {
        let db = tmp_db("round_trip");
        let store = ResidencyStore::open(&db).unwrap();
        let payload = BrickPayload::Uniform(7);
        store.put_brick(1, [2, -3, 4], 9, true, &payload).unwrap();
        let got = store.get(1, [2, -3, 4]).unwrap().unwrap();
        assert_eq!(
            got,
            ResidencyRecord::Brick {
                revision: 9,
                edited: true,
                payload,
            }
        );
        assert!(store.get(1, [0, 0, 0]).unwrap().is_none());
    }

    #[test]
    fn a_known_empty_record_round_trips_without_a_payload() {
        let db = tmp_db("known_empty");
        let store = ResidencyStore::open(&db).unwrap();
        store.put_known_empty(1, [0, 0, 0], 3, true).unwrap();
        assert_eq!(
            store.get(1, [0, 0, 0]).unwrap().unwrap(),
            ResidencyRecord::KnownEmpty {
                revision: 3,
                edited: true,
            }
        );
    }

    #[test]
    fn an_upsert_replaces_the_prior_record() {
        let db = tmp_db("upsert");
        let store = ResidencyStore::open(&db).unwrap();
        store
            .put_brick(1, [0, 0, 0], 1, false, &BrickPayload::Uniform(1))
            .unwrap();
        store
            .put_brick(1, [0, 0, 0], 2, true, &BrickPayload::Uniform(2))
            .unwrap();
        assert_eq!(
            store.get(1, [0, 0, 0]).unwrap().unwrap(),
            ResidencyRecord::Brick {
                revision: 2,
                edited: true,
                payload: BrickPayload::Uniform(2),
            }
        );
        assert_eq!(store.records().unwrap().len(), 1);
    }

    #[test]
    fn a_removed_record_is_unavailable_again() {
        let db = tmp_db("remove");
        let store = ResidencyStore::open(&db).unwrap();
        store
            .put_brick(1, [0, 0, 0], 1, false, &BrickPayload::Uniform(1))
            .unwrap();
        store.remove(1, [0, 0, 0]).unwrap();
        assert!(store.get(1, [0, 0, 0]).unwrap().is_none());
    }

    /// The property item 2 asks for directly: a brick captured to disk
    /// survives closing and reopening the store at the same path (a stand-in
    /// for a process restart -- `Connection` has no in-memory fallback here).
    #[test]
    fn a_captured_brick_survives_reopening_the_store() {
        let db = tmp_db("reopen");
        {
            let store = ResidencyStore::open(&db).unwrap();
            store
                .put_brick(5, [1, 2, 3], 42, true, &BrickPayload::Uniform(9))
                .unwrap();
        }
        let reopened = ResidencyStore::open(&db).unwrap();
        assert_eq!(
            reopened.get(5, [1, 2, 3]).unwrap().unwrap(),
            ResidencyRecord::Brick {
                revision: 42,
                edited: true,
                payload: BrickPayload::Uniform(9),
            }
        );
    }

    #[test]
    fn disk_bytes_is_nonzero_once_the_schema_and_a_row_exist() {
        let db = tmp_db("disk_bytes");
        let store = ResidencyStore::open(&db).unwrap();
        store
            .put_brick(1, [0, 0, 0], 1, false, &BrickPayload::Uniform(1))
            .unwrap();
        assert!(store.disk_bytes() > 0);
    }
}
