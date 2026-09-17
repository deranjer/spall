//! T23 / G3 row 7, item 2 — a real disk-backed [`spall_sim::BrickBacking`].
//!
//! `spall_sim::MemoryBacking` (the default `ResidencyPass` has always used) is
//! purely in-process: evicted-brick reload records live only in RAM and are
//! gone the moment the server process exits. `docs/reports/G3.md`'s own note
//! on this: "not unsafe for crash recovery, since journal replay from a
//! checkpoint reconstructs the world independently of it, but not durable in
//! its own right." `DiskBrickBacking` closes exactly that gap: it is backed
//! by [`spall_store::ResidencyStore`] (a SQLite file, zstd-compressed
//! payloads — the same low-level convention `spall_store`'s checkpoint/journal
//! schema uses, in its own table and its own file, see that module's docs for
//! why it is not part of the versioned save schema), so a brick captured here
//! survives closing and reopening the store — i.e. a process restart.
//!
//! This does **not** change what a crash can lose: the checkpoint/journal
//! database (`spall_store::Writer`) remains the sole authoritative durability
//! contract `docs/protocol.md` describes. `DiskBrickBacking` only makes the
//! residency *cache itself* durable across an orderly restart, so a large
//! evicted world does not have to be captured all over again (from generation
//! or from a checkpoint reload) the next time the process starts. It
//! implements `spall_sim::BrickBackingWriter` — the same trait `MemoryBacking`
//! implements (T23/G3 row 7 item 1's unification point) — so `ResidencyPass`
//! is unchanged either way; only which backing `serve()` installs differs
//! (`ServeConfig::residency_disk_path`).

use std::path::Path;

use spall_core::{BrickCoord, CELLS_PER_BRICK, LocalCell, MaterialId, Revision, VolumeId};
use spall_sim::{BackingBrick, BrickBacking, BrickBackingWriter};
use spall_store::{ResidencyRecord, ResidencyStore, StoreError, decode_cells, encode_cells};
use spall_voxel::{Brick, Volume};

/// Anything that can go wrong opening or using the disk-backed residency
/// cache.
#[derive(Debug, thiserror::Error)]
pub enum DiskBackingError {
    #[error("residency store: {0}")]
    Store(#[from] StoreError),
}

/// A real disk-backed [`BrickBackingWriter`], one SQLite file per world
/// directory (conventionally `<world>/residency.db`, alongside
/// `<world>/world.db`). Interior-mutable (`ResidencyStore` locks its own
/// connection), so it can be shared behind an `Arc` exactly like
/// `MemoryBacking`.
pub struct DiskBrickBacking {
    store: ResidencyStore,
}

impl DiskBrickBacking {
    /// Opens (creating if absent) the disk-backed residency cache at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DiskBackingError> {
        Ok(Self {
            store: ResidencyStore::open(path)?,
        })
    }
}

impl BrickBacking for DiskBrickBacking {
    fn load(&self, volume: VolumeId, coord: BrickCoord) -> BackingBrick {
        let key = [coord.x, coord.y, coord.z];
        let record = match self.store.get(volume.get(), key) {
            Ok(Some(record)) => record,
            Ok(None) | Err(_) => return BackingBrick::Unavailable,
        };
        match record {
            ResidencyRecord::KnownEmpty { revision, edited } => BackingBrick::KnownEmpty {
                revision: Revision(revision),
                edited,
            },
            ResidencyRecord::Brick {
                revision,
                edited,
                payload,
            } => match decode_cells(&payload) {
                Ok(cells) => {
                    let materials: Vec<MaterialId> = cells.into_iter().map(MaterialId).collect();
                    BackingBrick::Loaded(Brick::restored(&materials, Revision(revision), edited))
                }
                Err(_) => BackingBrick::Unavailable,
            },
        }
    }
}

