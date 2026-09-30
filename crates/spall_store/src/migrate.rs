//! Explicit copy migration. The source stays readable by its original binary.
use crate::{STORE_SCHEMA_VERSION, StoreError, StoredWorldMeta};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Copies a schema-1 save to a fresh destination, adds explicit empty water
/// checkpoint rows and stamps schema 2. Never edits or overwrites the source.
pub fn migrate_v1_water(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), StoreError> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    if destination.exists() {
        return Err(StoreError::Corrupt(
            "migration destination already exists".into(),
        ));
    }
    let source_conn = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version = crate::schema::read_user_version(&source_conn)?;
    if version != 1 {
        return Err(StoreError::Corrupt(format!(
            "water migration requires schema 1, found {version}"
        )));
    }
    source_conn.execute("VACUUM INTO ?", [destination.to_string_lossy().as_ref()])?;
    let mut conn = Connection::open_with_flags(destination, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let tx = conn.transaction()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS checkpoint_water (tick INTEGER PRIMARY KEY REFERENCES checkpoints(tick) ON DELETE CASCADE, state BLOB NOT NULL, crc BLOB NOT NULL);")?;
    let empty = crate::dto::encode(&Vec::<spall_protocol::WaterState>::new())?;
    let crc = crate::db::crc16(&empty);
    tx.execute(
        "INSERT OR IGNORE INTO checkpoint_water (tick,state,crc) SELECT tick,?,? FROM checkpoints",
        rusqlite::params![empty, crc.as_slice()],
    )?;
    let rows = {
        let mut stmt = tx.prepare("SELECT tick, meta FROM checkpoints")?;
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (tick, blob) in rows {
        let mut meta: StoredWorldMeta = crate::dto::decode(&blob)?;
        if meta.store_schema_version != 1 {
            return Err(StoreError::Corrupt(
                "migration checkpoint schema mismatch".into(),
            ));
        }
        meta.store_schema_version = STORE_SCHEMA_VERSION;
        tx.execute(
            "UPDATE checkpoints SET meta = ? WHERE tick = ?",
            rusqlite::params![crate::dto::encode(&meta)?, tick],
        )?;
    }
    let mut stmt = tx.prepare("SELECT meta FROM world WHERE id = 1")?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        let mut meta: StoredWorldMeta = crate::dto::decode(&row.get::<_, Vec<u8>>(0)?)?;
        meta.store_schema_version = STORE_SCHEMA_VERSION;
        tx.execute(
            "UPDATE world SET meta = ? WHERE id = 1",
            [crate::dto::encode(&meta)?],
        )?;
    }
    drop(rows);
    drop(stmt);
    tx.pragma_update(None, "user_version", STORE_SCHEMA_VERSION)?;
    tx.commit()?;
    drop(conn);
    crate::recover(destination)?;
    Ok(())
}
