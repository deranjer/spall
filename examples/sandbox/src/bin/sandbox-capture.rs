//! Offscreen renderer capture for the T05/T12 baseline and T13 lighting fixture.
//!
//! Meshes each acceptance shape (`spall_mesh::fixtures`), renders it to a
//! shaded PNG plus normal and depth debug images with `spall_render`, writes a
//! `summary.json`, and exits. Exit codes: `0` pass, `1` failure, `2` bad
//! arguments, `3` no GPU/capture capability.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use glam::{Mat4, Quat, Vec3};
use serde::Serialize;
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, SphereBrush};
use spall_jobs::{Generation, TopologyEpoch};
use spall_mesh::fixtures::{AcceptanceShape, acceptance_shapes, mesh_shape};
use spall_mesh::{MeshOptions, MeshStrategy, build_volume_mesh};
use spall_render::{
    Camera, CaptureOptions, CollapseFrame, CollapseSequenceOptions, DebugView, FrameLoopOptions,
    FrameSeriesOptions, FrameStats, LIGHT_CELL_SIZE_METRES, LightingRegion, LightingStep,
    LightingUpdate, LightingVolume, MotionFrame, MotionSequenceOptions, OCCLUDER_MAX_M,
    OCCLUDER_MIN_M, ProbeBand, RECEIVER_MAX_M, RECEIVER_MIN_M, RenderContext, RenderError, Scene,
    SceneItem, SequenceOptions, capture_collapse_sequence, capture_frame_loop,
    capture_frame_series, capture_lighting_sequence, capture_motion_sequence, capture_scene,
    colored_rooms, daylight_terrain_scene, emitter_occlusion_scenes, flicker_index,
    rapid_destruction,
};
use spall_sim::{Body, EditIntent, EditTarget, RequestId, Simulation, SimulationConfig, fixtures};
use spall_voxel::Sample;
use spall_voxel::Volume;

/// Nominal frame time used to turn `lighting-sequence` frame counts into
/// milliseconds — the provisional G2 client-frame target.
const NOMINAL_FRAME_MS: f64 = 16.7;
/// Band-luminance tolerance (fraction) for calling the lighting settled.
const CONVERGE_TOLERANCE: f32 = 0.05;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Strategy {
    Greedy,
    Culled,
}

impl From<Strategy> for MeshStrategy {
    fn from(value: Strategy) -> Self {
        match value {
            Strategy::Greedy => MeshStrategy::Greedy,
            Strategy::Culled => MeshStrategy::Culled,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "sandbox-capture",
    about = "Spall T05 offscreen renderer capture"
)]
struct Args {
    /// Output directory. One subdirectory per shape plus a summary.json.
    #[arg(long, default_value = ".local/runs/t05-capture")]
    out: PathBuf,
    #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(16..=7680))]
    width: u32,
    #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(16..=4320))]
    height: u32,
    #[arg(long, value_enum, default_value_t = Strategy::Greedy)]
    strategy: Strategy,
    /// Render only this shape (by name). Omit to render every acceptance shape.
    #[arg(long)]
    only: Option<String>,
    /// Fixture scene: `colored-room` (T13), `lighting-sequence` (T14),
    /// `g2-frames` (T15 cold GPU frame-cost percentiles), `g2-loop` (T15
    /// persistent-resource settled-frame GPU + CPU percentiles), `g2-motion`
    /// (T15 moving-frame sequences + ghosting / flicker / leakage flags),
    /// `g2-terrain` (T15 open daylight-terrain settled cost + stability),
    /// `g2-collapse` (T15 GI-lit rapid-destruction sequence, cold per-tick
    /// re-trace), or `g2-bounded-collapse` (T15 increment 6: the same collapse
    /// on the persistent loop with a bounded per-tick `LightingUpdate`).
    #[arg(long)]
    scene: Option<String>,
    /// `lighting-sequence` only: settle frames rendered after the edit so the
    /// temporal history's convergence latency can be measured.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=120))]
    settle_frames: u32,
    /// `g2-frames` only: frames rendered and discarded before measurement.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(0..=240))]
    g2_warmup_frames: u32,
    /// `g2-frames` only: frames folded into the per-pass percentiles.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=600))]
    g2_measured_frames: u32,
    /// `g2-loop` only: full-retrace frames rendered and discarded before measurement.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u32).range(0..=240))]
    g2_loop_warmup: u32,
    /// `g2-loop` only: measured frames per scene per re-trace mode.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u32).range(1..=600))]
    g2_loop_frames: u32,
    /// `g2-loop` only: cache-cell edge of the per-frame incremental re-trace box.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..=128))]
    g2_loop_edit_edge: u32,
    /// `g2-motion` only: consecutive frames per sequence (>= 120 per the gate).
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u32).range(24..=600))]
    g2_motion_frames: u32,
    /// `g2-bounded-collapse` only: consecutive collapse ticks rendered on the
    /// persistent loop (one frame per tick; >= 120 per the gate).
    #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(120..=600))]
    g2_bounded_collapse_ticks: u64,
}

#[derive(Serialize)]
struct ShapeSummary {
    name: String,
    strategy: String,
    quad_count: usize,
    triangle_count: usize,
    vertex_count: usize,
    surface_area_m2: f64,
    exposed_unit_faces: u64,
    unresolved_halo_faces: u64,
    items_drawn: usize,
    triangles_rasterised: u64,
    vertex_bytes: u64,
    index_bytes: u64,
    /// Real GPU render-pass time from timestamp queries, milliseconds. `null`
    /// when the adapter/driver does not support them (see `gpu_timing_available`).
    /// Never a CPU-derived figure.
    gpu_render_millis: Option<f64>,
    gpu_shadow_millis: Option<f64>,
    gpu_indirect_trace_millis: Option<f64>,
    gpu_indirect_denoise_millis: Option<f64>,
    gpu_opaque_millis: Option<f64>,
    gpu_tone_map_millis: Option<f64>,
    /// `true` only when `gpu_render_millis` is a measured device timing.
    gpu_timing_available: bool,
    /// CPU wall-clock for the whole render → readback → PNG-encode loop,
    /// milliseconds. This is NOT GPU time.
    cpu_capture_millis: f64,
    cpu_lighting_upload_millis: f64,
    /// CPU wall-clock inside GPU→CPU readback (map wait + row unpad), ms.
    cpu_readback_millis: f64,
    /// CPU wall-clock inside PNG compression and file writes, ms.
    cpu_encode_millis: f64,
    indirect_cells: usize,
    open_probe_luminance: Option<f32>,
    closed_probe_luminance: Option<f32>,
    thin_wall_leakage_ratio: Option<f32>,
    images: Vec<String>,
}

#[derive(Serialize)]
struct Summary {
    /// Version 4 adds T13 cache/probe data and trace/denoise timing.
    version: u32,
    adapter: String,
    backend: String,
    /// `true` when the shapes carry a measured GPU render-pass timing;
    /// `false` means GPU timing was unavailable on this adapter.
    gpu_timing_available: bool,
    width: u32,
    height: u32,
    strategy: String,
    shapes: Vec<ShapeSummary>,
}

fn view_dir() -> Vec3 {
    // A 3/4 view from front-right-above.
    Vec3::new(0.8, 0.55, 1.0)
}

/// Viewport aspect (width / height) for the requested capture size. The render
/// target is filled with whatever `--width`/`--height` ask for, so the camera
/// projection has to match or the PNG is geometrically distorted.
fn capture_aspect(width: u32, height: u32) -> f32 {
    width.max(1) as f32 / height.max(1) as f32
}

fn build_scene(
    shape: &AcceptanceShape,
    strategy: MeshStrategy,
    aspect: f32,
) -> (Scene, spall_mesh::MeshStats) {
    let vm = mesh_shape(&shape.volume, strategy);
    let stats = vm.stats;

    // Rotate about the mesh centre so the framed view stays centred. The one
    // rotated shape spins about an oblique axis so the tumble is obvious.
    let centre = mesh_centre(&vm.mesh);
    let rot = if shape.yaw != 0.0 {
        Mat4::from_axis_angle(Vec3::new(0.22, 1.0, 0.12).normalize(), shape.yaw)
    } else {
        Mat4::IDENTITY
    };
    let model = Mat4::from_translation(centre) * rot * Mat4::from_translation(-centre);

    let mut scene = Scene::new(spall_render::Camera {
        aspect,
        fov_y: 55_f32.to_radians(),
        ..Default::default()
    });
    scene = scene.with_item(SceneItem::new(shape.name, vm.mesh, model));
    scene.frame_all(view_dir());
    (scene, stats)
}

fn mesh_centre(mesh: &spall_mesh::Mesh) -> Vec3 {
    if mesh.vertices.is_empty() {
        return Vec3::ZERO;
    }
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for v in &mesh.vertices {
        let p = Vec3::from_array(v.position);
        min = min.min(p);
        max = max.max(p);
    }
    (min + max) * 0.5
}

fn run(args: &Args) -> Result<Summary, RenderError> {
    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();

    if let Some(scene) = &args.scene {
        if args.only.is_some() {
            return Err(RenderError::Gpu(
                "--scene and --only are mutually exclusive".into(),
            ));
        }
        // `lighting-sequence` and `destruction` have their own summary shapes
        // and are handled in main.
        if scene != "colored-room" {
            return Err(RenderError::Gpu(format!(
                "no fixture scene named {scene:?}"
            )));
        }
        return run_colored_room(args, &ctx, strategy);
    }

    let mut shapes = acceptance_shapes();
    if let Some(only) = &args.only {
        shapes.retain(|s| s.name == only);
        if shapes.is_empty() {
            return Err(RenderError::Gpu(format!(
                "no acceptance shape named {only:?}"
            )));
        }
    }

    let opts = CaptureOptions {
        width: args.width,
        height: args.height,
        ..Default::default()
    };

    let aspect = capture_aspect(args.width, args.height);
    let mut summaries = Vec::new();
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut gpu_timing_available = false;
    for shape in &shapes {
        let (scene, stats) = build_scene(shape, strategy, aspect);
        let out_dir = args.out.join(shape.name);
        let report = capture_scene(&ctx, &scene, &out_dir, &opts)?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();
        let timing = report.timing;
        gpu_timing_available |= timing.gpu_render_millis.is_some();

        summaries.push(ShapeSummary {
            name: shape.name.to_string(),
            strategy: format!("{strategy:?}"),
            quad_count: stats.quad_count,
            triangle_count: stats.triangle_count,
            vertex_count: stats.vertex_count,
            surface_area_m2: stats.surface_area_m2,
            exposed_unit_faces: stats.exposed_unit_faces,
            unresolved_halo_faces: stats.unresolved_halo_faces,
            items_drawn: report.items_drawn,
            triangles_rasterised: report.triangles,
            vertex_bytes: report.vertex_bytes,
            index_bytes: report.index_bytes,
            gpu_render_millis: timing.gpu_render_millis,
            gpu_shadow_millis: timing.gpu_passes.map(|passes| passes.shadow_millis),
            gpu_indirect_trace_millis: timing.gpu_passes.map(|passes| passes.indirect_trace_millis),
            gpu_indirect_denoise_millis: timing
                .gpu_passes
                .map(|passes| passes.indirect_denoise_millis),
            gpu_opaque_millis: timing.gpu_passes.map(|passes| passes.opaque_millis),
            gpu_tone_map_millis: timing.gpu_passes.map(|passes| passes.tone_map_millis),
            gpu_timing_available: timing.gpu_render_millis.is_some(),
            cpu_capture_millis: timing.cpu_total_millis,
            cpu_lighting_upload_millis: timing.cpu_lighting_upload_millis,
            cpu_readback_millis: timing.cpu_readback_millis,
            cpu_encode_millis: timing.cpu_encode_millis,
            indirect_cells: report.indirect_cells,
            open_probe_luminance: None,
            closed_probe_luminance: None,
            thin_wall_leakage_ratio: None,
            images: report
                .images
                .iter()
                .map(|i| i.path.display().to_string())
                .collect(),
        });
    }

    Ok(Summary {
        version: 4,
        adapter,
        backend,
        gpu_timing_available,
        width: args.width,
        height: args.height,
        strategy: format!("{strategy:?}"),
        shapes: summaries,
    })
}

fn run_colored_room(
    args: &Args,
    ctx: &RenderContext,
    strategy: MeshStrategy,
) -> Result<Summary, RenderError> {
    let opts = CaptureOptions {
        width: args.width,
        height: args.height,
        views: vec![DebugView::Shaded, DebugView::IndirectOnly],
        ..Default::default()
    };
    let mut summaries = Vec::new();
    let mut adapter = String::new();
    let mut backend = String::new();
    let mut gpu_timing_available = false;
    let fixtures = colored_rooms(capture_aspect(args.width, args.height));
    let metrics = fixtures[0].metrics;
    if metrics.closed_probe_luminance >= metrics.open_probe_luminance {
        return Err(RenderError::Gpu(format!(
            "closed-room probe is not darker: closed={} open={}",
            metrics.closed_probe_luminance, metrics.open_probe_luminance
        )));
    }
    if metrics.thin_wall_leakage_ratio > 0.05 {
        return Err(RenderError::Gpu(format!(
            "represented thin-wall leakage {} exceeds 0.05",
            metrics.thin_wall_leakage_ratio
        )));
    }
    for fixture in fixtures {
        let report = capture_scene(ctx, &fixture.scene, &args.out.join(fixture.name), &opts)?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();
        let timing = report.timing;
        gpu_timing_available |= timing.gpu_render_millis.is_some();
        let passes = timing.gpu_passes;
        summaries.push(ShapeSummary {
            name: fixture.name.to_owned(),
            strategy: format!("{strategy:?}"),
            quad_count: fixture.mesh_stats.quad_count,
            triangle_count: fixture.mesh_stats.triangle_count,
            vertex_count: fixture.mesh_stats.vertex_count,
            surface_area_m2: fixture.mesh_stats.surface_area_m2,
            exposed_unit_faces: fixture.mesh_stats.exposed_unit_faces,
            unresolved_halo_faces: fixture.mesh_stats.unresolved_halo_faces,
            items_drawn: report.items_drawn,
            triangles_rasterised: report.triangles,
            vertex_bytes: report.vertex_bytes,
            index_bytes: report.index_bytes,
            gpu_render_millis: timing.gpu_render_millis,
            gpu_shadow_millis: passes.map(|p| p.shadow_millis),
            gpu_indirect_trace_millis: passes.map(|p| p.indirect_trace_millis),
            gpu_indirect_denoise_millis: passes.map(|p| p.indirect_denoise_millis),
            gpu_opaque_millis: passes.map(|p| p.opaque_millis),
            gpu_tone_map_millis: passes.map(|p| p.tone_map_millis),
            gpu_timing_available: timing.gpu_render_millis.is_some(),
            cpu_capture_millis: timing.cpu_total_millis,
            cpu_lighting_upload_millis: timing.cpu_lighting_upload_millis,
            cpu_readback_millis: timing.cpu_readback_millis,
            cpu_encode_millis: timing.cpu_encode_millis,
            indirect_cells: report.indirect_cells,
            open_probe_luminance: Some(fixture.metrics.open_probe_luminance),
            closed_probe_luminance: Some(fixture.metrics.closed_probe_luminance),
            thin_wall_leakage_ratio: Some(fixture.metrics.thin_wall_leakage_ratio),
            images: report
                .images
                .iter()
                .map(|image| image.path.display().to_string())
                .collect(),
        });
    }
    Ok(Summary {
        version: 4,
        adapter,
        backend,
        gpu_timing_available,
        width: args.width,
        height: args.height,
        strategy: format!("{strategy:?}"),
        shapes: summaries,
    })
}

