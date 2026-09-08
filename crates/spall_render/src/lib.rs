//! `spall_render` — wgpu resources and render passes for the meshed voxel
//! baseline. Depends on `spall_mesh` and `spall_core` only; window and input
//! integration stay in `spall_client`.
//!
//! The T05 scope is deliberately small: one opaque pipeline that rasterises
//! greedy-meshed surfaces with material colours, sun/sky/AO shading, a correct
//! depth buffer and counter-clockwise front faces, a free-fly [`Camera`] with
//! frustum culling, bounded reusable GPU mesh uploads, and an offscreen
//! [`capture_scene`] that writes a shaded PNG plus normal and depth debug
//! images and then returns. HDR material/light work is T12; indirect light is
//! T13.

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
pub use pipeline::{DebugView, ScenePipeline};
pub use scene::{Scene, SceneItem, default_palette};
pub use target::OffscreenTarget;
pub use upload::{GpuMesh, MeshUploader, UploadBudget};
pub use vertex::{GpuVertex, to_gpu};
