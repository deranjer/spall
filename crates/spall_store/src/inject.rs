//! Test-only on-disk corruption injectors for the persistence crash suite
//! (`spall_server::persist::run_crash_suite` / `cargo xtask crash-test`).
//!
//! These mutate a **closed** database file directly — the caller must have
//! dropped its [`Writer`](crate::Writer) first — to model corruption that a
//! crash, a bad sector, or bit-rot can leave in the journal.
//! [`recover`](crate::recover) then has to report it and stop the durable
//! prefix at the last clean record.
//!
//! Not `#[cfg(test)]`: the crash suite that exercises the recovery contract is a
//! normal binary (`crash-bench`), the same reason
//! [`FaultPlan`](crate::FaultPlan) is public.

use std::path::Path;

use rusqlite::Connection;

use crate::StoreError;

/// Overwrite the stored CRC of journal row `seq` with a value that cannot match
/// its payload. [`recover`](crate::recover) stops replay *before* this record
/// and reports a CRC failure, so the durable prefix ends at `seq - 1`.
pub fn break_journal_crc(path: &Path, seq: u64) -> Result<(), StoreError> {
    let conn = Connection::open(path)?;
    let changed = conn.execute(
        "UPDATE journal SET crc = X'00' WHERE seq = ?1",
        [seq as i64],
    )?;
    if changed != 1 {
        return Err(StoreError::Corrupt(format!(
            "cannot corrupt journal seq {seq}: row not present"
        )));
    }
    Ok(())
}

/// Delete journal row `seq`, leaving an interior gap. [`recover`](crate::recover)
/// stops at `seq - 1` and reports the missing sequence.
pub fn remove_journal_row(path: &Path, seq: u64) -> Result<(), StoreError> {
    let conn = Connection::open(path)?;
    let changed = conn.execute("DELETE FROM journal WHERE seq = ?1", [seq as i64])?;
    if changed != 1 {
        return Err(StoreError::Corrupt(format!(
            "cannot remove journal seq {seq}: row not present"
        )));
    }
    Ok(())
}
