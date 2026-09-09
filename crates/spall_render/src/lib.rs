//! `spall_render` — wgpu resources and render passes for the meshed voxel
//! baseline. Depends on `spall_mesh` and `spall_core` only; window and input
//! integration stay in `spall_client`.
//!
//! T12 extends the visible-voxel baseline with explicit cascaded-shadow, HDR
//! opaque and fixed-exposure tone-map passes. T13 adds a fixed 128-cubed
//! camera-local occupancy/material cache and one-bounce compute prototype.

pub mod camera;
pub mod capture;
pub mod context;
pub mod fixtures;
pub mod indirect;
pub mod pipeline;
pub mod scene;
pub mod target;
pub mod upload;
pub mod vertex;

pub use camera::{Aabb, Camera, Frustum};
pub use capture::{CaptureImage, CaptureOptions, CaptureReport, CaptureTiming, capture_scene};
pub use context::{RenderContext, RenderError};
pub use fixtures::{LightingFixture, LightingFixtureMetrics, colored_rooms};
pub use indirect::{LIGHT_CELL_SIZE_METRES, LIGHT_VOLUME_DIM, LightingVolume};
pub use pipeline::{CASCADE_COUNT, DebugView, PassTiming, ScenePipeline};
pub use scene::{Material, Scene, SceneItem, default_materials};
pub use target::OffscreenTarget;
pub use upload::{GpuMesh, MeshUploader, UploadBudget};
pub use vertex::{GpuVertex, to_gpu};
