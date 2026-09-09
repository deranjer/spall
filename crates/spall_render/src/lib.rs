//! `spall_render` — wgpu resources and render passes for the meshed voxel
//! baseline. Depends on `spall_mesh` and `spall_core` only; window and input
//! integration stay in `spall_client`.
//!
//! T12 extends the visible-voxel baseline with explicit cascaded-shadow, HDR
//! opaque and fixed-exposure tone-map passes. Indirect light remains T13.

pub mod camera;
pub mod capture;
pub mod context;
pub mod pipeline;
pub mod scene;
pub mod target;
pub mod upload;
pub mod vertex;

pub use camera::{Aabb, Camera, Frustum};
pub use capture::{CaptureImage, CaptureOptions, CaptureReport, CaptureTiming, capture_scene};
pub use context::{RenderContext, RenderError};
pub use pipeline::{CASCADE_COUNT, DebugView, PassTiming, ScenePipeline};
pub use scene::{Material, Scene, SceneItem, default_materials};
pub use target::OffscreenTarget;
pub use upload::{GpuMesh, MeshUploader, UploadBudget};
pub use vertex::{GpuVertex, to_gpu};
