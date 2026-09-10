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
pub use capture::{
    BandTrace, CaptureImage, CaptureOptions, CaptureReport, CaptureTiming, FrameLoopOptions,
    FrameLoopReport, FrameSeriesOptions, FrameSeriesPasses, FrameSeriesReport, FrameStats,
    LightingStep, MotionFrame, MotionImage, MotionSequenceOptions, MotionSequenceReport, ProbeBand,
    SequenceOptions, SequenceReport, SequenceStep, capture_frame_loop, capture_frame_series,
    capture_lighting_sequence, capture_motion_sequence, capture_scene, flicker_index,
    max_step_fraction, settle_index,
};
pub use context::{RenderContext, RenderError};
pub use fixtures::{
    EmitterOcclusionScenes, LightingFixture, LightingFixtureMetrics, MovingBodyOverlap,
    OCCLUDER_MAX_M, OCCLUDER_MIN_M, PanningCamera, RECEIVER_MAX_M, RECEIVER_MIN_M,
    RapidDestruction, colored_rooms, emitter_occlusion_scenes, moving_body_overlap, panning_camera,
    rapid_destruction,
};
pub use indirect::{
    LIGHT_CELL_SIZE_METRES, LIGHT_VOLUME_DIM, LightingRegion, LightingUpdate, LightingVolume,
};
pub use pipeline::{CASCADE_COUNT, DebugView, PassTiming, ScenePipeline};
pub use scene::{Material, Scene, SceneItem, default_materials};
pub use target::OffscreenTarget;
pub use upload::{GpuMesh, MeshUploader, UploadBudget};
pub use vertex::{GpuVertex, to_gpu};
