//! `spall_voxel` — sparse voxel storage: bricks, volumes, copy-on-write edits,
//! and revisioned authoritative content.
//!
//! No GPU, window, or network dependency. A brick holds authoritative per-cell
//! layers as `Uniform` metadata or a `Dense` reference-counted array; immutable
//! [`brick::BrickSnapshot`]s share the payload and stay fixed while the live
//! brick is edited. A [`volume::Volume`] is a sparse map of bricks with a
//! `VolumeId`, a fixed [`spall_core::CellSizeCode`], and optional brick bounds;
//! sampling keeps *absent*, *failed*, *empty*, and *filled* distinct.
//! [`edit::EditPlan`] / [`volume::Volume::apply_edit`] provide transactional
//! copy-on-write edits with before/after revision and content-hash records and
//! modified-air tombstones. T02 is the storage mechanism only — no server loop
//! drives it yet.

pub mod accounting;
pub mod brick;
pub mod edit;
pub mod volume;

#[cfg(any(test, feature = "oracle"))]
pub mod oracle;

#[cfg(test)]
mod random_parity;

pub use accounting::MemoryReport;
pub use brick::{Brick, BrickHash, BrickSnapshot, DENSE_LAYER_BYTES, LayerKind};
pub use edit::{BrickRevisionRecord, CellEdit, EditError, EditOutcome, EditPlan};
pub use volume::{AccessError, BrickBounds, BrickState, Residency, Sample, Volume};
