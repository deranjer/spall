//! Foundation types shared across the engine: integer coordinates, stable ids
//! and monotonic counters, materials and the world material manifest, fixed
//! cell sizes and brush/pose units, plus the T00 process-lifecycle records.
//!
//! Everything here is data and pure logic. No GPU, window, network, or
//! filesystem dependency belongs in this crate beyond the JSONL process log
//! from T00.

pub mod coord;
pub mod ids;
pub mod material;
pub mod units;

pub use coord::{BRICK_EDGE, BrickCoord, CELLS_PER_BRICK, CoordOverflow, GlobalCell, LocalCell};
pub use ids::{
    EntityId, IdAllocator, IdError, JournalSeq, Revision, Tick, TransactionId, VolumeId, WorldId,
};
pub use material::{
    MAX_MATERIALS, ManifestError, MaterialDef, MaterialFlags, MaterialId, MaterialManifest,
    RenderProps, SimProps,
};
pub use units::{
    BRUSH_FRACTION_BITS, BRUSH_UNIT, BrushError, BrushPoint, CellSizeCode, MAX_BRUSH_RADIUS_CELLS,
    NonFinitePose, Pose, QUAT_SCALE, QuantizedQuat, SphereBrush, ZeroQuaternion,
};

use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const PROCESS_METADATA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub version: u32,
    pub event: ProcessEvent,
    pub role: ProcessRole,
    pub pid: u32,
    pub unix_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEvent {
    Started,
    Ready,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessRole {
    Server,
    Client,
    Xtask,
}

impl ProcessRecord {
    pub fn new(event: ProcessEvent, role: ProcessRole, detail: Option<String>) -> Self {
        Self {
            version: PROCESS_METADATA_VERSION,
            event,
            role,
            pid: std::process::id(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock precedes Unix epoch")
                .as_millis(),
            detail,
        }
    }
}

#[derive(Debug, Error)]
pub enum JsonlError {
    #[error("cannot create JSONL log {path}: {source}")]
    Create { path: String, source: io::Error },
    #[error("cannot write JSONL log: {0}")]
    Write(#[from] io::Error),
    #[error("cannot serialize process record: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub struct JsonlLog {
    file: File,
}

impl JsonlLog {
    pub fn create(path: &Path) -> Result<Self, JsonlError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| JsonlError::Create {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|source| JsonlError::Create {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Self { file })
    }

    pub fn write(&mut self, record: &ProcessRecord) -> Result<(), JsonlError> {
        serde_json::to_writer(&mut self.file, record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

pub fn log_has_ready_record(path: &Path, role: ProcessRole, expected_pid: u32) -> io::Result<bool> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    Ok(source.lines().any(|line| {
        serde_json::from_str::<ProcessRecord>(line).is_ok_and(|record| {
            record.event == ProcessEvent::Ready && record.role == role && record.pid == expected_pid
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_record_round_trips_and_readiness_is_role_specific() {
        let path =
            std::env::temp_dir().join(format!("spall-core-{}-ready.jsonl", std::process::id()));
        let mut log = JsonlLog::create(&path).unwrap();
        log.write(&ProcessRecord::new(
            ProcessEvent::Ready,
            ProcessRole::Server,
            None,
        ))
        .unwrap();
        assert!(log_has_ready_record(&path, ProcessRole::Server, std::process::id()).unwrap());
        assert!(!log_has_ready_record(&path, ProcessRole::Client, std::process::id()).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn bare_log_filename_does_not_try_to_create_an_empty_directory() {
        let path = Path::new("spall-core-bare-filename-test.jsonl");
        let mut log = JsonlLog::create(path).unwrap();
        log.write(&ProcessRecord::new(
            ProcessEvent::Started,
            ProcessRole::Server,
            None,
        ))
        .unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_creation_reports_a_non_directory_parent() {
        let parent =
            std::env::temp_dir().join(format!("spall-core-log-parent-{}", std::process::id()));
        std::fs::write(&parent, b"not a directory").unwrap();
        assert!(matches!(
            JsonlLog::create(&parent.join("server.jsonl")),
            Err(JsonlError::Create { .. })
        ));
        std::fs::remove_file(parent).unwrap();
    }
}