#[derive(Serialize)]
struct LightingSequenceStep {
    index: usize,
    label: String,
    dirty_cells: usize,
    retraced_cells: u64,
    band_luminance: f32,
    gpu_trace_millis: Option<f64>,
    gpu_denoise_millis: Option<f64>,
    gpu_temporal_millis: Option<f64>,
    image: String,
}

/// T14 `lighting-sequence` evidence: the `rapid_destruction` edit followed by
/// settle frames, with the temporal accumulation on so convergence latency is
/// meaningful.
#[derive(Serialize)]
struct LightingSequenceSummary {
    version: u32,
    scene: &'static str,
    adapter: String,
    backend: String,
    gpu_timing_available: bool,
    width: u32,
    height: u32,
    total_cells: u64,
    band: [f32; 2],
    halo_cells: u32,
    temporal_weight: f32,
    /// Frames from the edit to the first frame that reflects it — always 1 with
    /// the one-frame-per-step model.
    edit_latency_frames: u32,
    edit_latency_millis: f64,
    /// Frames from the edit until the measured band stays within
    /// `converge_tolerance` of its final value; `null` if it never settled in
    /// the captured run.
    converge_frames: Option<u32>,
    converge_millis: Option<f64>,
    converge_tolerance: f32,
    nominal_frame_millis: f64,
    steps: Vec<LightingSequenceStep>,
}

fn run_lighting_sequence(args: &Args) -> Result<LightingSequenceSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let aspect = capture_aspect(args.width, args.height);
    let rd = rapid_destruction(aspect);
    let camera = rd.scene.camera;
    let band = rd.receiver_band;

    let mut steps = Vec::with_capacity(1 + args.settle_frames as usize);
    steps.push(LightingStep::edit("remove occluder", rd.remove_occluder));
    for frame in 1..=args.settle_frames {
        steps.push(LightingStep::view(format!("settle {frame}"), camera));
    }

    let report = capture_lighting_sequence(
        &ctx,
        rd.scene,
        SequenceOptions {
            band,
            temporal_weight: 0.1,
            width: args.width,
            height: args.height,
            ..Default::default()
        },
        &steps,
        &args.out,
    )?;

    // Step 0 is the base (occluder present); step 1 is the edit; the rest are
    // settle frames. Convergence is measured against the final captured frame.
    let post_edit: Vec<f32> = report
        .steps
        .iter()
        .skip(1)
        .map(|step| step.band_luminance)
        .collect();
    let final_band = post_edit.last().copied().unwrap_or(0.0);
    let tolerance = CONVERGE_TOLERANCE * final_band.abs().max(1.0);
    let converge_frames = (1..=post_edit.len()).find(|&k| {
        post_edit[k - 1..]
            .iter()
            .all(|value| (value - final_band).abs() <= tolerance)
    });

    let gpu_timing_available = report
        .steps
        .iter()
        .any(|step| step.gpu_trace_millis.is_some());

    Ok(LightingSequenceSummary {
        version: 1,
        scene: "lighting-sequence",
        adapter: report.adapter.clone(),
        backend: report.backend.clone(),
        gpu_timing_available,
        width: report.width,
        height: report.height,
        total_cells: report.total_cells,
        band: report.band,
        halo_cells: report.halo_cells,
        temporal_weight: report.temporal_weight,
        edit_latency_frames: 1,
        edit_latency_millis: NOMINAL_FRAME_MS,
        converge_frames: converge_frames.map(|k| k as u32),
        converge_millis: converge_frames.map(|k| k as f64 * NOMINAL_FRAME_MS),
        converge_tolerance: CONVERGE_TOLERANCE,
        nominal_frame_millis: NOMINAL_FRAME_MS,
        steps: report
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| LightingSequenceStep {
                index,
                label: step.label.clone(),
                dirty_cells: step.dirty_cells,
                retraced_cells: step.retraced_cells,
                band_luminance: step.band_luminance,
                gpu_trace_millis: step.gpu_trace_millis,
                gpu_denoise_millis: step.gpu_denoise_millis,
                gpu_temporal_millis: step.gpu_temporal_millis,
                image: step.indirect_image.display().to_string(),
            })
            .collect(),
    })
}

// --- `--scene destruction` (T11a / ENG-62 increment 2) ----------------------

/// A 3/4 view from front-left-above, aimed to keep the cross-brick bridge scene
/// and the falling beam in frame.
fn destruction_view_dir() -> Vec3 {
    Vec3::new(-0.85, 0.5, 1.0)
}

/// The `g1-networked-destruction` cut script (`fixtures/scenarios/`), as
/// `(at_tick, [x, y, z] cell, radius_cells)`. Every entry is driven against
/// terrain here — the authoritative body-recut nuance is covered by the
/// networked fixture; this run exists for the *visual* destruction evidence.
const DESTRUCTION_SCRIPT: [(u64, [i64; 3], i64); 10] = [
    (4, [31, 4, 1], 3),
    (10, [21, 1, 1], 1),
    (16, [32, 4, 2], 3),
    (24, [43, 1, 1], 1),
    (32, [24, 1, 1], 1),
    (42, [41, 1, 1], 1),
    (54, [30, 7, 1], 1),
    (64, [39, 1, 3], 1),
    (74, [26, 1, 3], 1),
    (84, [22, 0, 0], 1),
];

/// Server ticks at which a frame is captured: before the first cut, mid-sever,
/// just after the beam detaches, mid-fall, and settled.
const DESTRUCTION_CAPTURE_TICKS: [u64; 5] = [3, 20, 45, 90, 190];

const DESTRUCTION_TICKS: u64 = 200;

fn brush_cell(cell: [i64; 3], radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            cell[0] * BRUSH_UNIT + h,
            cell[1] * BRUSH_UNIT + h,
            cell[2] * BRUSH_UNIT + h,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .expect("scripted brush is valid")
}

fn volume_mesh(volume: &Volume, strategy: MeshStrategy) -> spall_mesh::VolumeMesh {
    build_volume_mesh(
        volume,
        Generation(1),
        TopologyEpoch::START,
        MeshOptions {
            strategy,
            ..MeshOptions::default()
        },
    )
    .expect("cross-brick bridge scene meshes within budget")
}

/// Mesh the live authoritative world (terrain + every detached body at its
/// current pose) into a capture [`Scene`]. When `camera` is `None` the camera is
/// framed on the initial scene; pass the framed camera back in for later frames
/// so the beam is seen to fall against a fixed view.
fn destruction_scene(
    sim: &Simulation,
    strategy: MeshStrategy,
    aspect: f32,
    camera: Option<Camera>,
) -> Scene {
    let mut scene = Scene::new(camera.unwrap_or(Camera {
        aspect,
        fov_y: 55_f32.to_radians(),
        ..Default::default()
    }));

    let terrain = volume_mesh(&sim.world().terrain().volume, strategy);
    if !terrain.mesh.vertices.is_empty() {
        scene = scene.with_item(SceneItem::new("terrain", terrain.mesh, Mat4::IDENTITY));
    }

    for (i, body) in sim.world().bodies().enumerate() {
        let vm = volume_mesh(&body.volume, strategy);
        if vm.mesh.vertices.is_empty() {
            continue;
        }
        let q = body.pose.rotation;
        let t = body.pose.translation_m;
        let model = Mat4::from_rotation_translation(
            Quat::from_xyzw(q.x as f32, q.y as f32, q.z as f32, q.w as f32),
            Vec3::new(t[0] as f32, t[1] as f32, t[2] as f32),
        );
        scene = scene.with_item(SceneItem::new(format!("body_{i}"), vm.mesh, model));
    }

    scene.materials = spall_render::default_materials();
    if camera.is_none() {
        scene.frame_all(destruction_view_dir());
    }
    scene
}

#[derive(Serialize)]
struct DestructionFrame {
    tick: u64,
    transactions_committed_so_far: u64,
    body_count: usize,
    detached_body_max_drop_m: f64,
    items_drawn: usize,
    triangles_rasterised: u64,
    gpu_render_millis: Option<f64>,
    gpu_shadow_millis: Option<f64>,
    gpu_opaque_millis: Option<f64>,
    gpu_tone_map_millis: Option<f64>,
    cpu_capture_millis: f64,
    image: String,
}

#[derive(Serialize)]
struct DestructionSummary {
    version: u32,
    scene: &'static str,
    adapter: String,
    backend: String,
    /// `true` only when the frames carry a measured GPU render-pass timing.
    gpu_timing_available: bool,
    width: u32,
    height: u32,
    server_ticks: u64,
    transactions_committed: u64,
    final_world_hash: String,
    total_solid_cells_start: u64,
    total_solid_cells_end: u64,
    frames: Vec<DestructionFrame>,
}

/// T11a / ENG-62 increment 2: drive the authoritative `spall_sim` world on the
/// `g1-networked-destruction` cross-brick scene through the cut script and
/// render offscreen frames of the real destruction — terrain and detached
/// bodies meshed from the live world — with measured GPU pass timings.
fn run_destruction(args: &Args) -> Result<DestructionSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();
    let aspect = capture_aspect(args.width, args.height);

    let mut setup = fixtures::cross_brick_bridged_setup();
    // Match the networked host: no body in this scene enables per-body CCD and
    // the terrain collider is rebuilt on every cut, so the CCD sweep is skipped.
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup))
        .map_err(|e| RenderError::Gpu(format!("destruction sim setup failed: {e}")))?;
    let actor = EntityId::new(1).expect("nonzero entity id");

    let total_solid_cells_start = sim.world().total_solid_cells();
    let opts = CaptureOptions {
        width: args.width,
        height: args.height,
        views: vec![DebugView::Shaded],
        ..Default::default()
    };

    let mut next_cut = 0usize;
    let mut request_id = 1u64;
    let mut committed = 0u64;
    let mut camera: Option<Camera> = None;
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut gpu_timing_available = false;
    let mut frames = Vec::new();

    for tick in 1..=DESTRUCTION_TICKS {
        while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
            let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
            let _ = sim.submit(EditIntent::cut(
                RequestId(request_id),
                actor,
                EditTarget::Terrain,
                brush_cell(cell, radius),
            ));
            request_id += 1;
            next_cut += 1;
        }

        let report = sim
            .tick()
            .map_err(|e| RenderError::Gpu(format!("destruction sim tick {tick} failed: {e}")))?;
        committed += report.committed.len() as u64;

        if DESTRUCTION_CAPTURE_TICKS.contains(&tick) {
            let scene = destruction_scene(&sim, strategy, aspect, camera);
            if camera.is_none() {
                camera = Some(scene.camera);
            }
            let out_dir = args.out.join(format!("tick_{tick:03}"));
            let cap_start = std::time::Instant::now();
            let report_img = capture_scene(&ctx, &scene, &out_dir, &opts)?;
            let cpu_capture_millis = cap_start.elapsed().as_secs_f64() * 1_000.0;
            adapter = report_img.adapter.clone();
            backend = report_img.backend.clone();
            gpu_timing_available |= report_img.timing.gpu_render_millis.is_some();

            let max_drop = sim
                .world()
                .bodies()
                .map(|b| -b.pose.translation_m[1])
                .fold(0.0_f64, f64::max);
            let passes = report_img.timing.gpu_passes;
            frames.push(DestructionFrame {
                tick,
                transactions_committed_so_far: committed,
                body_count: sim.world().body_count(),
                detached_body_max_drop_m: max_drop,
                items_drawn: report_img.items_drawn,
                triangles_rasterised: report_img.triangles,
                gpu_render_millis: report_img.timing.gpu_render_millis,
                gpu_shadow_millis: passes.map(|p| p.shadow_millis),
                gpu_opaque_millis: passes.map(|p| p.opaque_millis),
                gpu_tone_map_millis: passes.map(|p| p.tone_map_millis),
                cpu_capture_millis,
                image: report_img
                    .images
                    .first()
                    .map(|i| i.path.display().to_string())
                    .unwrap_or_default(),
            });
        }
    }

    Ok(DestructionSummary {
        version: 1,
        scene: "destruction",
        adapter,
        backend,
        gpu_timing_available,
        width: args.width,
        height: args.height,
        server_ticks: DESTRUCTION_TICKS,
        transactions_committed: committed,
        final_world_hash: sim.world().world_hash().to_string(),
        total_solid_cells_start,
        total_solid_cells_end: sim.world().total_solid_cells(),
        frames,
    })
}

// --- `--scene destruction-networked` (T11a / ENG-62 increment 3) ---------

/// Mesh a **replicated** world (terrain + every known body, at its latest
/// replicated pose) into a capture [`Scene`] — the [`destruction_scene`]
/// sibling for a [`spall_client::ReplicaWorld`] instead of an authoritative
/// [`Simulation`]. `camera` behaves the same way: `None` frames on the
/// initial scene, `Some` reuses a fixed camera for later frames.
fn destruction_scene_from_replica(
    replica: &spall_client::ReplicaWorld,
    strategy: MeshStrategy,
    aspect: f32,
    camera: Option<Camera>,
) -> Scene {
    let mut scene = Scene::new(camera.unwrap_or(Camera {
        aspect,
        fov_y: 55_f32.to_radians(),
        ..Default::default()
    }));

    if let Some(terrain) = replica.terrain_volume() {
        let vm = volume_mesh(terrain, strategy);
        if !vm.mesh.vertices.is_empty() {
            scene = scene.with_item(SceneItem::new("terrain", vm.mesh, Mat4::IDENTITY));
        }
    }

    let render_tick = replica.now_tick() as f64;
    for (i, (entity, volume_id)) in replica.body_volumes().enumerate() {
        let Some(volume) = replica.volume(volume_id) else {
            continue;
        };
        let vm = volume_mesh(volume, strategy);
        if vm.mesh.vertices.is_empty() {
            continue;
        }
        let model = match replica.interpolated_pose(entity, render_tick) {
            Some(pose) => {
                let [x, y, z, w] = pose.rotation.to_unit().unwrap_or([0.0, 0.0, 0.0, 1.0]);
                let t = pose.translation_m;
                Mat4::from_rotation_translation(
                    Quat::from_xyzw(x, y, z, w),
                    Vec3::new(t[0] as f32, t[1] as f32, t[2] as f32),
                )
            }
            // No motion snapshot yet for this body (it detached this
            // instant, before the 20 Hz batch caught up): draw it at its
            // volume-local origin rather than skip it.
            None => Mat4::IDENTITY,
        };
        scene = scene.with_item(SceneItem::new(format!("body_{i}"), vm.mesh, model));
    }

    scene.materials = spall_render::default_materials();
    if camera.is_none() {
        scene.frame_all(destruction_view_dir());
    }
    scene
}