impl BrickBackingWriter for DiskBrickBacking {
    fn capture(&self, volume: &Volume, coord: BrickCoord) -> bool {
        let Ok(Some(snap)) = volume.snapshot_brick(coord) else {
            return false;
        };
        let mut cells = vec![0u16; CELLS_PER_BRICK];
        for (i, slot) in cells.iter_mut().enumerate() {
            let local = LocalCell::from_linear_index(i as u16).expect("i < CELLS_PER_BRICK");
            *slot = snap.get(local).raw();
        }
        let Ok(payload) = encode_cells(&cells) else {
            return false;
        };
        self.store
            .put_brick(
                volume.id().get(),
                [coord.x, coord.y, coord.z],
                snap.revision().get(),
                snap.is_edited(),
                &payload,
            )
            .is_ok()
    }

    fn disk_bytes(&self) -> Option<u64> {
        Some(self.store.disk_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, GlobalCell, VolumeId};
    use spall_voxel::Volume;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spall_disk_backing_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("residency.db")
    }

    fn vol_with_one_dense_brick() -> (Volume, BrickCoord) {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let coord = GlobalCell::new(0, 0, 0).split().0;
        let mut plan = spall_voxel::EditPlan::new(v.id());
        plan.set(GlobalCell::new(0, 0, 0), MaterialId(3));
        plan.set(GlobalCell::new(1, 0, 0), MaterialId(4));
        v.apply_edit(&plan).unwrap();
        (v, coord)
    }

    #[test]
    fn a_captured_brick_loads_back_identically() {
        let path = tmp_path("capture_load");
        let backing = DiskBrickBacking::open(&path).unwrap();
        let (volume, coord) = vol_with_one_dense_brick();
        assert!(backing.capture(&volume, coord));

        let expected = volume.snapshot_brick(coord).unwrap().unwrap();
        let BackingBrick::Loaded(loaded) = backing.load(volume.id(), coord) else {
            panic!("expected a loaded brick");
        };
        for i in 0..CELLS_PER_BRICK as u16 {
            let local = LocalCell::from_linear_index(i).unwrap();
            assert_eq!(loaded.get(local), expected.get(local));
        }
        assert_eq!(loaded.revision(), expected.revision());
        assert_eq!(loaded.is_edited(), expected.is_edited());
    }

    #[test]
    fn an_uncaptured_brick_is_unavailable() {
        let path = tmp_path("unavailable");
        let backing = DiskBrickBacking::open(&path).unwrap();
        let coord = GlobalCell::new(5, 5, 5).split().0;
        assert!(matches!(
            backing.load(VolumeId::new(1).unwrap(), coord),
            BackingBrick::Unavailable
        ));
    }

    /// The property item 2 asks for directly: a brick captured to disk
    /// survives closing and reopening the backing at the same path -- a
    /// stand-in for a process restart, since `ResidencyStore` keeps nothing
    /// in memory that isn't also on disk once `capture` returns `true`.
    #[test]
    fn a_captured_brick_survives_a_simulated_process_restart() {
        let path = tmp_path("restart");
        let (volume, coord) = vol_with_one_dense_brick();
        let expected = volume.snapshot_brick(coord).unwrap().unwrap();
        {
            let backing = DiskBrickBacking::open(&path).unwrap();
            assert!(backing.capture(&volume, coord));
        }
        // Fresh instance, same file: nothing carried over except what is on
        // disk.
        let reopened = DiskBrickBacking::open(&path).unwrap();
        let BackingBrick::Loaded(loaded) = reopened.load(volume.id(), coord) else {
            panic!("expected the reload to survive reopening the store");
        };
        for i in 0..CELLS_PER_BRICK as u16 {
            let local = LocalCell::from_linear_index(i).unwrap();
            assert_eq!(loaded.get(local), expected.get(local));
        }
        assert_eq!(loaded.revision(), expected.revision());
    }

    #[test]
    fn disk_bytes_is_reported_and_memory_backing_reports_none() {
        let path = tmp_path("disk_bytes");
        let backing = DiskBrickBacking::open(&path).unwrap();
        let (volume, coord) = vol_with_one_dense_brick();
        assert!(backing.capture(&volume, coord));
        assert!(backing.disk_bytes().unwrap() > 0);

        assert_eq!(spall_sim::MemoryBacking::default().disk_bytes(), None);
    }
}
