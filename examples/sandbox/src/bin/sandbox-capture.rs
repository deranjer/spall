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
use spall_core::{EntityId, SphereBrush};
use spall_jobs::{Generation, TopologyEpoch};
use spall_mesh::fixtures::{AcceptanceShape, acceptance_shapes, mesh_shape};
use spall_mesh::{MeshOptions, MeshStrategy, build_volume_mesh};
use spall_render::{
    Camera, CaptureOptions, DebugView, FrameLoopOptions, FrameSeriesOptions, FrameStats,
    LightingStep, RenderContext, RenderError, Scene, SceneItem, SequenceOptions,
    capture_frame_loop, capture_frame_series, capture_lighting_sequence, capture_scene,
    colored_rooms, emitter_occlusion_scenes, rapid_destruction,
};
use spall_sim::{EditIntent, EditTarget, RequestId, Simulation, SimulationConfig, fixtures};
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
    /// `g2-frames` (T15 cold GPU frame-cost percentiles), or `g2-loop` (T15
    /// persistent-resource settled-frame GPU + CPU percentiles).
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
    /// `max(gpu_frame.p95, cpu_frame.p95)` — a pipelined client's frame p95
    /// lower bound (CPU frame N+1 overlaps GPU frame N).
    client_frame_p95_pipelined_ms: Option<f64>,
    /// `gpu_frame.p95 + cpu_frame.p95` — this serial harness's frame p95, an
    /// upper bound on a pipelined client.
    client_frame_p95_serial_ms: Option<f64>,
    gpu_p95_target_met: Option<bool>,
    cpu_p95_target_met: Option<bool>,
    /// Pipelined client-frame p95 estimate <= 16.7 ms.
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
    /// Worst pipelined client-frame p95 estimate across every scene/mode.
    worst_client_frame_p95_pipelined_ms: Option<f64>,
    /// `true` iff every scene/mode with GPU timing met the pipelined client p95.
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
    let serial = match (gpu_p95, cpu_p95) {
        (Some(g), Some(c)) => Some(g + c),
        _ => None,
    };
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
        client_frame_p95_pipelined_ms: pipelined,
        client_frame_p95_serial_ms: serial,
        gpu_p95_target_met: gpu_p95.map(|v| v <= G2_GPU_P95_TARGET_MS),
        cpu_p95_target_met: cpu_p95.map(|v| v <= G2_CPU_FRAME_TARGET_MS),
        client_p95_target_met: pipelined.map(|v| v <= G2_CLIENT_FRAME_TARGET_MS),
        first_image: r.first_image.display().to_string(),
        last_image: r.last_image.display().to_string(),
    };
    (run, pipelined)
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
    let mut worst_pipelined: Option<f64> = None;
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
            let (run, pipelined) = g2_loop_run(report, mode);
            if let Some(p) = pipelined {
                any_timed = true;
                worst_pipelined = Some(worst_pipelined.map_or(p, |w| w.max(p)));
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
        worst_client_frame_p95_pipelined_ms: worst_pipelined,
        client_p95_target_met: any_timed.then_some(all_met),
        scenes: scene_summaries,
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
    if args.scene.as_deref() == Some("g2-frames") {
        return finish(&args.out, run_g2_frames(&args));
    }
    if args.scene.as_deref() == Some("g2-loop") {
        return finish(&args.out, run_g2_loop(&args));
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