/// T11a / ENG-62 increment 3: the same cut script and capture ticks as
/// [`run_destruction`], but this time driven over **real QUIC**: a real
/// `spall_server::serve` host, a real plain scripted client that performs
/// every cut, and a real second "observer" client that never edits — only
/// connects, replicates, and is the one this function meshes and renders.
/// Proves the destruction capture path against genuinely network-replicated
/// state (`spall_client::ReplicaWorld`), not the authoritative
/// `spall_sim::Simulation` directly (contrast [`run_destruction`]). Server
/// and both clients run as real Tokio-driven QUIC endpoints on background
/// threads in this process — the same pattern `spall_server`'s own
/// `replication_session` / `late_join_session` integration tests use, real
/// wire serialization and transport over loopback, just not separate OS
/// processes.
fn run_destruction_networked(args: &Args) -> Result<DestructionSummary, RenderError> {
    use spall_client::{
        BaselineScene, ClientNetConfig, ReplicaWorld, ScriptTarget, ScriptedAction, cut_request,
        run_replication_client,
    };
    use spall_net::{Fingerprint, JoinToken, TransportConfig};
    use spall_server::{Scene as ServerScene, ServeConfig, serve};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();
    let aspect = capture_aspect(args.width, args.height);

    let dir = std::env::temp_dir().join(format!(
        "spall-capture-destruction-networked-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| RenderError::Gpu(format!("scratch dir {}: {e}", dir.display())))?;
    let token = JoinToken::generate().expect("join token");
    let fp_path = dir.join("server.fingerprint");
    let addr_path = dir.join("server.addr");

    let server_cfg = ServeConfig {
        listen: "127.0.0.1:0".parse().expect("valid loopback addr"),
        scene: ServerScene::CrossBridgeCut,
        join_token: token,
        max_ticks: DESTRUCTION_TICKS,
        quiescence_ticks: 0,
        min_clients: 2,
        max_clients: 2,
        startup_timeout: Duration::from_secs(20),
        // Real-time pacing, matching `spall_server`'s own networked
        // integration tests: the scripted client's `at_tick` actions are
        // timed off wall-clock, so the server must actually advance at
        // 60 Hz for them to land near the intended tick.
        paced: true,
        log_json: dir.join("server.jsonl"),
        summary_json: Some(dir.join("server.summary.json")),
        fingerprint_out: Some(fp_path.clone()),
        addr_out: Some(addr_path.clone()),
        transport: TransportConfig::for_tests(),
        save: None,
        checkpoint_interval_ticks: 0,
        seed: 0,
        catch_up_cap: spall_server::serve::DEFAULT_CATCH_UP_CAP,
        max_join_retries: spall_server::serve::DEFAULT_MAX_JOIN_RETRIES,
        dev_unvalidated_actions: true,
        save_faults: None,
        await_body_settle: false,
        motion_interest: None,
        residency: None,
    };
    let server_thread = std::thread::spawn(move || serve(server_cfg));

    let wait_for_file = |path: &PathBuf, deadline: Duration| -> Result<String, RenderError> {
        let start = Instant::now();
        loop {
            if let Ok(s) = std::fs::read_to_string(path)
                && !s.trim().is_empty()
            {
                return Ok(s.trim().to_string());
            }
            if start.elapsed() >= deadline {
                return Err(RenderError::Gpu(format!(
                    "timed out waiting for {}",
                    path.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let fp_hex = wait_for_file(&fp_path, Duration::from_secs(15))?;
    let addr_str = wait_for_file(&addr_path, Duration::from_secs(15))?;
    let fingerprint = Fingerprint::from_hex(&fp_hex)
        .ok_or_else(|| RenderError::Gpu("invalid server fingerprint".into()))?;
    let connect_addr = addr_str
        .parse()
        .map_err(|e| RenderError::Gpu(format!("invalid server addr {addr_str}: {e}")))?;

    // The plain scripted client: performs every `DESTRUCTION_SCRIPT` cut
    // against terrain, same as `run_destruction`'s authoritative loop, but
    // as a real `ActionRequest` over the wire.
    let cutter_cfg = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: DESTRUCTION_SCRIPT
            .iter()
            .enumerate()
            .map(|(i, &(at_tick, cell, radius))| ScriptedAction {
                at_tick,
                request: cut_request(i as u64 + 1, 0, cell, radius),
                target: ScriptTarget::Terrain,
            })
            .collect(),
        movement_script: Vec::new(),
        late_join: false,
        baseline_scene: BaselineScene::CrossBridgeCut,
        run_ticks: DESTRUCTION_TICKS,
        idle_grace: Duration::from_secs(5),
        overall_timeout: Duration::from_secs(30),
        log_json: dir.join("cutter.jsonl"),
        summary_json: Some(dir.join("cutter.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: None,
    };
    let cutter_thread = std::thread::spawn(move || run_replication_client(cutter_cfg));

    // The observer client: no script of its own, `on_replica_ready` hands
    // this thread the live, network-replicated world the instant its
    // baseline is installed, so the capture loop below can poll it on its
    // own schedule — completely decoupled from the client's own
    // receive/apply loop.
    let replica_slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>> = Arc::new(Mutex::new(None));
    let replica_slot_hook = replica_slot.clone();
    let observer_cfg = ClientNetConfig {
        connect_addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: false,
        baseline_scene: BaselineScene::CrossBridgeCut,
        run_ticks: DESTRUCTION_TICKS,
        idle_grace: Duration::from_secs(5),
        overall_timeout: Duration::from_secs(30),
        log_json: dir.join("observer.jsonl"),
        summary_json: Some(dir.join("observer.summary.json")),
        transport: TransportConfig::for_tests(),
        client_residency: None,
        on_replica_ready: Some(Arc::new(move |r| {
            *replica_slot_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        })),
    };
    let observer_thread = std::thread::spawn(move || run_replication_client(observer_cfg));

    let replica = {
        let start = Instant::now();
        loop {
            if let Some(r) = replica_slot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                break r;
            }
            if start.elapsed() >= Duration::from_secs(15) {
                return Err(RenderError::Gpu(
                    "observer client never reported its replica ready".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // Poll the live replica for each capture tick, in order. Reaching a tick
    // is "the replica has observed at least this server tick" — real network
    // delay means that can lag real wall-clock time, but never runs
    // backwards, so waiting it out (bounded) is always correct, just not
    // instant.
    let opts = CaptureOptions {
        width: args.width,
        height: args.height,
        views: vec![DebugView::Shaded],
        ..Default::default()
    };
    let mut camera: Option<Camera> = None;
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut gpu_timing_available = false;
    let mut frames = Vec::new();
    let total_solid_cells_start = {
        let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
        guard.total_solid_cells()
    };

    for &tick in &DESTRUCTION_CAPTURE_TICKS {
        let start = Instant::now();
        loop {
            let reached = replica.lock().unwrap_or_else(|e| e.into_inner()).now_tick() >= tick;
            if reached {
                break;
            }
            if start.elapsed() >= Duration::from_secs(20) {
                return Err(RenderError::Gpu(format!(
                    "observer replica never reached tick {tick} (network run stalled)"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let scene = {
            let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
            destruction_scene_from_replica(&guard, strategy, aspect, camera)
        };
        if camera.is_none() {
            camera = Some(scene.camera);
        }
        let out_dir = args.out.join(format!("tick_{tick:03}"));
        let cap_start = Instant::now();
        let report_img = capture_scene(&ctx, &scene, &out_dir, &opts)?;
        let cpu_capture_millis = cap_start.elapsed().as_secs_f64() * 1_000.0;
        adapter = report_img.adapter.clone();
        backend = report_img.backend.clone();
        gpu_timing_available |= report_img.timing.gpu_render_millis.is_some();

        let max_drop = {
            let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
            let render_tick = guard.now_tick() as f64;
            guard
                .body_volumes()
                .filter_map(|(e, _)| guard.interpolated_pose(e, render_tick))
                .map(|p| -p.translation_m[1])
                .fold(0.0_f64, f64::max)
        };
        let body_count = replica
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .body_volumes()
            .count();
        let passes = report_img.timing.gpu_passes;
        frames.push(DestructionFrame {
            tick,
            transactions_committed_so_far: 0, // not tracked client-side; see final_world_hash
            body_count,
            detached_body_max_drop_m: max_drop,
            items_drawn: report_img.items_drawn,
            triangles_rasterised: report_img.triangles,
            gpu_render_millis: report_img.timing.gpu_render_millis,
            gpu_shadow_millis: passes.map(|p| p.shadow_millis),
            gpu_opaque_millis: passes.map(|p| p.opaque_millis),
            gpu_tone_map_millis: passes.map(|p| p.tone_map_millis),
            cpu_capture_millis,
            image: report_img
                .images
                .first()
                .map(|i| i.path.display().to_string())
                .unwrap_or_default(),
        });
    }

    let (final_world_hash, total_solid_cells_end) = {
        let guard = replica.lock().unwrap_or_else(|e| e.into_inner());
        (guard.world_hash().to_string(), guard.total_solid_cells())
    };

    let cutter = cutter_thread
        .join()
        .map_err(|_| RenderError::Gpu("cutter client thread panicked".into()))?
        .map_err(|e| RenderError::Gpu(format!("cutter client failed: {e}")))?;
    let transactions_committed = cutter.transactions_applied;
    let server = server_thread
        .join()
        .map_err(|_| RenderError::Gpu("server thread panicked".into()))?
        .map_err(|e| RenderError::Gpu(format!("server failed: {e}")))?;
    let _ = observer_thread.join();

    if server.final_world_hash != final_world_hash {
        return Err(RenderError::Gpu(format!(
            "observer replica hash {final_world_hash} != server hash {} — replication did not converge",
            server.final_world_hash
        )));
    }

    Ok(DestructionSummary {
        version: 1,
        scene: "destruction-networked",
        adapter,
        backend,
        gpu_timing_available,
        width: args.width,
        height: args.height,
        server_ticks: DESTRUCTION_TICKS,
        transactions_committed,
        final_world_hash,
        total_solid_cells_start,
        total_solid_cells_end,
        frames,
    })
}

// --- `--scene g2-collapse` (T15 / ENG-22 increment 5) ---------------------

/// Server ticks a lit frame is captured at across the 200-tick collapse:
/// before the first cut, through the sever, the detach, the fall, and settled.
const G2_COLLAPSE_CAPTURE_TICKS: [u64; 13] = [3, 8, 16, 24, 32, 45, 60, 75, 92, 115, 140, 170, 198];

/// Terrain-cell world size for `cross_brick_bridged_setup` (`CellSizeCode::Quarter`).
const TERRAIN_CELL_M: f32 = 0.25;

fn mesh_world_aabb(mesh: &spall_mesh::Mesh) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for v in &mesh.vertices {
        let p = Vec3::from_array(v.position);
        min = min.min(p);
        max = max.max(p);
    }
    (min, max)
}

/// Sample a `spall_voxel` terrain volume's occupancy into a fresh lighting
/// clipmap: step through its world AABB at the terrain cell size and set every
/// clipmap cell whose centre lands in a filled terrain cell.
fn terrain_light_volume(
    terrain: &spall_voxel::Volume,
    origin: Vec3,
    strategy: MeshStrategy,
) -> LightingVolume {
    let mut v = LightingVolume::empty(origin);
    let step = TERRAIN_CELL_M;
    let tmesh = volume_mesh(terrain, strategy);
    if tmesh.mesh.vertices.is_empty() {
        return v;
    }
    let (mn, mx) = mesh_world_aabb(&tmesh.mesh);
    let mut y = mn.y - step;
    while y <= mx.y + step {
        let mut x = mn.x - step;
        while x <= mx.x + step {
            let mut z = mn.z - step;
            while z <= mx.z + step {
                let p = Vec3::new(x, y, z);
                let g = GlobalCell::new(
                    (p.x / TERRAIN_CELL_M).floor() as i64,
                    (p.y / TERRAIN_CELL_M).floor() as i64,
                    (p.z / TERRAIN_CELL_M).floor() as i64,
                );
                if let Ok(Sample::Filled(_)) = terrain.sample(g) {
                    v.set(v.world_to_cell(p), 1);
                }
                z += step;
            }
            x += step;
        }
        y += step;
    }
    v
}

/// World-space AABB of a detached body's meshed volume at its current pose, or
/// `None` when the body has no geometry. This is the box the clipmap fills
/// solid for the debris (a box approximation, consistent with how T14
/// `moving_box` treats a moving body).
fn body_world_aabb(body: &Body, strategy: MeshStrategy) -> Option<(Vec3, Vec3)> {
    let bm = volume_mesh(&body.volume, strategy);
    if bm.mesh.vertices.is_empty() {
        return None;
    }
    let q = body.pose.rotation;
    let t = body.pose.translation_m;
    let model = Mat4::from_rotation_translation(
        Quat::from_xyzw(q.x as f32, q.y as f32, q.z as f32, q.w as f32),
        Vec3::new(t[0] as f32, t[1] as f32, t[2] as f32),
    );
    let mut mn = Vec3::splat(f32::INFINITY);
    let mut mx = Vec3::splat(f32::NEG_INFINITY);
    for vtx in &bm.mesh.vertices {
        let p = model.transform_point3(Vec3::from_array(vtx.position));
        mn = mn.min(p);
        mx = mx.max(p);
    }
    Some((mn, mx))
}

/// Build a T13/T14 lighting clipmap from the live authoritative world: the
/// terrain occupancy (see [`terrain_light_volume`]) plus every detached body's
/// world AABB filled solid (see [`body_world_aabb`]).
fn sim_light_volume(sim: &Simulation, origin: Vec3, strategy: MeshStrategy) -> LightingVolume {
    let mut v = terrain_light_volume(&sim.world().terrain().volume, origin, strategy);

    for body in sim.world().bodies() {
        let Some((mn, mx)) = body_world_aabb(body, strategy) else {
            continue;
        };
        let lo = v.world_to_cell(mn);
        let hi = v.world_to_cell(mx) + glam::IVec3::ONE;
        v.fill_box(lo, hi, 1);
    }
    v
}

/// Mesh the live world (terrain + bodies) into a capture [`Scene`] and attach a
/// clipmap rebuilt from it, so `capture_scene` lights the frame with T13/T14
/// indirect. A daytime sky clear gives the open scene an ambient term.
fn g2_collapse_scene(
    sim: &Simulation,
    strategy: MeshStrategy,
    aspect: f32,
    camera: Option<Camera>,
) -> Scene {
    let mut scene = destruction_scene(sim, strategy, aspect, camera);
    scene.clear = [0.25, 0.38, 0.58, 1.0];
    scene.with_lighting(sim_light_volume(sim, Vec3::splat(-32.0), strategy))
}

#[derive(Serialize)]
struct G2CollapseFrame {
    tick: u64,
    transactions_committed_so_far: u64,
    solid_cells: u64,
    body_count: usize,
    detached_body_max_drop_m: f64,
    indirect_cells: usize,
    /// Full per-tick re-trace GPU cost (cold — `capture_scene` re-traces the
    /// whole clipmap every call; the bounded per-tick path needs a
    /// sim → `LightingUpdate` translation, increment 6).
    gpu_render_millis: Option<f64>,
    gpu_indirect_trace_millis: Option<f64>,
    gpu_indirect_denoise_millis: Option<f64>,
    gpu_shadow_millis: Option<f64>,
    gpu_opaque_millis: Option<f64>,
    gpu_tone_map_millis: Option<f64>,
    image: String,
}

#[derive(Serialize)]
struct G2CollapseSummary {
    /// Version 1: T15 increment 5 — GI-lit rapid-destruction sequence.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    gpu_timing_available: bool,
    server_ticks: u64,
    transactions_committed: u64,
    final_world_hash: String,
    solid_cells_start: u64,
    solid_cells_end: u64,
    /// Per-tick cold full-retrace GPU cost, percentiles over the captured ticks.
    gpu_render: Option<G2StatBlock>,
    gpu_indirect_trace: Option<G2StatBlock>,
    gpu_indirect_denoise: Option<G2StatBlock>,
    /// Settled-frame (last capture tick) 60-frame static-noise flicker of a
    /// region away from the collapse, and of a region the fallen beam shadows.
    settled_stable_band_flicker: f32,
    settled_shadow_band_flicker: f32,
    frames: Vec<G2CollapseFrame>,
    quality_flags: Vec<String>,
}

fn run_g2_collapse(args: &Args) -> Result<G2CollapseSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);

    let mut setup = fixtures::cross_brick_bridged_setup();
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup))
        .map_err(|e| RenderError::Gpu(format!("collapse sim setup failed: {e}")))?;
    let actor = EntityId::new(1).expect("nonzero entity id");
    let solid_cells_start = sim.world().total_solid_cells();

    let opts = CaptureOptions {
        width,
        height,
        views: vec![DebugView::Shaded],
        ..Default::default()
    };

    let mut next_cut = 0usize;
    let mut request_id = 1u64;
    let mut committed = 0u64;
    let mut camera: Option<Camera> = None;
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut gpu_timing_available = false;
    let mut frames: Vec<G2CollapseFrame> = Vec::new();
    let mut render_ms = Vec::new();
    let mut trace_ms = Vec::new();
    let mut denoise_ms = Vec::new();
    let mut last_solid = solid_cells_start;
    let mut quality_flags = Vec::new();
    let mut last_scene: Option<Scene> = None;

    for tick in 1..=DESTRUCTION_TICKS {
        while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
            let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
            let _ = sim.submit(EditIntent::cut(
                RequestId(request_id),
                actor,
                EditTarget::Terrain,
                brush_cell(cell, radius),
            ));
            request_id += 1;
            next_cut += 1;
        }
        let report = sim
            .tick()
            .map_err(|e| RenderError::Gpu(format!("collapse sim tick {tick} failed: {e}")))?;
        committed += report.committed.len() as u64;

        if !G2_COLLAPSE_CAPTURE_TICKS.contains(&tick) {
            continue;
        }
        let scene = g2_collapse_scene(&sim, strategy, aspect, camera);
        if camera.is_none() {
            camera = Some(scene.camera);
        }
        let out_dir = args.out.join(format!("tick_{tick:03}"));
        let img = capture_scene(&ctx, &scene, &out_dir, &opts)?;
        adapter = img.adapter.clone();
        backend = img.backend.clone();
        gpu_timing_available |= img.timing.gpu_render_millis.is_some();

        let solid_cells = sim.world().total_solid_cells();
        if solid_cells > last_solid {
            quality_flags.push(format!(
                "g2-collapse: solid cell count rose {last_solid} -> {solid_cells} at tick {tick} — mined terrain regrew"
            ));
        }
        last_solid = solid_cells;

        let passes = img.timing.gpu_passes;
        if let Some(p) = passes {
            render_ms.push(p.total());
            trace_ms.push(p.indirect_trace_millis);
            denoise_ms.push(p.indirect_denoise_millis);
        }
        let max_drop = sim
            .world()
            .bodies()
            .map(|b| -b.pose.translation_m[1])
            .fold(0.0_f64, f64::max);
        frames.push(G2CollapseFrame {
            tick,
            transactions_committed_so_far: committed,
            solid_cells,
            body_count: sim.world().body_count(),
            detached_body_max_drop_m: max_drop,
            indirect_cells: img.indirect_cells,
            gpu_render_millis: img.timing.gpu_render_millis,
            gpu_indirect_trace_millis: passes.map(|p| p.indirect_trace_millis),
            gpu_indirect_denoise_millis: passes.map(|p| p.indirect_denoise_millis),
            gpu_shadow_millis: passes.map(|p| p.shadow_millis),
            gpu_opaque_millis: passes.map(|p| p.opaque_millis),
            gpu_tone_map_millis: passes.map(|p| p.tone_map_millis),
            image: img
                .images
                .first()
                .map(|i| i.path.display().to_string())
                .unwrap_or_default(),
        });
        last_scene = Some(scene);
    }

    // Settled-frame stability: 60 identical Shaded frames of the last captured
    // tick's lit world.
    let (mut settled_stable_band_flicker, mut settled_shadow_band_flicker) = (0.0, 0.0);
    if let Some(scene) = last_scene {
        let motion_opts = MotionSequenceOptions {
            width,
            height,
            exposure: 1.0,
            view: DebugView::Shaded,
            temporal_weight: 0.1,
            halo_cells: 12,
            png_stride: 30,
        };
        let mframes: Vec<MotionFrame> = (0..60)
            .map(|_| MotionFrame {
                camera: scene.camera,
                update: LightingUpdate::new(),
            })
            .collect();
        let probes = [
            ProbeBand {
                name: "stable".into(),
                band: [0.05, 0.30],
            },
            ProbeBand {
                name: "shadow".into(),
                band: [0.40, 0.70],
            },
        ];
        let noise = capture_motion_sequence(
            &ctx,
            &scene,
            &mframes,
            &probes,
            &args.out.join("settled-noise"),
            &motion_opts,
        )?;
        settled_stable_band_flicker = flicker_index(&noise.bands[0].luminance);
        settled_shadow_band_flicker = flicker_index(&noise.bands[1].luminance);
        if settled_stable_band_flicker > G2_FLICKER_FLAG {
            quality_flags.push(format!(
                "g2-collapse: settled `stable` band is not steady (flicker index {settled_stable_band_flicker:.4} > {G2_FLICKER_FLAG}) — temporal noise / instability"
            ));
        }
    }

    Ok(G2CollapseSummary {
        version: 1,
        mode: "g2-collapse",
        adapter,
        backend,
        width,
        height,
        gpu_timing_available,
        server_ticks: DESTRUCTION_TICKS,
        transactions_committed: committed,
        final_world_hash: sim.world().world_hash().to_string(),
        solid_cells_start,
        solid_cells_end: sim.world().total_solid_cells(),
        gpu_render: FrameStats::from_samples(&render_ms).map(Into::into),
        gpu_indirect_trace: FrameStats::from_samples(&trace_ms).map(Into::into),
        gpu_indirect_denoise: FrameStats::from_samples(&denoise_ms).map(Into::into),
        settled_stable_band_flicker,
        settled_shadow_band_flicker,
        frames,
        quality_flags,
    })
}

// --- `--scene g2-bounded-collapse` (T15 / ENG-22 increment 6) ------------

/// Lighting-clipmap world origin for the collapse fixture (matches increment 5).
const G2_COLLAPSE_LIGHT_ORIGIN: Vec3 = Vec3::splat(-32.0);

/// Ticks the bounded per-tick path is checked against a cold full-refresh
/// clipmap of the same world — spaced across the sever, detach, fall and settle.
const G2_BOUNDED_CONVERGENCE_TICKS: [u64; 5] = [8, 24, 45, 92, 170];

/// Max accepted relative luminance gap between the bounded per-tick path and a
/// cold full-refresh reference at a convergence checkpoint. Above this, a stale
/// vacated shadow or an erased overlapping occupancy is the likely cause.
const G2_BOUNDED_CONVERGENCE_FLAG: f32 = 0.12;

/// Min accepted cell-for-cell agreement between the bounded path's final clipmap
/// and a from-scratch resample of the final world.
const G2_BOUNDED_FINAL_AGREEMENT_FLAG: f32 = 0.98;

/// Distinct clipmap cells that carry filled terrain inside a world-space box,
/// sampled at the terrain cell size exactly as [`terrain_light_volume`] does.
fn terrain_solid_light_cells_in(
    terrain: &Volume,
    origin: Vec3,
    box_min: Vec3,
    box_max: Vec3,
) -> std::collections::BTreeSet<(i32, i32, i32)> {
    let mut cells = std::collections::BTreeSet::new();
    let step = TERRAIN_CELL_M;
    let mut y = box_min.y;
    while y <= box_max.y {
        let mut x = box_min.x;
        while x <= box_max.x {
            let mut z = box_min.z;
            while z <= box_max.z {
                let g = GlobalCell::new(
                    (x / TERRAIN_CELL_M).floor() as i64,
                    (y / TERRAIN_CELL_M).floor() as i64,
                    (z / TERRAIN_CELL_M).floor() as i64,
                );
                if let Ok(Sample::Filled(_)) = terrain.sample(g) {
                    let c = ((Vec3::new(x, y, z) - origin) / LIGHT_CELL_SIZE_METRES).floor();
                    cells.insert((c.x as i32, c.y as i32, c.z as i32));
                }
                z += step;
            }
            x += step;
        }
        y += step;
    }
    cells
}

/// A single-cell solid `LightingRegion` for clipmap cell `cell`.
fn light_cell_region(origin: Vec3, cell: (i32, i32, i32)) -> LightingRegion {
    let lo =
        origin + Vec3::new(cell.0 as f32, cell.1 as f32, cell.2 as f32) * LIGHT_CELL_SIZE_METRES;
    LightingRegion {
        min_m: lo,
        max_m: lo + Vec3::splat(LIGHT_CELL_SIZE_METRES),
        material: 1,
    }
}

/// World-space AABB of a scripted terrain cut brush (script cells are terrain
/// cells), padded by one clipmap cell so the cleared span covers the sphere.
fn cut_world_aabb(cell: [i64; 3], radius_cells: i64) -> (Vec3, Vec3) {
    let centre = Vec3::new(
        cell[0] as f32 + 0.5,
        cell[1] as f32 + 0.5,
        cell[2] as f32 + 0.5,
    ) * TERRAIN_CELL_M;
    let pad = Vec3::splat(radius_cells as f32 * TERRAIN_CELL_M + LIGHT_CELL_SIZE_METRES);
    (centre - pad, centre + pad)
}

/// Translate one tick's committed terrain cuts and moving-body poses into the
/// engine-agnostic [`LightingUpdate`] the persistent loop consumes: every
/// changed world box is marked dirty, then the terrain still solid inside it
/// (and every body at its new pose) is re-asserted so nothing overlapping is
/// erased. This is the "translate committed cuts and old/new body bounds into
/// the existing lighting DTOs" step the G2 follow-up calls for.
fn collapse_tick_update(
    terrain: &Volume,
    origin: Vec3,
    cuts_this_tick: &[([i64; 3], i64)],
    prev_bodies: &[(Vec3, Vec3)],
    cur_bodies: &[(Vec3, Vec3)],
) -> LightingUpdate {
    let mut update = LightingUpdate::new();

    for &(cell, radius) in cuts_this_tick {
        let (bmin, bmax) = cut_world_aabb(cell, radius);
        update = update.dirty_bound(bmin, bmax);
        for c in terrain_solid_light_cells_in(terrain, origin, bmin, bmax) {
            update = update.with_region(light_cell_region(origin, c));
        }
    }

    let n = prev_bodies.len().max(cur_bodies.len());
    for i in 0..n {
        let prev = prev_bodies.get(i).copied();
        let next = cur_bodies.get(i).copied();
        let (dmin, dmax) = match (prev, next) {
            (Some(p), Some(nx)) => {
                update = update.dirty_bound(p.0, p.1).dirty_bound(nx.0, nx.1);
                (p.0.min(nx.0), p.1.max(nx.1))
            }
            (Some(p), None) => {
                update = update.dirty_bound(p.0, p.1);
                (p.0, p.1)
            }
            (None, Some(nx)) => {
                update = update.dirty_bound(nx.0, nx.1);
                (nx.0, nx.1)
            }
            (None, None) => continue,
        };
        for c in terrain_solid_light_cells_in(terrain, origin, dmin, dmax) {
            update = update.with_region(light_cell_region(origin, c));
        }
        if let Some(nx) = next {
            update = update.region(nx.0, nx.1, 1);
        }
    }

    update
}

/// Cold full-refresh reference probe luminances for `scene`: seed the history
/// with one full-cache trace, hold it (no accumulation), and read the bands.
fn reference_probe_bands(
    ctx: &RenderContext,
    scene: &Scene,
    probes: &[ProbeBand],
    out_dir: &std::path::Path,
) -> Result<Vec<f32>, RenderError> {
    let frames = vec![
        MotionFrame {
            camera: scene.camera,
            update: LightingUpdate::new(),
        },
        MotionFrame {
            camera: scene.camera,
            update: LightingUpdate::new(),
        },
    ];
    let opts = MotionSequenceOptions {
        width: 1920,
        height: 1080,
        exposure: 1.0,
        view: DebugView::Shaded,
        temporal_weight: 1.0,
        halo_cells: 12,
        png_stride: 0,
    };
    let r = capture_motion_sequence(ctx, scene, &frames, probes, out_dir, &opts)?;
    Ok(r.bands
        .iter()
        .map(|b| b.luminance.last().copied().unwrap_or(0.0))
        .collect())
}

#[derive(Serialize)]
struct G2ProbeDelta {
    probe: String,
    bounded_luminance: f32,
    reference_luminance: f32,
    rel_delta: f32,
}

#[derive(Serialize)]
struct G2ConvergenceCheck {
    tick: u64,
    frame: usize,
    max_rel_delta: f32,
    probes: Vec<G2ProbeDelta>,
}

#[derive(Serialize)]
struct G2BoundedCollapseFrame {
    tick: u64,
    transactions_committed_so_far: u64,
    body_count: usize,
    detached_body_max_drop_m: f64,
    /// Clipmap cells re-uploaded this tick (the bounded GPU upload set).
    dirty_light_cells: usize,
    /// Clipmap cells the trace recomputed this tick (dirty AABB + halo).
    retraced_light_cells: u64,
    gpu_frame_millis: Option<f64>,
    gpu_lighting_millis: Option<f64>,
    serial_frame_millis: Option<f64>,
}

#[derive(Serialize)]
struct G2BoundedCollapseSummary {
    /// Version 1: T15 increment 6 — bounded per-tick destruction on the
    /// persistent-resource loop.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    gpu_timing_available: bool,
    server_ticks: u64,
    frames_rendered: usize,
    transactions_committed: u64,
    final_world_hash: String,
    solid_cells_start: u64,
    solid_cells_end: u64,
    gpu_p95_target_millis: f64,
    cpu_p95_target_millis: f64,
    client_p95_target_millis: f64,
    /// Per rendered tick: trace + denoise + temporal + opaque + tone map.
    gpu_frame: Option<G2StatBlock>,
    /// Per rendered tick: the lighting cost only (trace + denoise + temporal).
    gpu_lighting: Option<G2StatBlock>,
    gpu_indirect_trace: Option<G2StatBlock>,
    gpu_indirect_denoise: Option<G2StatBlock>,
    gpu_indirect_temporal: Option<G2StatBlock>,
    /// Renderer per-tick CPU encode cost (no GPU wait, no readback).
    cpu_frame: Option<G2StatBlock>,
    /// Directly-paired per-tick CPU encode + GPU device total, percentiled — a
    /// measured serial frame upper bound, not a sum of marginal percentiles.
    serial_frame: Option<G2StatBlock>,
    gpu_p95_target_met: Option<bool>,
    cpu_p95_target_met: Option<bool>,
    client_p95_target_met: Option<bool>,
    /// Bounded re-upload / re-trace sizes over the collapse.
    dirty_light_cells_p50: u64,
    dirty_light_cells_p95: u64,
    dirty_light_cells_max: u64,
    retraced_light_cells_p50: u64,
    retraced_light_cells_p95: u64,
    retraced_light_cells_max: u64,
    total_light_cells: u64,
    /// Structural edit-to-visible latency of the T14 partial-update path.
    edit_to_visible_frames: u32,
    edit_visible_within_one_frame: bool,
    /// Index of the first rendered tick that dirtied clipmap cells (first
    /// committed cut / first body motion).
    first_edit_frame: Option<usize>,
    /// Bounded vs cold full-refresh at the convergence checkpoints.
    convergence_checks: Vec<G2ConvergenceCheck>,
    convergence_max_rel_delta: f32,
    convergence_ok: bool,
    /// Final persistent clipmap vs a from-scratch resample of the final world.
    final_clipmap_cell_agreement: f32,
    final_solid_cells_bounded: u64,
    final_solid_cells_full_refresh: u64,
    /// 60-frame static-noise flicker of the settled final tick, per probe band.
    settled_band_flicker: Vec<f32>,
    frames: Vec<G2BoundedCollapseFrame>,
    quality_flags: Vec<String>,
    images: Vec<String>,
}

/// Nearest-rank percentile of a `u64` sample set (same convention as `FrameStats`).
fn u64_percentile(samples: &[u64], q: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// T15 increment 6: run the `g1-networked-destruction` collapse on the
/// persistent-resource loop, translating each tick's committed cuts and
/// moving-body poses into a bounded [`LightingUpdate`], and report the per-tick
/// cost, the bounded re-trace sizes, convergence against a cold full-refresh,
/// and the stale-shadow / erased-occupancy checks.
fn run_g2_bounded_collapse(args: &Args) -> Result<G2BoundedCollapseSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);
    let origin = G2_COLLAPSE_LIGHT_ORIGIN;
    let ticks = args.g2_bounded_collapse_ticks;

    let mut setup = fixtures::cross_brick_bridged_setup();
    setup.physics.disable_ccd = true;
    let mut sim = Simulation::new(SimulationConfig::new(setup))
        .map_err(|e| RenderError::Gpu(format!("bounded-collapse sim setup failed: {e}")))?;
    let actor = EntityId::new(1).expect("nonzero entity id");
    let solid_cells_start = sim.world().total_solid_cells();

    // Frame 0 = the world at rest; its full clipmap seeds the persistent loop.
    let base_scene = g2_collapse_scene(&sim, strategy, aspect, None);
    let camera = base_scene.camera;

    let probes = [
        ProbeBand {
            name: "sky".into(),
            band: [0.05, 0.28],
        },
        ProbeBand {
            name: "notch".into(),
            band: [0.36, 0.62],
        },
        ProbeBand {
            name: "ground".into(),
            band: [0.68, 0.95],
        },
    ];

    let mut collapse_frames: Vec<CollapseFrame> = Vec::with_capacity(ticks as usize);
    let mut frame_meta: Vec<(u64, u64, usize, f64)> = Vec::with_capacity(ticks as usize);
    let mut checkpoints: Vec<(usize, u64, Scene)> = Vec::new();

    let mut next_cut = 0usize;
    let mut request_id = 1u64;
    let mut committed = 0u64;
    let mut pending_cuts: Vec<(u64, [i64; 3], i64)> = Vec::new();
    let mut prev_bodies: Vec<(Vec3, Vec3)> = Vec::new();

    for tick in 1..=ticks {
        while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
            let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
            let _ = sim.submit(EditIntent::cut(
                RequestId(request_id),
                actor,
                EditTarget::Terrain,
                brush_cell(cell, radius),
            ));
            pending_cuts.push((request_id, cell, radius));
            request_id += 1;
            next_cut += 1;
        }
        let report = sim.tick().map_err(|e| {
            RenderError::Gpu(format!("bounded-collapse sim tick {tick} failed: {e}"))
        })?;
        committed += report.committed.len() as u64;

        let mut cuts_this_tick: Vec<([i64; 3], i64)> = Vec::new();
        if !report.committed.is_empty() {
            let landed: std::collections::HashSet<u64> =
                report.committed.iter().map(|(id, _)| id.0).collect();
            pending_cuts.retain(|&(id, cell, radius)| {
                if landed.contains(&id) {
                    cuts_this_tick.push((cell, radius));
                    false
                } else {
                    true
                }
            });
        }

        let cur_bodies: Vec<(Vec3, Vec3)> = sim
            .world()
            .bodies()
            .filter_map(|b| body_world_aabb(b, strategy))
            .collect();
        let update = collapse_tick_update(
            &sim.world().terrain().volume,
            origin,
            &cuts_this_tick,
            &prev_bodies,
            &cur_bodies,
        );
        prev_bodies = cur_bodies;

        let max_drop = sim
            .world()
            .bodies()
            .map(|b| -b.pose.translation_m[1])
            .fold(0.0_f64, f64::max);
        frame_meta.push((tick, committed, sim.world().body_count(), max_drop));

        if G2_BOUNDED_CONVERGENCE_TICKS.contains(&tick) {
            checkpoints.push((
                collapse_frames.len(),
                tick,
                g2_collapse_scene(&sim, strategy, aspect, Some(camera)),
            ));
        }

        let items = destruction_scene(&sim, strategy, aspect, Some(camera)).items;
        collapse_frames.push(CollapseFrame {
            camera,
            items,
            update,
        });
    }

    let opts = CollapseSequenceOptions {
        width,
        height,
        exposure: 1.0,
        temporal_weight: 0.1,
        halo_cells: 12,
        png_stride: 15,
    };
    let report = capture_collapse_sequence(
        &ctx,
        &base_scene,
        collapse_frames,
        &probes,
        &args.out.join("ticks"),
        &opts,
    )?;

    let mut quality_flags: Vec<String> = Vec::new();

    // Convergence: bounded per-tick path vs a cold full-refresh at checkpoints.
    let mut convergence_checks: Vec<G2ConvergenceCheck> = Vec::new();
    let mut convergence_max_rel_delta = 0.0_f32;
    for (frame_idx, tick, ref_scene) in &checkpoints {
        let ref_bands = reference_probe_bands(
            &ctx,
            ref_scene,
            &probes,
            &args.out.join(format!("ref_{tick:03}")),
        )?;
        let mut deltas = Vec::new();
        let mut check_max = 0.0_f32;
        for (p, rb) in ref_bands.iter().enumerate() {
            let bounded = report
                .bands
                .get(p)
                .and_then(|b| b.luminance.get(*frame_idx))
                .copied()
                .unwrap_or(0.0);
            let rel = (bounded - rb).abs() / rb.abs().max(1e-3);
            check_max = check_max.max(rel);
            convergence_max_rel_delta = convergence_max_rel_delta.max(rel);
            deltas.push(G2ProbeDelta {
                probe: probes[p].name.clone(),
                bounded_luminance: bounded,
                reference_luminance: *rb,
                rel_delta: rel,
            });
        }
        convergence_checks.push(G2ConvergenceCheck {
            tick: *tick,
            frame: *frame_idx,
            max_rel_delta: check_max,
            probes: deltas,
        });
    }
    let convergence_ok = convergence_max_rel_delta <= G2_BOUNDED_CONVERGENCE_FLAG;
    if !convergence_ok {
        quality_flags.push(format!(
            "g2-bounded-collapse: bounded path diverges from the full-refresh reference (max rel delta {convergence_max_rel_delta:.3} > {G2_BOUNDED_CONVERGENCE_FLAG}) — possible stale vacated shadow or erased overlapping occupancy"
        ));
    }

    // Final clipmap vs a from-scratch resample of the final world.
    let full_final = sim_light_volume(&sim, origin, strategy);
    let (mut agree, mut total) = (0u64, 0u64);
    let (mut bounded_solid, mut refresh_solid) = (0u64, 0u64);
    for (a, b) in report
        .final_lighting
        .cells()
        .iter()
        .zip(full_final.cells().iter())
    {
        total += 1;
        if a == b {
            agree += 1;
        }
        if *a != 0 {
            bounded_solid += 1;
        }
        if *b != 0 {
            refresh_solid += 1;
        }
    }
    let final_agreement = agree as f32 / total.max(1) as f32;
    if final_agreement < G2_BOUNDED_FINAL_AGREEMENT_FLAG {
        quality_flags.push(format!(
            "g2-bounded-collapse: final bounded clipmap disagrees with a from-scratch resample in {:.2}% of cells (< {:.2}% agreement) — stale or erased occupancy",
            (1.0 - final_agreement) * 100.0,
            G2_BOUNDED_FINAL_AGREEMENT_FLAG * 100.0
        ));
    }

    // Edit-to-visible latency: the T14 partial-update path programs frame N's
    // trace region from frame N's own dirty cells *before* frame N renders — one
    // frame by construction. Corroborated end to end by the bounded path (which
    // applies exactly one tick's edit per rendered frame) staying within
    // tolerance of the full-refresh reference at every checkpoint from the first
    // cut onward: a missed edit would drift the accumulated state progressively,
    // not hold a flat offset.
    let first_edit_frame = report.dirty_cells.iter().position(|&d| d > 0);
    let edit_visible_within_one_frame = first_edit_frame.is_some()
        && report.retraced_cells.iter().any(|&r| r > 0)
        && convergence_ok;
    if !edit_visible_within_one_frame {
        quality_flags.push(
            "g2-bounded-collapse: bounded edits did not track the full-refresh reference — edit-to-visible latency not corroborated".into(),
        );
    }

    // Settled stability of the final tick, matching increment 5.
    let settled_scene = g2_collapse_scene(&sim, strategy, aspect, Some(camera));
    let mframes: Vec<MotionFrame> = (0..60)
        .map(|_| MotionFrame {
            camera,
            update: LightingUpdate::new(),
        })
        .collect();
    let noise = capture_motion_sequence(
        &ctx,
        &settled_scene,
        &mframes,
        &probes,
        &args.out.join("settled-noise"),
        &MotionSequenceOptions {
            width,
            height,
            exposure: 1.0,
            view: DebugView::Shaded,
            temporal_weight: 0.1,
            halo_cells: 12,
            png_stride: 30,
        },
    )?;
    let settled_band_flicker: Vec<f32> = noise
        .bands
        .iter()
        .map(|b| flicker_index(&b.luminance))
        .collect();
    for (band, &f) in probes.iter().zip(settled_band_flicker.iter()) {
        if f > G2_FLICKER_FLAG {
            quality_flags.push(format!(
                "g2-bounded-collapse: settled `{}` band is not steady (flicker index {f:.4} > {G2_FLICKER_FLAG})",
                band.name
            ));
        }
    }

    let retraced_u64: Vec<u64> = report.retraced_cells.clone();
    let dirty_u64: Vec<u64> = report.dirty_cells.iter().map(|&d| d as u64).collect();
    let timed = report.gpu_frame_millis.len() == report.frames;
    let frames: Vec<G2BoundedCollapseFrame> = frame_meta
        .iter()
        .enumerate()
        .map(
            |(i, &(tick, committed_so_far, body_count, max_drop))| G2BoundedCollapseFrame {
                tick,
                transactions_committed_so_far: committed_so_far,
                body_count,
                detached_body_max_drop_m: max_drop,
                dirty_light_cells: report.dirty_cells.get(i).copied().unwrap_or(0),
                retraced_light_cells: report.retraced_cells.get(i).copied().unwrap_or(0),
                gpu_frame_millis: timed.then(|| report.gpu_frame_millis[i]),
                gpu_lighting_millis: timed.then(|| report.gpu_lighting_millis[i]),
                serial_frame_millis: timed.then(|| report.serial_frame_millis[i]),
            },
        )
        .collect();

    let gpu_p95 = report.gpu_frame.map(|s| s.p95_millis);
    let cpu_p95 = report.cpu_frame.map(|s| s.p95_millis);
    let serial_p95 = report.serial_frame.map(|s| s.p95_millis);

    Ok(G2BoundedCollapseSummary {
        version: 1,
        mode: "g2-bounded-collapse",
        adapter: report.adapter.clone(),
        backend: report.backend.clone(),
        width,
        height,
        gpu_timing_available: report.gpu_timing_available,
        server_ticks: ticks,
        frames_rendered: report.frames,
        transactions_committed: committed,
        final_world_hash: sim.world().world_hash().to_string(),
        solid_cells_start,
        solid_cells_end: sim.world().total_solid_cells(),
        gpu_p95_target_millis: G2_GPU_P95_TARGET_MS,
        cpu_p95_target_millis: G2_CPU_FRAME_TARGET_MS,
        client_p95_target_millis: G2_CLIENT_FRAME_TARGET_MS,
        gpu_frame: report.gpu_frame.map(Into::into),
        gpu_lighting: report.gpu_lighting.map(Into::into),
        gpu_indirect_trace: report.gpu_indirect_trace.map(Into::into),
        gpu_indirect_denoise: report.gpu_indirect_denoise.map(Into::into),
        gpu_indirect_temporal: report.gpu_indirect_temporal.map(Into::into),
        cpu_frame: report.cpu_frame.map(Into::into),
        serial_frame: report.serial_frame.map(Into::into),
        gpu_p95_target_met: gpu_p95.map(|v| v <= G2_GPU_P95_TARGET_MS),
        cpu_p95_target_met: cpu_p95.map(|v| v <= G2_CPU_FRAME_TARGET_MS),
        client_p95_target_met: serial_p95.map(|v| v <= G2_CLIENT_FRAME_TARGET_MS),
        dirty_light_cells_p50: u64_percentile(&dirty_u64, 0.50),
        dirty_light_cells_p95: u64_percentile(&dirty_u64, 0.95),
        dirty_light_cells_max: dirty_u64.iter().copied().max().unwrap_or(0),
        retraced_light_cells_p50: u64_percentile(&retraced_u64, 0.50),
        retraced_light_cells_p95: u64_percentile(&retraced_u64, 0.95),
        retraced_light_cells_max: retraced_u64.iter().copied().max().unwrap_or(0),
        total_light_cells: report.total_cells,
        edit_to_visible_frames: 1,
        edit_visible_within_one_frame,
        first_edit_frame,
        convergence_checks,
        convergence_max_rel_delta,
        convergence_ok,
        final_clipmap_cell_agreement: final_agreement,
        final_solid_cells_bounded: bounded_solid,
        final_solid_cells_full_refresh: refresh_solid,
        settled_band_flicker,
        frames,
        quality_flags,
        images: report
            .images
            .iter()
            .map(|i| i.path.display().to_string())
            .collect(),
    })
}

// --- `--scene g2-frames` (T15 / ENG-22 increment 1) ------------------------

/// Provisional G2 GPU frame p95 target (`docs/validation.md` "G2").
const G2_GPU_P95_TARGET_MS: f64 = 12.0;

#[derive(Serialize)]
struct G2StatBlock {
    samples: usize,
    min_millis: f64,
    p50_millis: f64,
    p95_millis: f64,
    p99_millis: f64,
    max_millis: f64,
    mean_millis: f64,
}

impl From<FrameStats> for G2StatBlock {
    fn from(s: FrameStats) -> Self {
        Self {
            samples: s.samples,
            min_millis: s.min_millis,
            p50_millis: s.p50_millis,
            p95_millis: s.p95_millis,
            p99_millis: s.p99_millis,
            max_millis: s.max_millis,
            mean_millis: s.mean_millis,
        }
    }
}

#[derive(Serialize)]
struct G2SceneSummary {
    name: String,
    indirect_cells: usize,
    /// `true` only when the blocks below are measured device timings.
    gpu_timing_available: bool,
    /// Sum of every measured pass family per frame.
    gpu_total: Option<G2StatBlock>,
    gpu_indirect_trace: Option<G2StatBlock>,
    gpu_indirect_denoise: Option<G2StatBlock>,
    gpu_shadow: Option<G2StatBlock>,
    gpu_opaque: Option<G2StatBlock>,
    gpu_tone_map: Option<G2StatBlock>,
    /// `gpu_total` p95 <= the provisional target; `null` without GPU timing.
    gpu_p95_target_met: Option<bool>,
    first_image: String,
    last_image: String,
}

#[derive(Serialize)]
struct G2FramesSummary {
    /// Version 1: T15 increment 1 — GPU frame-cost percentiles on the existing
    /// T13/T14 lighting fixtures.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    exposure: f32,
    warmup_frames: u32,
    measured_frames: u32,
    gpu_timing_available: bool,
    gpu_p95_target_millis: f64,
    /// Worst `gpu_total` p95 across every measured scene; `null` without GPU
    /// timing.
    worst_gpu_total_p95_millis: Option<f64>,
    /// `true` iff every scene with GPU timing met the p95 target.
    gpu_p95_target_met: Option<bool>,
    scenes: Vec<G2SceneSummary>,
}

/// The G2 still scenes covered by increment 1. Exterior daylight terrain and
/// the moving-debris / active-collapse sequences are increment 2.
fn g2_frame_scenes(aspect: f32) -> Vec<(String, Scene)> {
    let mut scenes: Vec<(String, Scene)> = Vec::new();
    for room in colored_rooms(aspect) {
        scenes.push((room.name.to_string(), room.scene));
    }
    let eo = emitter_occlusion_scenes(aspect);
    scenes.push(("emitter_occlusion_lit".to_string(), eo.lit));
    scenes.push(("emitter_occlusion_occluded".to_string(), eo.occluded));
    scenes
}

fn run_g2_frames(args: &Args) -> Result<G2FramesSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    // The gate fixes 1920x1080; `--width`/`--height` do not apply to this mode.
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);
    let opts = FrameSeriesOptions {
        width,
        height,
        exposure: 1.0,
        warmup_frames: args.g2_warmup_frames,
        measured_frames: args.g2_measured_frames,
    };

    let mut scene_summaries = Vec::new();
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut worst_p95: Option<f64> = None;
    let mut any_timed = false;
    let mut all_met = true;

    for (name, scene) in g2_frame_scenes(aspect) {
        let report = capture_frame_series(&ctx, &scene, &args.out.join(&name), &opts)?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();

        let scene_met = report.passes.gpu_total.map(|t| {
            any_timed = true;
            worst_p95 = Some(worst_p95.map_or(t.p95_millis, |w| w.max(t.p95_millis)));
            let met = t.p95_millis <= G2_GPU_P95_TARGET_MS;
            all_met &= met;
            met
        });

        scene_summaries.push(G2SceneSummary {
            name,
            indirect_cells: report.indirect_cells,
            gpu_timing_available: report.gpu_timing_available,
            gpu_total: report.passes.gpu_total.map(Into::into),
            gpu_indirect_trace: report.passes.indirect_trace.map(Into::into),
            gpu_indirect_denoise: report.passes.indirect_denoise.map(Into::into),
            gpu_shadow: report.passes.shadow.map(Into::into),
            gpu_opaque: report.passes.opaque.map(Into::into),
            gpu_tone_map: report.passes.tone_map.map(Into::into),
            gpu_p95_target_met: scene_met,
            first_image: report.first_image.display().to_string(),
            last_image: report.last_image.display().to_string(),
        });
    }

    Ok(G2FramesSummary {
        version: 1,
        mode: "g2-frames",
        adapter,
        backend,
        width,
        height,
        exposure: 1.0,
        warmup_frames: opts.warmup_frames,
        measured_frames: opts.measured_frames,
        gpu_timing_available: any_timed,
        gpu_p95_target_millis: G2_GPU_P95_TARGET_MS,
        worst_gpu_total_p95_millis: worst_p95,
        gpu_p95_target_met: any_timed.then_some(all_met),
        scenes: scene_summaries,
    })
}

// --- `--scene g2-loop` (T15 / ENG-22 increment 2) --------------------------

/// Provisional G2 client / GPU / CPU frame p95 targets (`docs/validation.md` "G2").
const G2_CLIENT_FRAME_TARGET_MS: f64 = 16.7;
const G2_CPU_FRAME_TARGET_MS: f64 = 4.0;

#[derive(Serialize)]
struct G2LoopRun {
    /// `settled` (nothing re-traced) or `edit` (a per-frame re-trace box).
    mode: &'static str,
    retrace_edge_cells: u32,
    temporal_weight: f32,
    gpu_timing_available: bool,
    /// One-off shadow-cascade render; a settled frame does not re-cast it.
    shadow_once_millis: Option<f64>,
    /// Per measured frame: indirect trace + denoise + temporal + opaque + tone map.
    gpu_frame: Option<G2StatBlock>,
    gpu_indirect_trace: Option<G2StatBlock>,
    gpu_indirect_denoise: Option<G2StatBlock>,
    gpu_indirect_temporal: Option<G2StatBlock>,
    gpu_opaque: Option<G2StatBlock>,
    gpu_tone_map: Option<G2StatBlock>,
    /// Renderer per-frame CPU encode cost (no wait, no readback).
    cpu_frame: Option<G2StatBlock>,
    /// Directly-paired per-frame CPU encode + GPU device total (each frame's own
    /// two measurements summed, then percentiled) — the measured serial
    /// submit-then-sync frame duration. p99 / max included for the tail.
    serial_frame: Option<G2StatBlock>,
    /// `max(gpu_frame.p95, cpu_frame.p95)` — a pipelined client's frame p95
    /// lower bound (CPU frame N+1 overlaps GPU frame N). Estimate, not evidence.
    client_frame_p95_pipelined_ms: Option<f64>,
    /// `serial_frame.p95` — the measured serial frame p95, a true upper bound on
    /// a pipelined client. This is **not** `gpu_frame.p95 + cpu_frame.p95`; a
    /// sum of marginal percentiles is neither measured nor a valid bound.
    client_frame_p95_serial_ms: Option<f64>,
    gpu_p95_target_met: Option<bool>,
    cpu_p95_target_met: Option<bool>,
    /// Measured serial client-frame p95 <= 16.7 ms.
    client_p95_target_met: Option<bool>,
    first_image: String,
    last_image: String,
}

#[derive(Serialize)]
struct G2LoopSceneSummary {
    name: String,
    indirect_cells: usize,
    runs: Vec<G2LoopRun>,
}

#[derive(Serialize)]
struct G2LoopSummary {
    /// Version 1: T15 increment 2 — persistent-resource settled-frame loop.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    exposure: f32,
    warmup_frames: u32,
    measured_frames: u32,
    gpu_timing_available: bool,
    gpu_p95_target_millis: f64,
    cpu_p95_target_millis: f64,
    client_p95_target_millis: f64,
    /// Worst measured serial client-frame p95 across every scene/mode.
    worst_client_frame_p95_serial_ms: Option<f64>,
    /// `true` iff every scene/mode with GPU timing met the measured serial
    /// client-frame p95 target.
    client_p95_target_met: Option<bool>,
    scenes: Vec<G2LoopSceneSummary>,
}

fn g2_loop_run(r: spall_render::FrameLoopReport, mode: &'static str) -> (G2LoopRun, Option<f64>) {
    let gpu_p95 = r.gpu_frame.map(|s| s.p95_millis);
    let cpu_p95 = r.cpu_frame.map(|s| s.p95_millis);
    let pipelined = match (gpu_p95, cpu_p95) {
        (Some(g), Some(c)) => Some(g.max(c)),
        _ => None,
    };
    // The measured serial frame p95 — each frame's own CPU + GPU total,
    // percentiled — not a sum of the two marginal p95 values.
    let serial = r.serial_frame.map(|s| s.p95_millis);
    let run = G2LoopRun {
        mode,
        retrace_edge_cells: r.retrace_edge_cells,
        temporal_weight: r.temporal_weight,
        gpu_timing_available: r.gpu_timing_available,
        shadow_once_millis: r.shadow_once_millis,
        gpu_frame: r.gpu_frame.map(Into::into),
        gpu_indirect_trace: r.gpu_indirect_trace.map(Into::into),
        gpu_indirect_denoise: r.gpu_indirect_denoise.map(Into::into),
        gpu_indirect_temporal: r.gpu_indirect_temporal.map(Into::into),
        gpu_opaque: r.gpu_opaque.map(Into::into),
        gpu_tone_map: r.gpu_tone_map.map(Into::into),
        cpu_frame: r.cpu_frame.map(Into::into),
        serial_frame: r.serial_frame.map(Into::into),
        client_frame_p95_pipelined_ms: pipelined,
        client_frame_p95_serial_ms: serial,
        gpu_p95_target_met: gpu_p95.map(|v| v <= G2_GPU_P95_TARGET_MS),
        cpu_p95_target_met: cpu_p95.map(|v| v <= G2_CPU_FRAME_TARGET_MS),
        client_p95_target_met: serial.map(|v| v <= G2_CLIENT_FRAME_TARGET_MS),
        first_image: r.first_image.display().to_string(),
        last_image: r.last_image.display().to_string(),
    };
    (run, serial)
}

fn run_g2_loop(args: &Args) -> Result<G2LoopSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    // The gate fixes 1920x1080; `--width`/`--height` do not apply to this mode.
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);

    let base = FrameLoopOptions {
        width,
        height,
        exposure: 1.0,
        warmup_frames: args.g2_loop_warmup,
        measured_frames: args.g2_loop_frames,
        retrace_edge_cells: 0,
        temporal_weight: 0.1,
    };
    // Two re-trace modes per scene: a settled frame with no lighting change, and
    // a per-frame incremental edit box (a stand-in for moving debris / an edit).
    let modes: [(&'static str, u32); 2] = [("settled", 0), ("edit", args.g2_loop_edit_edge)];

    let mut scene_summaries = Vec::new();
    let (mut adapter, mut backend) = (String::new(), String::new());
    let mut worst_serial: Option<f64> = None;
    let mut any_timed = false;
    let mut all_met = true;

    for (name, scene) in g2_frame_scenes(aspect) {
        let mut runs = Vec::new();
        let mut indirect_cells = 0usize;
        for (mode, edge) in modes {
            let opts = FrameLoopOptions {
                retrace_edge_cells: edge,
                ..base
            };
            let report = capture_frame_loop(&ctx, &scene, &args.out.join(&name).join(mode), &opts)?;
            adapter = report.adapter.clone();
            backend = report.backend.clone();
            indirect_cells = report.indirect_cells;
            let (run, serial) = g2_loop_run(report, mode);
            if let Some(p) = serial {
                any_timed = true;
                worst_serial = Some(worst_serial.map_or(p, |w| w.max(p)));
                all_met &= p <= G2_CLIENT_FRAME_TARGET_MS;
            }
            runs.push(run);
        }
        scene_summaries.push(G2LoopSceneSummary {
            name,
            indirect_cells,
            runs,
        });
    }

    Ok(G2LoopSummary {
        version: 1,
        mode: "g2-loop",
        adapter,
        backend,
        width,
        height,
        exposure: 1.0,
        warmup_frames: base.warmup_frames,
        measured_frames: base.measured_frames,
        gpu_timing_available: any_timed,
        gpu_p95_target_millis: G2_GPU_P95_TARGET_MS,
        cpu_p95_target_millis: G2_CPU_FRAME_TARGET_MS,
        client_p95_target_millis: G2_CLIENT_FRAME_TARGET_MS,
        worst_client_frame_p95_serial_ms: worst_serial,
        client_p95_target_met: any_timed.then_some(all_met),
        scenes: scene_summaries,
    })
}

// --- `--scene g2-motion` (T15 / ENG-22 increment 3) -----------------------

/// Mean abs frame-to-frame change over mean level above which a band is flagged
/// for review as unstable / noisy.
const G2_FLICKER_FLAG: f32 = 0.03;
/// Fractional gap from the pre-disturbance level above which the band is flagged
/// as a ghost / light trail.
const G2_GHOST_RESIDUAL_FLAG: f32 = 0.05;
/// Minimum fractional brightening of the shadow band when the occluder leaves;
/// below it the lighting cache is not tracking the move.
const G2_RECOVERY_MIN_GAIN: f32 = 0.05;

fn mean(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().copied().sum::<f32>() / xs.len() as f32
}

fn lerp_box(a: (Vec3, Vec3), b: (Vec3, Vec3), t: f32) -> (Vec3, Vec3) {
    (a.0.lerp(b.0, t), a.1.lerp(b.1, t))
}

#[derive(Serialize)]
struct G2BandTrace {
    name: String,
    band: [f32; 2],
    flicker_index: f32,
    max_step_fraction: f32,
    luminance: Vec<f32>,
}

#[derive(Serialize)]
struct G2MotionRun {
    /// `static-noise`, `moving-occluder` (smooth), or `occluder-jump` (discrete).
    name: &'static str,
    /// `shaded` (the real client frame) or `indirect-only` (transported indirect
    /// radiance isolated — the sensitive view for a lighting change).
    view: &'static str,
    frames: usize,
    gpu_timing_available: bool,
    gpu_frame: Option<G2StatBlock>,
    cpu_frame: Option<G2StatBlock>,
    bands: Vec<G2BandTrace>,
    /// `moving-occluder` only: shadow-band level with the occluder in / out of
    /// the light path, and after it returns.
    occluded_level: Option<f32>,
    clear_level: Option<f32>,
    return_level: Option<f32>,
    /// `(clear - occluded) / occluded` — the shadow band must brighten when the
    /// occluder leaves.
    recovery_gain: Option<f32>,
    /// `|return - occluded| / occluded` — must be ~0 (shadow re-forms exactly).
    return_residual: Option<f32>,
    /// First frame the shadow band reached and held near `clear_level`.
    settle_frames_out: Option<usize>,
    images: Vec<String>,
    /// Anything crossing a documented threshold, phrased for the gate reviewer.
    quality_flags: Vec<String>,
}

#[derive(Serialize)]
struct G2MotionSummary {
    /// Version 1: T15 increment 3 — moving-frame sequences + quality flags.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    frames: u32,
    flicker_flag_threshold: f32,
    ghost_residual_flag_threshold: f32,
    recovery_min_gain: f32,
    /// Every run's `quality_flags`, flattened. Empty ⇒ nothing to review.
    quality_flags: Vec<String>,
    runs: Vec<G2MotionRun>,
}

fn g2_band_trace(t: &spall_render::BandTrace) -> G2BandTrace {
    G2BandTrace {
        name: t.name.clone(),
        band: t.band,
        flicker_index: flicker_index(&t.luminance),
        max_step_fraction: spall_render::max_step_fraction(&t.luminance),
        luminance: t.luminance.clone(),
    }
}

fn run_g2_motion(args: &Args) -> Result<G2MotionSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);
    let n = args.g2_motion_frames as usize;
    // The static-noise run renders the real `Shaded` frame; the occluder runs
    // render `IndirectOnly` so the moving shadow is not swamped by direct light.
    let shaded_opts = MotionSequenceOptions {
        width,
        height,
        exposure: 1.0,
        view: DebugView::Shaded,
        temporal_weight: 0.1,
        halo_cells: 12,
        png_stride: (n / 6).max(1),
    };
    let indirect_opts = MotionSequenceOptions {
        view: DebugView::IndirectOnly,
        ..shaded_opts
    };

    let scenes = emitter_occlusion_scenes(aspect);
    let shadow_band = scenes.receiver_band;
    let lit_band = [0.55_f32, 0.92];
    let far_band = [0.78_f32, 0.98];

    let mut runs = Vec::new();
    let mut all_flags = Vec::new();
    let adapter;
    let backend;

    // --- Run 1: a static camera and world for `n` identical frames. With
    // nothing moving, any frame-to-frame band change is temporal noise /
    // instability in the trace + denoise + temporal pipeline.
    {
        let base = emitter_occlusion_scenes(aspect).occluded;
        let frames: Vec<MotionFrame> = (0..n)
            .map(|_| MotionFrame {
                camera: base.camera,
                update: LightingUpdate::new(),
            })
            .collect();
        let probes = [
            ProbeBand {
                name: "shadow".into(),
                band: shadow_band,
            },
            ProbeBand {
                name: "lit".into(),
                band: lit_band,
            },
        ];
        let report = capture_motion_sequence(
            &ctx,
            &base,
            &frames,
            &probes,
            &args.out.join("static-noise"),
            &shaded_opts,
        )?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();

        let mut flags = Vec::new();
        for band in &report.bands {
            let fi = flicker_index(&band.luminance);
            if fi > G2_FLICKER_FLAG {
                flags.push(format!(
                    "static-noise: `{}` band is not steady with nothing moving (flicker index {fi:.4} > {G2_FLICKER_FLAG}) — temporal noise / instability",
                    band.name
                ));
            }
        }
        all_flags.extend(flags.iter().cloned());
        runs.push(G2MotionRun {
            name: "static-noise",
            view: "shaded",
            frames: report.frames,
            gpu_timing_available: report.gpu_timing_available,
            gpu_frame: report.gpu_frame.map(Into::into),
            cpu_frame: report.cpu_frame.map(Into::into),
            bands: report.bands.iter().map(g2_band_trace).collect(),
            occluded_level: None,
            clear_level: None,
            return_level: None,
            recovery_gain: None,
            return_residual: None,
            settle_frames_out: None,
            images: report
                .images
                .iter()
                .map(|i| i.path.display().to_string())
                .collect(),
            quality_flags: flags,
        });
    }

    // --- Runs 2 & 3: an occluder leaves the emitter -> receiver path and
    // returns, once as a smooth per-frame move (bounded re-trace follows a
    // narrow moving slice) and once as two discrete jumps (each `moving_box`
    // dirties the whole vacated corridor). Comparing the two isolates whether a
    // stale shadow is the documented halo limit or a real defect.
    let in_path = (OCCLUDER_MIN_M, OCCLUDER_MAX_M);
    let clear = (
        OCCLUDER_MIN_M + Vec3::new(-14.0, 0.0, 0.0),
        OCCLUDER_MAX_M + Vec3::new(-14.0, 0.0, 0.0),
    );
    let receiver = LightingRegion {
        min_m: RECEIVER_MIN_M,
        max_m: RECEIVER_MAX_M,
        material: 1,
    };
    let occluder_probes = [
        ProbeBand {
            name: "shadow".into(),
            band: shadow_band,
        },
        ProbeBand {
            name: "far".into(),
            band: far_band,
        },
    ];

    // Smooth: box position eased out over 0..40%, held, eased back 50..90%, held.
    let smooth_t = |i: usize| -> f32 {
        let f = i as f32 / (n.max(2) - 1) as f32;
        let t = if f < 0.40 {
            f / 0.40
        } else if f < 0.50 {
            1.0
        } else if f < 0.90 {
            1.0 - (f - 0.50) / 0.40
        } else {
            0.0
        };
        t.clamp(0.0, 1.0)
    };
    // Discrete: fully out at 20%, fully back at 70% — one big `moving_box` each.
    let jump_t = |i: usize| -> f32 {
        let f = i as f32 / (n.max(2) - 1) as f32;
        if (0.20..0.70).contains(&f) { 1.0 } else { 0.0 }
    };

    for (name, dir) in [
        ("moving-occluder", &smooth_t as &dyn Fn(usize) -> f32),
        ("occluder-jump", &jump_t as &dyn Fn(usize) -> f32),
    ] {
        let base = emitter_occlusion_scenes(aspect).occluded;
        let cam = base.camera;
        let frames: Vec<MotionFrame> = (0..n)
            .map(|i| {
                let prev = lerp_box(in_path, clear, dir(i.saturating_sub(1)));
                let next = lerp_box(in_path, clear, dir(i));
                MotionFrame {
                    camera: cam,
                    update: LightingUpdate::moving_box(prev, next, 1, &[receiver]),
                }
            })
            .collect();
        let report = capture_motion_sequence(
            &ctx,
            &base,
            &frames,
            &occluder_probes,
            &args.out.join(name),
            &indirect_opts,
        )?;

        let shadow = &report.bands[0].luminance;
        let far = &report.bands[1].luminance;
        let win = |lo: f32, hi: f32| -> f32 {
            let lo = (lo * n as f32) as usize;
            let hi = ((hi * n as f32) as usize).clamp(lo + 1, shadow.len());
            mean(&shadow[lo..hi])
        };
        let occluded_level = win(0.0, 0.06);
        // Sample the shadow band mid-way through each "held clear" window.
        let clear_level = if name == "occluder-jump" {
            win(0.40, 0.55)
        } else {
            win(0.42, 0.49)
        };
        let return_level = win(0.94, 1.0);
        let frac = |num: f32| {
            if occluded_level.abs() > f32::EPSILON {
                num / occluded_level
            } else {
                0.0
            }
        };
        let recovery_gain = frac(clear_level - occluded_level);
        let return_residual = frac((return_level - occluded_level).abs());
        // Frames for the shadow band to reach and hold near `clear_level`,
        // measured over the first half (before the occluder starts back).
        let settle_frames_out = spall_render::settle_index(
            &shadow[..(n / 2).min(shadow.len())],
            clear_level,
            0.06,
            (0.06 * n as f32) as usize,
        );
        let far_flicker = flicker_index(far);

        let mut flags = Vec::new();
        if recovery_gain < G2_RECOVERY_MIN_GAIN {
            flags.push(format!(
                "{name}: shadow band barely brightened when the occluder left (recovery gain {recovery_gain:.4} < {G2_RECOVERY_MIN_GAIN})"
            ));
        }
        if return_residual > G2_GHOST_RESIDUAL_FLAG {
            flags.push(format!(
                "{name}: shadow band did not return after the occluder came back (residual {return_residual:.4} > {G2_GHOST_RESIDUAL_FLAG}) — possible ghost / light trail"
            ));
        }
        if far_flicker > G2_FLICKER_FLAG {
            flags.push(format!(
                "{name}: static `far` band flickers during the move (flicker index {far_flicker:.4} > {G2_FLICKER_FLAG}) — temporal noise"
            ));
        }
        all_flags.extend(flags.iter().cloned());
        runs.push(G2MotionRun {
            name,
            view: "indirect-only",
            frames: report.frames,
            gpu_timing_available: report.gpu_timing_available,
            gpu_frame: report.gpu_frame.map(Into::into),
            cpu_frame: report.cpu_frame.map(Into::into),
            bands: report.bands.iter().map(g2_band_trace).collect(),
            occluded_level: Some(occluded_level),
            clear_level: Some(clear_level),
            return_level: Some(return_level),
            recovery_gain: Some(recovery_gain),
            return_residual: Some(return_residual),
            settle_frames_out,
            images: report
                .images
                .iter()
                .map(|i| i.path.display().to_string())
                .collect(),
            quality_flags: flags,
        });
    }

    Ok(G2MotionSummary {
        version: 1,
        mode: "g2-motion",
        adapter,
        backend,
        width,
        height,
        frames: args.g2_motion_frames,
        flicker_flag_threshold: G2_FLICKER_FLAG,
        ghost_residual_flag_threshold: G2_GHOST_RESIDUAL_FLAG,
        recovery_min_gain: G2_RECOVERY_MIN_GAIN,
        quality_flags: all_flags,
        runs,
    })
}

// --- `--scene g2-terrain` (T15 / ENG-22 increment 4) ----------------------

#[derive(Serialize)]
struct G2TerrainSummary {
    /// Version 1: T15 increment 4 — open daylight-terrain scene.
    version: u32,
    mode: &'static str,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    indirect_cells: usize,
    gpu_p95_target_millis: f64,
    cpu_p95_target_millis: f64,
    client_p95_target_millis: f64,
    /// Settled-frame and per-frame-edit cost on the persistent-resource loop
    /// (same shape as `g2-loop`).
    settled: G2LoopRun,
    edit: G2LoopRun,
    /// 120-frame static-noise stability of the `Shaded` terrain frame.
    lit_band_flicker: f32,
    shadow_band_flicker: f32,
    lit_band_max_step: f32,
    shadow_band_max_step: f32,
    quality_flags: Vec<String>,
}

fn run_g2_terrain(args: &Args) -> Result<G2TerrainSummary, RenderError> {
    let ctx = RenderContext::headless()?;
    let (width, height) = (1920u32, 1080u32);
    let aspect = capture_aspect(width, height);
    let terrain = daylight_terrain_scene(aspect);

    let settled_opts = FrameLoopOptions {
        width,
        height,
        exposure: 1.0,
        warmup_frames: args.g2_loop_warmup,
        measured_frames: args.g2_loop_frames,
        retrace_edge_cells: 0,
        temporal_weight: 0.1,
    };
    let edit_opts = FrameLoopOptions {
        retrace_edge_cells: args.g2_loop_edit_edge,
        ..settled_opts
    };

    let settled_report = capture_frame_loop(
        &ctx,
        &terrain.scene,
        &args.out.join("settled"),
        &settled_opts,
    )?;
    let indirect_cells = settled_report.indirect_cells;
    let adapter = settled_report.adapter.clone();
    let backend = settled_report.backend.clone();
    let (settled, _) = g2_loop_run(settled_report, "settled");

    let edit_report = capture_frame_loop(&ctx, &terrain.scene, &args.out.join("edit"), &edit_opts)?;
    let (edit, _) = g2_loop_run(edit_report, "edit");

    // Stability: 120 identical Shaded frames of the terrain.
    let n = args.g2_motion_frames as usize;
    let motion_opts = MotionSequenceOptions {
        width,
        height,
        exposure: 1.0,
        view: DebugView::Shaded,
        temporal_weight: 0.1,
        halo_cells: 12,
        png_stride: (n / 4).max(1),
    };
    let frames: Vec<MotionFrame> = (0..n)
        .map(|_| MotionFrame {
            camera: terrain.scene.camera,
            update: LightingUpdate::new(),
        })
        .collect();
    let probes = [
        ProbeBand {
            name: "lit".into(),
            band: terrain.lit_band,
        },
        ProbeBand {
            name: "shadow".into(),
            band: terrain.shadow_band,
        },
    ];
    let noise = capture_motion_sequence(
        &ctx,
        &terrain.scene,
        &frames,
        &probes,
        &args.out.join("static-noise"),
        &motion_opts,
    )?;
    let lit = &noise.bands[0];
    let shadow = &noise.bands[1];
    let lit_band_flicker = flicker_index(&lit.luminance);
    let shadow_band_flicker = flicker_index(&shadow.luminance);
    let lit_band_max_step = spall_render::max_step_fraction(&lit.luminance);
    let shadow_band_max_step = spall_render::max_step_fraction(&shadow.luminance);

    let mut quality_flags = Vec::new();
    for (mode, run) in [("settled", &settled), ("edit", &edit)] {
        if run.gpu_p95_target_met == Some(false) {
            quality_flags.push(format!(
                "g2-terrain/{mode}: GPU frame p95 over the provisional target"
            ));
        }
        if run.cpu_p95_target_met == Some(false) {
            quality_flags.push(format!(
                "g2-terrain/{mode}: CPU frame p95 over the provisional target"
            ));
        }
        if run.client_p95_target_met == Some(false) {
            quality_flags.push(format!(
                "g2-terrain/{mode}: measured serial client-frame p95 over the provisional target"
            ));
        }
    }
    for (name, fi) in [("lit", lit_band_flicker), ("shadow", shadow_band_flicker)] {
        if fi > G2_FLICKER_FLAG {
            quality_flags.push(format!(
                "g2-terrain: `{name}` band is not steady with nothing moving (flicker index {fi:.4} > {G2_FLICKER_FLAG}) — temporal noise / instability"
            ));
        }
    }

    Ok(G2TerrainSummary {
        version: 1,
        mode: "g2-terrain",
        adapter,
        backend,
        width,
        height,
        indirect_cells,
        gpu_p95_target_millis: G2_GPU_P95_TARGET_MS,
        cpu_p95_target_millis: G2_CPU_FRAME_TARGET_MS,
        client_p95_target_millis: G2_CLIENT_FRAME_TARGET_MS,
        settled,
        edit,
        lit_band_flicker,
        shadow_band_flicker,
        lit_band_max_step,
        shadow_band_max_step,
        quality_flags,
    })
}

/// Serialise `result` to `<out>/summary.json` and turn it into a process exit
/// code, so every scene mode shares one write + error path.
fn finish<T: Serialize>(out: &std::path::Path, result: Result<T, RenderError>) -> ExitCode {
    let summary = match result {
        Ok(summary) => summary,
        Err(RenderError::NoAdapter) => {
            eprintln!(
                "sandbox-capture: no compatible GPU adapter; offscreen capture needs a working GPU/driver"
            );
            return ExitCode::from(3);
        }
        Err(error) => {
            eprintln!("sandbox-capture: {error}");
            return ExitCode::from(1);
        }
    };
    let path = out.join("summary.json");
    let body = match serde_json::to_vec_pretty(&summary) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("sandbox-capture: cannot serialise summary: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = std::fs::write(&path, body) {
        eprintln!("sandbox-capture: cannot write {}: {error}", path.display());
        return ExitCode::from(1);
    }
    println!("sandbox-capture: summary written to {}", path.display());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

    if let Err(error) = std::fs::create_dir_all(&args.out) {
        eprintln!(
            "sandbox-capture: cannot create {}: {error}",
            args.out.display()
        );
        return ExitCode::from(1);
    }

    if args.scene.as_deref() == Some("lighting-sequence") {
        return finish(&args.out, run_lighting_sequence(&args));
    }
    if args.scene.as_deref() == Some("destruction") {
        return finish(&args.out, run_destruction(&args));
    }
    if args.scene.as_deref() == Some("destruction-networked") {
        return finish(&args.out, run_destruction_networked(&args));
    }
    if args.scene.as_deref() == Some("g2-frames") {
        return finish(&args.out, run_g2_frames(&args));
    }
    if args.scene.as_deref() == Some("g2-loop") {
        return finish(&args.out, run_g2_loop(&args));
    }
    if args.scene.as_deref() == Some("g2-motion") {
        return finish(&args.out, run_g2_motion(&args));
    }
    if args.scene.as_deref() == Some("g2-terrain") {
        return finish(&args.out, run_g2_terrain(&args));
    }
    if args.scene.as_deref() == Some("g2-collapse") {
        return finish(&args.out, run_g2_collapse(&args));
    }
    if args.scene.as_deref() == Some("g2-bounded-collapse") {
        return finish(&args.out, run_g2_bounded_collapse(&args));
    }
    finish(&args.out, run(&args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;
    use spall_render::Camera;

    /// Project a world point to pixel coordinates for a `width`x`height` target.
    fn to_pixels(cam: &Camera, width: u32, height: u32, p: Vec3) -> (f32, f32) {
        let clip = cam.view_projection() * p.extend(1.0);
        let ndc = clip.truncate() / clip.w;
        let px = (ndc.x * 0.5 + 0.5) * width as f32;
        let py = (1.0 - (ndc.y * 0.5 + 0.5)) * height as f32;
        (px, py)
    }

    /// Ratio of the on-screen pixel length of equal world-space steps along the
    /// camera's right and up axes. A geometrically faithful capture renders a
    /// world-space square as a pixel square, so this is ~1.0.
    fn right_over_up_pixel_ratio(width: u32, height: u32, aspect: f32) -> f32 {
        let shapes = acceptance_shapes();
        let cube = shapes
            .iter()
            .find(|s| s.name == "cube")
            .expect("cube shape");
        let (scene, _) = build_scene(cube, MeshStrategy::Greedy, aspect);
        let cam = &scene.camera;
        let bounds = scene.world_bounds().expect("framed bounds");
        let centre = (bounds.min + bounds.max) * 0.5;
        let step = ((bounds.max - bounds.min) * 0.5).length() * 0.25;

        let origin = to_pixels(cam, width, height, centre);
        let right = to_pixels(cam, width, height, centre + cam.right() * step);
        let up = to_pixels(cam, width, height, centre + cam.up() * step);
        let dx = (right.0 - origin.0).hypot(right.1 - origin.1);
        let dy = (up.0 - origin.0).hypot(up.1 - origin.1);
        dx / dy
    }

    #[test]
    fn capture_aspect_follows_the_requested_dimensions() {
        assert!((capture_aspect(1280, 720) - 16.0 / 9.0).abs() < 1e-6);
        assert!((capture_aspect(240, 240) - 1.0).abs() < 1e-6);
        assert!((capture_aspect(320, 240) - 4.0 / 3.0).abs() < 1e-6);
    }

    /// The destruction capture's scene builder (GPU-free): drive the authoritative
    /// world through the cut script, and at each capture tick it must mesh the
    /// live terrain plus every detached body into a framed scene.
    #[test]
    fn destruction_scene_meshes_the_live_world_and_frames_it() {
        use spall_sim::{
            EditIntent, EditTarget, RequestId, Simulation, SimulationConfig, fixtures,
        };

        let mut setup = fixtures::cross_brick_bridged_setup();
        setup.physics.disable_ccd = true;
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        let actor = spall_core::EntityId::new(1).unwrap();

        let mut next_cut = 0usize;
        let mut request_id = 1u64;
        let mut camera = None;
        let mut saw_body_item = false;

        for tick in 1..=90u64 {
            while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
                let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
                let _ = sim.submit(EditIntent::cut(
                    RequestId(request_id),
                    actor,
                    EditTarget::Terrain,
                    brush_cell(cell, radius),
                ));
                request_id += 1;
                next_cut += 1;
            }
            sim.tick().unwrap();

            if DESTRUCTION_CAPTURE_TICKS.contains(&tick) {
                let scene = destruction_scene(&sim, MeshStrategy::Greedy, 16.0 / 9.0, camera);
                assert!(
                    scene.items.iter().any(|i| i.name == "terrain"),
                    "tick {tick}: terrain is always meshed"
                );
                if camera.is_none() {
                    // First capture: the camera was framed on the scene bounds.
                    assert!(scene.world_bounds().is_some());
                    camera = Some(scene.camera);
                }
                if sim.world().body_count() > 0 {
                    saw_body_item |= scene.items.iter().any(|i| i.name.starts_with("body_"));
                }
            }
        }

        assert!(
            sim.world().body_count() > 0,
            "the cut script detaches the cross-brick beam"
        );
        assert!(
            saw_body_item,
            "a detached body is meshed into the capture scene at its pose"
        );
    }

    /// The GI-lit collapse scene's clipmap (GPU-free): terrain occupancy is
    /// sampled into solid clipmap cells, the open sky above stays air, and a
    /// terrain cut removes solid cells from the clipmap on the next rebuild.
    #[test]
    fn sim_light_volume_tracks_terrain_occupancy_and_cuts() {
        let mut setup = fixtures::cross_brick_bridged_setup();
        setup.physics.disable_ccd = true;
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        let origin = Vec3::splat(-32.0);
        let terrain_solids = |sim: &Simulation| {
            terrain_light_volume(&sim.world().terrain().volume, origin, MeshStrategy::Greedy)
                .cells()
                .iter()
                .filter(|&&m| m != 0)
                .count()
        };

        let before = sim_light_volume(&sim, origin, MeshStrategy::Greedy);
        let solid_before = terrain_solids(&sim);
        assert!(
            solid_before > 0,
            "the bridge terrain fills solid clipmap cells"
        );

        // Well above the tallest geometry is open air, so this is an exterior.
        let sky = before.world_to_cell(Vec3::new(4.0, 40.0, 4.0));
        let dim = spall_render::LIGHT_VOLUME_DIM as i32;
        let flat = |c: glam::IVec3| (c.x + dim * (c.y + dim * c.z)) as usize;
        assert_eq!(before.cells()[flat(sky)], 0, "open sky above the scene");

        let actor = spall_core::EntityId::new(1).unwrap();
        let mut next_cut = 0usize;
        let mut committed = 0u64;
        for tick in 1..=40u64 {
            while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
                let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
                let _ = sim.submit(EditIntent::cut(
                    RequestId(next_cut as u64 + 1),
                    actor,
                    EditTarget::Terrain,
                    brush_cell(cell, radius),
                ));
                next_cut += 1;
            }
            committed += sim.tick().unwrap().committed.len() as u64;
        }
        assert!(
            committed > 0,
            "the cut script commits terrain damage by tick 40"
        );

        let solid_after = terrain_solids(&sim);
        assert!(
            solid_after < solid_before,
            "cuts remove solid terrain clipmap cells: {solid_before} -> {solid_after}"
        );
    }

    /// Increment 6 (GPU-free): a clipmap fed only bounded per-tick
    /// [`collapse_tick_update`]s — seeded once, then never fully resampled —
    /// must still agree cell-for-cell with a from-scratch resample of the
    /// evolving world. Disagreement would be a stale vacated shadow or an
    /// erased overlapping occupancy.
    #[test]
    fn bounded_collapse_update_tracks_the_full_resample() {
        let mut setup = fixtures::cross_brick_bridged_setup();
        setup.physics.disable_ccd = true;
        let mut sim = Simulation::new(SimulationConfig::new(setup)).unwrap();
        let origin = G2_COLLAPSE_LIGHT_ORIGIN;
        let strategy = MeshStrategy::Greedy;
        let actor = spall_core::EntityId::new(1).unwrap();

        let mut mirror = sim_light_volume(&sim, origin, strategy);
        let seed_solid = mirror.cells().iter().filter(|&&m| m != 0).count();

        let mut next_cut = 0usize;
        let mut request_id = 1u64;
        let mut pending: Vec<(u64, [i64; 3], i64)> = Vec::new();
        let mut prev_bodies: Vec<(Vec3, Vec3)> = Vec::new();
        let mut applied_any_cut = false;
        let mut saw_body = false;

        for tick in 1..=60u64 {
            while next_cut < DESTRUCTION_SCRIPT.len() && DESTRUCTION_SCRIPT[next_cut].0 == tick {
                let (_, cell, radius) = DESTRUCTION_SCRIPT[next_cut];
                let _ = sim.submit(EditIntent::cut(
                    RequestId(request_id),
                    actor,
                    EditTarget::Terrain,
                    brush_cell(cell, radius),
                ));
                pending.push((request_id, cell, radius));
                request_id += 1;
                next_cut += 1;
            }
            let report = sim.tick().unwrap();
            let landed: std::collections::HashSet<u64> =
                report.committed.iter().map(|(id, _)| id.0).collect();
            let mut cuts_this_tick = Vec::new();
            pending.retain(|&(id, cell, radius)| {
                if landed.contains(&id) {
                    cuts_this_tick.push((cell, radius));
                    false
                } else {
                    true
                }
            });
            applied_any_cut |= !cuts_this_tick.is_empty();

            let cur_bodies: Vec<(Vec3, Vec3)> = sim
                .world()
                .bodies()
                .filter_map(|b| body_world_aabb(b, strategy))
                .collect();
            saw_body |= !cur_bodies.is_empty();
            let update = collapse_tick_update(
                &sim.world().terrain().volume,
                origin,
                &cuts_this_tick,
                &prev_bodies,
                &cur_bodies,
            );
            prev_bodies = cur_bodies;
            mirror.apply_update(&update);
            let _ = mirror.take_dirty();

            let solid_now = mirror.cells().iter().filter(|&&m| m != 0).count();
            assert!(
                solid_now <= seed_solid + 8,
                "tick {tick}: bounded clipmap solid count {solid_now} rose above the seed {seed_solid} — over-filled occupancy"
            );
        }

        assert!(
            applied_any_cut,
            "the cut script commits terrain damage by tick 60"
        );
        assert!(
            saw_body,
            "the cut script detaches the cross-brick beam by tick 60"
        );

        let full = sim_light_volume(&sim, origin, strategy);
        let (agree, total) = mirror
            .cells()
            .iter()
            .zip(full.cells().iter())
            .fold((0u64, 0u64), |(a, t), (m, f)| {
                (a + u64::from(m == f), t + 1)
            });
        let frac = agree as f32 / total as f32;
        assert!(
            frac > 0.995,
            "bounded per-tick clipmap tracks a full resample: {frac:.4} cell agreement"
        );
    }

    #[test]
    fn square_target_keeps_world_proportions() {
        let ratio = right_over_up_pixel_ratio(240, 240, capture_aspect(240, 240));
        assert!(
            (ratio - 1.0).abs() < 0.03,
            "square capture distorted, right/up pixel ratio {ratio}"
        );
    }

    #[test]
    fn four_by_three_target_keeps_world_proportions() {
        let ratio = right_over_up_pixel_ratio(320, 240, capture_aspect(320, 240));
        assert!(
            (ratio - 1.0).abs() < 0.03,
            "4:3 capture distorted, right/up pixel ratio {ratio}"
        );
    }

    #[test]
    fn a_fixed_16_9_projection_distorts_a_4_3_target() {
        // Regression witness: the old hard-coded `aspect: 16.0 / 9.0` squashes a
        // 4:3 capture horizontally to (4/3) / (16/9) = 0.75 of its true width.
        let ratio = right_over_up_pixel_ratio(320, 240, 16.0 / 9.0);
        assert!(
            (ratio - 0.75).abs() < 0.03,
            "expected the mismatched projection to squash to ~0.75, got {ratio}"
        );
    }

    #[test]
    fn g2_frame_scenes_are_distinct_lit_and_framed() {
        let scenes = g2_frame_scenes(1920.0 / 1080.0);
        assert_eq!(
            scenes.len(),
            4,
            "increment 1 covers the two rooms and the lit/occluded emitter scenes"
        );
        let names: Vec<&str> = scenes.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"colored_room_open"));
        assert!(names.contains(&"colored_room_closed"));
        assert!(names.contains(&"emitter_occlusion_lit"));
        assert!(names.contains(&"emitter_occlusion_occluded"));
        for (name, scene) in &scenes {
            assert!(!scene.items.is_empty(), "{name}: has geometry to draw");
            assert!(
                scene.lighting.is_some(),
                "{name}: carries a lighting volume"
            );
            assert!(
                scene.world_bounds().is_some(),
                "{name}: framed onto its bounds"
            );
        }
    }

    #[test]
    fn the_framed_shape_stays_inside_the_view() {
        for (w, h) in [(240u32, 240u32), (320, 240), (240, 320)] {
            let shapes = acceptance_shapes();
            let cube = shapes.iter().find(|s| s.name == "cube").unwrap();
            let (scene, _) = build_scene(cube, MeshStrategy::Greedy, capture_aspect(w, h));
            let cam = &scene.camera;
            let item = &scene.items[0];
            let (mut max_x, mut max_y) = (0.0f32, 0.0f32);
            for v in &item.mesh.vertices {
                let world = item.model.transform_point3(Vec3::from_array(v.position));
                let clip = cam.view_projection() * world.extend(1.0);
                let ndc = clip.truncate() / clip.w;
                max_x = max_x.max(ndc.x.abs());
                max_y = max_y.max(ndc.y.abs());
            }
            assert!(
                max_x <= 1.0 && max_y <= 1.0,
                "{w}x{h}: shape clipped, ndc extent ({max_x}, {max_y})"
            );
            assert!(
                max_y > 0.5,
                "{w}x{h}: shape not framed, ndc y extent {max_y}"
            );
        }
    }
}
