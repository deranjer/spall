//! `spall_mesh` — surface generation for voxel volumes. No GPU, window, or
//! network dependency.
//!
//! Given a [`spall_voxel::Volume`] this crate turns the solid cells into a
//! triangle [`Mesh`] of their exposed faces:
//!
//! - [`culled`] emits one unit quad per exposed cell face. It is the reference
//!   surface — every other strategy must cover the exact same set of unit faces
//!   and therefore the exact same surface area.
//! - [`greedy`] merges coplanar exposed faces that share a material, a facing,
//!   and a full four-corner ambient-occlusion tuple into larger quads. The
//!   AO-aware merge rule is fixed *before* AO is baked into vertices so a merged
//!   quad's bilinear AO stays exactly equal to every unit face it replaced
//!   (see [`ao`]).
//! - [`build_volume_mesh`] meshes a whole volume, sampling a one-cell halo
//!   across brick seams for face culling and AO, and returns a
//!   [`spall_jobs::JobToken`] over the exact brick revisions and
//!   missing-neighbour sentinels it read. When a halo brick later arrives the
//!   token goes stale and the caller re-meshes.
//!
//! Faces are emitted in a canonical order ([`FaceDir`] then plane then `v` then
//! `u`) so a mesh is a deterministic function of the volume contents;
//! [`mesh_digest`] pins that for tests.
//!
//! Everything here is all-resident G1 behaviour: the whole volume is meshed at
//! once. Per-brick chunk meshing and streamed halo loading are a later
//! optimisation (T18).

pub mod ao;
pub mod build;
pub mod culled;
pub mod enumerate;
pub mod face;
pub mod fixtures;
pub mod greedy;
pub mod mesh;
pub mod sample;

pub use ao::ao_factor;
pub use build::{MeshOptions, VolumeMesh, build_volume_mesh};
pub use culled::emit_culled;
pub use enumerate::{ExposedFace, for_each_exposed_face};
pub use face::{FACE_DIRS, FaceDir};
pub use greedy::emit_greedy;
pub use mesh::{FaceQuad, Mesh, MeshStats, MeshStrategy, Vertex, mesh_digest, mesh_digest_hex};
pub use sample::{CellBox, Occupancy, VolumeSampler};
