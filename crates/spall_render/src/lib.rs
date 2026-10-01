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
pub mod environment;
pub mod fixtures;
pub mod game_renderer;
pub mod indirect;
pub mod instances;
pub mod pipeline;
pub(crate) mod probe;
pub mod scene;
pub mod sky_visibility;
pub mod target;
pub mod upload;
pub mod vertex;
pub mod viewport;
pub mod water;

pub use camera::{Aabb, Camera, Frustum};
pub use capture::{
    BandTrace, CaptureImage, CaptureOptions, CaptureReport, CaptureTiming, CollapseFrame,
    CollapseSequenceOptions, CollapseSequenceReport, FrameLoopOptions, FrameLoopReport,
    FrameSeriesOptions, FrameSeriesPasses, FrameSeriesReport, FrameStats, LightingStep,
    MotionFrame, MotionImage, MotionSequenceOptions, MotionSequenceReport, ProbeBand,
    SequenceOptions, SequenceReport, SequenceStep, capture_collapse_sequence, capture_frame_loop,
    capture_frame_series, capture_lighting_sequence, capture_motion_sequence, capture_scene,
    flicker_index, max_step_fraction, settle_index,
};
pub use context::{RenderContext, RenderError};
pub use environment::{Environment, EnvironmentPreset};
pub use fixtures::{
    DaylightTerrainScene, EmitterOcclusionScenes, LightingFixture, LightingFixtureMetrics,
    MovingBodyOverlap, OCCLUDER_MAX_M, OCCLUDER_MIN_M, PanningCamera, RECEIVER_MAX_M,
    RECEIVER_MIN_M, RapidDestruction, colored_rooms, daylight_terrain_scene,
    emitter_occlusion_scenes, moving_body_overlap, panning_camera, rapid_destruction,
};
pub use game_renderer::{GamePassTimings, GameRenderer};
pub use indirect::{
    LIGHT_CELL_SIZE_METRES, LIGHT_VOLUME_DIM, LightingRegion, LightingUpdate, LightingVolume,
};
pub use instances::{CubeInstance, CubeVertex, InstanceSet};
pub use pipeline::{
    CASCADE_COUNT, DebugView, MAX_POINT_LIGHTS, PassTiming, PointLight, ScenePipeline,
};
pub use scene::{Material, Scene, SceneItem, default_materials, materials_from_manifest};
pub use sky_visibility::{
    SKY_MAX_RAY_CELLS, SkyVisibility, UNKNOWN_CELL, cache_origin_around, mark_world_box,
};
pub use target::OffscreenTarget;
pub use upload::{GpuMesh, MeshUploader, UploadBudget};
pub use vertex::{GpuVertex, to_gpu};
pub use viewport::{MeshChunk, ViewportFrame, ViewportRenderer};
pub use water::{NO_WATER, WaterField};
