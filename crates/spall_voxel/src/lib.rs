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
//! modified-air tombstones.
//!
//! T03 adds read-side queries and brush plans on top of that storage:
//! [`query`] — a 3D DDA ray traversal over a volume, in local cell space or
//! against a rigidly [`transform`]ed volume; [`brush`] — deterministic box and
//! integer-sphere [`EditPlan`] generators; and [`fixtures`] — small,
//! digest-pinned terrain / structure volumes reused by later tasks. No server
//! loop drives any of this yet.

pub mod accounting;
pub mod brick;
pub mod brush;
pub mod edit;
pub mod fixtures;
pub mod logical;
pub mod query;
pub mod residency;
pub mod transform;
pub mod volume;

#[cfg(any(test, feature = "oracle"))]
pub mod oracle;

#[cfg(test)]
mod random_parity;

pub use accounting::MemoryReport;
pub use brick::{Brick, BrickHash, BrickSnapshot, DENSE_LAYER_BYTES, LayerKind};
pub use edit::{BrickRevisionRecord, CellEdit, EditError, EditOutcome, EditPlan};
pub use logical::{
    BrickDigest, DigestError, EvictedBricks, LogicalBrick, logical_bricks, logical_solid_cells,
};
pub use query::{
    Face, MissReason, Ray, RayConfig, RayError, RayHit, RayOutcome, cast_ray, cast_ray_world,
};
pub use residency::{
    BrickCacheKey, CacheBudget, CacheEntryState, CollisionAdmission, CollisionReadiness,
    InterestRadii, ResidencyCache, ResidencyPlan,
};
pub use transform::RigidXform;
pub use volume::{AccessError, BrickBounds, BrickState, Residency, Sample, Volume};
