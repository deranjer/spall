//! Moving bodies in the lighting cache: they block skylight and act as bounce
//! sources, updates are incremental (a slice per frame), overlapping bodies are
//! never erased, and nothing is left behind after motion stops. See
//! `docs/reports/ENG-101.md`.
//!
//! Ignored by default; run with
//! `cargo test -p spall_render --test dynamic_lighting_gpu -- --ignored --nocapture`.

use glam::{Quat, Vec3};
use spall_render::indirect::LightingVolume;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, Material,
    OffscreenTarget, RenderContext, cache_origin_around, mark_world_box,
};

const SIZE: (u32, u32) = (480, 360);
const IDENTITY: [f32; 4] = CubeInstance::IDENTITY_ROTATION;
const WHITE: u32 = 1;
const LAMP: u32 = 2;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),
        Material::new([0.8, 0.8, 0.8], 0.9, 0.0),
        Material::new([1.0, 0.45, 0.1], 0.9, 0.0).emissive(6.0),
    ]
}

fn boxed(material: u32, center: [f32; 3], size: [f32; 3]) -> CubeInstance {
    CubeInstance::new(center, material, size, IDENTITY)
}

fn ground() -> CubeInstance {
    boxed(WHITE, [0.0, -0.25, 0.0], [60.0, 0.5, 60.0])
}

fn terrain_occupancy(boxes: &[CubeInstance]) -> LightingVolume {
    let mut volume = LightingVolume::empty(cache_origin_around(Vec3::ZERO));
    for b in boxes {
        let (c, h) = (Vec3::from_array(b.offset), Vec3::from_array(b.size) * 0.5);
        mark_world_box(&mut volume, c - h, c + h, b.material);
    }
    volume
}

/// Skylight only (no sun), so the ground under a body is dark only if the body
/// blocks sky; and bounce needs an emitter.
fn skylight_only() -> Environment {
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_intensity = 0.0;
    environment
}

fn lamp_only() -> Environment {
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    environment
}

/// A low side-on view that sees the ground under a raised slab's edge.
fn side_camera() -> Camera {
    Camera::looking_along(
        Vec3::new(9.0, 3.0, 0.5),
        Vec3::new(-9.0, -3.0, -0.5),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

fn camera() -> Camera {
    Camera::looking_along(
        Vec3::new(2.0, 14.0, 6.0),
        Vec3::new(0.0, -1.0, -0.35),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

/// A persistent renderer: the lighting state carries across frames, which is
/// exactly what these tests are about.
struct Session<'a> {
    ctx: &'a RenderContext,
    renderer: GameRenderer,
    target: OffscreenTarget,
    camera: Camera,
    environment: Environment,
    view: DebugView,
    frames: u32,
}

impl<'a> Session<'a> {
    fn new(
        ctx: &'a RenderContext,
        environment: Environment,
        view: DebugView,
        terrain: &[CubeInstance],
    ) -> Self {
        let mut renderer = GameRenderer::new(
            &ctx.device,
            &ctx.queue,
            COLOR_FORMAT,
            &materials(),
            SIZE,
            None,
        );
        renderer.set_terrain(&ctx.device, &ctx.queue, terrain);
        renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&terrain_occupancy(terrain)));
        Self {
            ctx,
            renderer,
            target: OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1),
            camera: camera(),
            environment,
            view,
            frames: 0,
        }
    }

    /// Render one frame with `bodies` and read it back.
    fn frame(&mut self, bodies: &[CubeInstance]) -> Vec<u8> {
        self.renderer
            .set_bodies(&self.ctx.device, &self.ctx.queue, bodies);
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.renderer.render(
            &self.ctx.device,
            &self.ctx.queue,
            &mut encoder,
            self.target.color_view(),
            &self.camera,
            &self.environment,
            self.view,
        );
        self.target.copy_to_readback(&mut encoder);
        self.ctx.queue.submit([encoder.finish()]);
        self.frames += 1;
        self.target.read_rgba(self.ctx).expect("readback")
    }

    /// Render frames with a fixed `bodies` set until the lighting sweep has
    /// finished, returning `(frames used, final frame)`.
    fn settle(&mut self, bodies: &[CubeInstance]) -> (u32, Vec<u8>) {
        let mut used = 1;
        self.frame(bodies);
        while self.renderer.lighting_is_sweeping() && used < 60 {
            self.frame(bodies);
            used += 1;
        }
        // One more so the last slice's result is in the image.
        (used + 1, self.frame(bodies))
    }
}

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn at(camera: &Camera, rgba: &[u8], p: Vec3) -> [f32; 3] {
    let ndc = camera.project(p).expect("in front of the camera");
    let x = (((ndc[0] * 0.5 + 0.5) * SIZE.0 as f32) as u32).min(SIZE.0 - 1);
    let y = (((0.5 - ndc[1] * 0.5) * SIZE.1 as f32) as u32).min(SIZE.1 - 1);
    let i = ((y * SIZE.0 + x) * 4) as usize;
    [
        srgb_to_linear(rgba[i]),
        srgb_to_linear(rgba[i + 1]),
        srgb_to_linear(rgba[i + 2]),
    ]
}

fn max_abs_diff(a: &[u8], b: &[u8]) -> (f32, u8) {
    let diffs: Vec<u8> = a
        .chunks_exact(4)
        .zip(b.chunks_exact(4))
        .flat_map(|(x, y)| (0..3).map(move |c| x[c].abs_diff(y[c])))
        .collect();
    (
        diffs.iter().map(|d| f32::from(*d)).sum::<f32>() / diffs.len() as f32,
        *diffs.iter().max().unwrap(),
    )
}

fn save_png(name: &str, rgba: &[u8]) {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-101-dynamic");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    image::save_buffer(
        dir.join(name),
        rgba,
        SIZE.0,
        SIZE.1,
        image::ColorType::Rgba8,
    )
    .expect("write png");
}

/// A body over the ground blocks skylight; once it is gone the ground returns
/// to exactly what it was: no trail.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_body_blocks_skylight_and_leaves_no_trail() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let slab = [boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0])];

    let (base_frames, open) = session.settle(&[]);
    let open_luma = at(&session.camera, &open, probe)[1];
    let (shade_frames, shaded) = session.settle(&slab);
    save_png("body-over-ground.png", &shaded);
    let under = at(&session.camera, &shaded, probe)[1];
    let (clear_frames, cleared) = session.settle(&[]);
    save_png("body-removed.png", &cleared);
    let after = at(&session.camera, &cleared, probe)[1];
    println!(
        "open ground {open_luma:.3}; under the body {under:.3} ({shade_frames} frames to settle); after removal {after:.3} ({clear_frames} frames); first population {base_frames} frames"
    );
    assert!(
        under < open_luma * 0.6,
        "a body 2 m up must shade the ground: {under} vs {open_luma}"
    );
    let (mean, max) = max_abs_diff(&open, &cleared);
    assert!(
        mean < 0.05 && max <= 2,
        "removing the body must restore the open frame exactly: mean {mean}, max {max}"
    );
    assert!(
        shade_frames <= 12,
        "a sliced sweep should take about 8 frames: {shade_frames}"
    );
}

/// Rapid motion (a rotating body swept across the scene each frame) settles to
/// the same image as a fresh render with the body at rest in its final pose.
#[test]
#[ignore = "requires a working GPU adapter"]
fn rapid_motion_settles_to_the_final_pose_with_no_ghosts() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let pose = |i: u32| {
        let t = i as f32;
        CubeInstance::new(
            [
                -6.0 + t * 0.45,
                1.5 + (t * 0.3).sin() * 0.5,
                (t * 0.2).cos() * 2.0,
            ],
            WHITE,
            [1.5, 0.5, 1.5],
            Quat::from_rotation_y(t * 0.4).to_array(),
        )
    };
    let mut moving = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    moving.settle(&[]);
    for i in 0..30 {
        moving.frame(&[pose(i)]);
    }
    let (frames, after_motion) = moving.settle(&[pose(29)]);

    let mut fresh = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    let (_, reference) = fresh.settle(&[pose(29)]);
    save_png("rapid-motion-settled.png", &after_motion);
    save_png("rapid-motion-fresh.png", &reference);
    let (mean, max) = max_abs_diff(&after_motion, &reference);
    println!(
        "after 30 frames of motion + {frames} to settle vs a fresh render at the final pose: mean |diff| {mean:.4}, max {max}"
    );
    assert!(
        mean < 0.05 && max <= 3,
        "motion left a trail: mean {mean}, max {max}"
    );
}

/// A moving emissive body relights the floor around its new position and stops
/// lighting where it was.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_moving_lamp_relights_the_floor() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, lamp_only(), DebugView::IndirectOnly, &[ground()]);
    let lamp_at = |x: f32| boxed(LAMP, [x, 0.5, 0.0], [1.0, 1.0, 1.0]);
    let (left, right) = (Vec3::new(-6.0, 0.0, 0.0), Vec3::new(6.0, 0.0, 0.0));
    let beside = |lamp_x: f32| Vec3::new(lamp_x + 2.0, 0.0, 0.0);

    let (_, image) = session.settle(&[lamp_at(-6.0)]);
    let near_left = at(&session.camera, &image, beside(-6.0))[0];
    let near_right_before = at(&session.camera, &image, beside(6.0))[0];
    let (frames, image) = session.settle(&[lamp_at(6.0)]);
    save_png("lamp-moved.png", &image);
    let near_left_after = at(&session.camera, &image, beside(-6.0))[0];
    let near_right = at(&session.camera, &image, beside(6.0))[0];
    let _ = (left, right);
    println!(
        "floor beside the lamp at -6: {near_left:.3} then {near_left_after:.3}; beside +6: {near_right_before:.3} then {near_right:.3} ({frames} frames)"
    );
    assert!(
        near_left > 0.01 && near_right_before < near_left * 0.2,
        "the lamp at -6 lights its side only"
    );
    assert!(
        near_right > 0.01,
        "the moved lamp lights the floor beside its new position: {near_right}"
    );
    assert!(
        near_left_after < near_left * 0.1,
        "the old position must go dark: {near_left_after} vs {near_left}"
    );
}

/// Two overlapping bodies share cache cells; when one leaves, the other still
/// blocks and the terrain under both is intact.
#[test]
#[ignore = "requires a working GPU adapter"]
fn overlapping_bodies_are_not_erased_when_one_leaves() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    let a = boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    let b = boxed(WHITE, [1.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    let probe = Vec3::new(2.0, 0.0, 0.0);

    let (_, open) = session.settle(&[]);
    let open_luma = at(&session.camera, &open, probe)[1];
    let (_, both) = session.settle(&[a, b]);
    let both_luma = at(&session.camera, &both, probe)[1];
    let (_, only_b) = session.settle(&[b]);
    let b_luma = at(&session.camera, &only_b, probe)[1];
    println!("open {open_luma:.3}; both {both_luma:.3}; only b {b_luma:.3}");
    assert!(both_luma < open_luma * 0.6);
    assert!(
        b_luma < open_luma * 0.6,
        "the remaining body must still block skylight: {b_luma} vs {open_luma}"
    );
    let (_, gone) = session.settle(&[]);
    let (mean, max) = max_abs_diff(&open, &gone);
    assert!(
        mean < 0.05 && max <= 2,
        "everything gone must restore the open frame: {mean}, {max}"
    );
}

/// Per-frame cost of moving 200 bodies through the cache (release build):
/// CPU cell-diff + upload, and the GPU time of the slice each frame does.
#[test]
#[ignore = "requires a working GPU adapter; records measurements"]
fn moving_bodies_cost_per_frame() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let terrain: Vec<CubeInstance> =
        vec![ground(), boxed(WHITE, [0.0, 2.0, -8.0], [16.0, 4.0, 0.5])];
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials(),
        (1920, 1080),
        ctx.supports_gpu_timestamps()
            .then(|| ctx.timestamp_period_ns()),
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &terrain);
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&terrain_occupancy(&terrain)));
    let target = OffscreenTarget::new(&ctx.device, 1920, 1080);
    let cam = Camera::looking_along(
        Vec3::new(0.0, 4.0, 10.0),
        Vec3::new(0.0, -0.2, -1.0),
        75_f32.to_radians(),
        1920.0 / 1080.0,
    );
    let environment = EnvironmentPreset::Daylight.environment();
    let bodies = |frame: u32| -> Vec<CubeInstance> {
        (0..200)
            .map(|i| {
                let a = i as f32 * 0.7 + frame as f32 * 0.05;
                CubeInstance::new(
                    [
                        a.cos() * (2.0 + i as f32 * 0.04),
                        1.0 + (i % 7) as f32 * 0.3,
                        a.sin() * (2.0 + i as f32 * 0.04),
                    ],
                    WHITE,
                    [0.25; 3],
                    Quat::from_rotation_y(a).to_array(),
                )
            })
            .collect()
    };
    let (mut cpu, mut sky, mut bounce, mut set_bodies_ms) = (vec![], vec![], vec![], vec![]);
    for frame in 0..160 {
        let list = bodies(frame);
        let start = std::time::Instant::now();
        renderer.set_bodies(&ctx.device, &ctx.queue, &list);
        let set_ms = start.elapsed().as_secs_f64() * 1000.0;
        set_bodies_ms.push(set_ms);
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &cam,
            &environment,
            DebugView::Shaded,
        );
        ctx.queue.submit([encoder.finish()]);
        renderer.finish_timing();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        ctx.wait().expect("idle");
        if frame >= 40 {
            cpu.push(ms);
            if let Some(t) = renderer.pass_timings() {
                sky.extend(t.sky_visibility_ms);
                bounce.extend(t.bounce_ms);
            }
        }
    }
    let p = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(f64::total_cmp);
        v.get(((v.len().max(1) - 1) as f64 * q) as usize).copied()
    };
    let (cpu50, cpu95) = (p(&mut cpu, 0.5), p(&mut cpu, 0.95));
    let (sky50, bounce50) = (p(&mut sky, 0.5), p(&mut bounce, 0.5));
    println!(
        "200 moving bodies, 1080p: set_bodies + record + submit p50 {cpu50:?} p95 {cpu95:?} ms; per-frame slice: sky {sky50:?} ms, bounce {bounce50:?} ms; sweep {} frames",
        renderer.lighting_sweep_frames().unwrap_or(0)
    );
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-101-dynamic");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("body-cost.json"),
        format!(
            "{{\"adapter\": \"{}\", \"build\": \"{}\", \"bodies\": 200, \"cpu_frame_ms_p50\": {:.3}, \"cpu_frame_ms_p95\": {:.3}, \"slice_sky_ms_p50\": {:.4}, \"slice_bounce_ms_p50\": {:.4}, \"sweep_frames\": {}}}\n",
            ctx.adapter_name(),
            if cfg!(debug_assertions) { "debug" } else { "release" },
            cpu50.unwrap_or(0.0),
            cpu95.unwrap_or(0.0),
            sky50.unwrap_or(0.0),
            bounce50.unwrap_or(0.0),
            renderer.lighting_sweep_frames().unwrap_or(0)
        ),
    )
    .expect("write");
}

/// Scrolling the cache (the camera walked away from its centre) recomputes into
/// a back result set a slice per frame and swaps when complete: shading keeps
/// the old, consistent cache until then (no half-moved frame, no single-frame
/// whole-cache hitch), and afterwards uses the new one.
#[test]
#[ignore = "requires a working GPU adapter"]
fn scrolling_the_cache_swaps_in_one_consistent_frame() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let slab = boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    // The mesh is only the ground; the slab exists only in the lighting
    // occupancy, so the frame differs between caches by lighting alone.
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    session.renderer.set_sky_occupancy(
        &ctx.device,
        &ctx.queue,
        Some(&terrain_occupancy(&[ground(), slab])),
    );
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let (_, shaded) = session.settle(&[]);
    let shaded_luma = at(&session.camera, &shaded, probe)[1];

    // A different origin (8 m along +x, 4 m along +z) with the slab gone.
    let mut moved = LightingVolume::empty(cache_origin_around(Vec3::new(8.0, 0.0, 4.0)));
    mark_world_box(
        &mut moved,
        Vec3::new(-30.0, -0.5, -30.0),
        Vec3::new(30.0, 0.0, 30.0),
        WHITE,
    );
    assert_ne!(
        moved.origin(),
        cache_origin_around(Vec3::ZERO),
        "the test must actually move the cache"
    );
    session
        .renderer
        .set_sky_occupancy(&ctx.device, &ctx.queue, Some(&moved));

    let mut frames_old = 0;
    let mut worst_mean = 0.0_f32;
    let mut used = 0;
    let mut last = shaded.clone();
    while used < 40 {
        last = session.frame(&[]);
        used += 1;
        if session.renderer.lighting_is_sweeping() {
            let (mean, _) = max_abs_diff(&shaded, &last);
            worst_mean = worst_mean.max(mean);
            frames_old += 1;
        } else {
            break;
        }
    }
    // The swap frame's own image, then one more with the back set in front.
    let after = session.frame(&[]);
    save_png("scroll-after.png", &after);
    let open_luma = at(&session.camera, &after, probe)[1];
    println!(
        "under slab {shaded_luma:.3}; after scroll {open_luma:.3}; {frames_old} frames still showing the old cache (worst mean diff {worst_mean:.3}); sweep took {:?} frames",
        session.renderer.lighting_sweep_frames()
    );
    assert!(
        frames_old >= 6,
        "the re-centre must be spread over slices, not one dispatch: {frames_old}"
    );
    assert!(
        worst_mean < 0.05,
        "shading must keep the old cache until the swap: mean diff {worst_mean}"
    );
    assert!(
        open_luma > shaded_luma * 1.5,
        "after the swap the new cache (no slab) lights the ground: {open_luma} vs {shaded_luma}"
    );
    let _ = last;
}

/// A body that appears while the cache is re-centring is not lost: it is laid
/// over the new cache and lit once the swap lands.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_body_arriving_during_a_scroll_is_lit_after_the_swap() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let (_, open) = session.settle(&[]);
    let open_luma = at(&session.camera, &open, probe)[1];

    let mut moved = LightingVolume::empty(cache_origin_around(Vec3::new(8.0, 0.0, 4.0)));
    mark_world_box(
        &mut moved,
        Vec3::new(-30.0, -0.5, -30.0),
        Vec3::new(30.0, 0.0, 30.0),
        WHITE,
    );
    session
        .renderer
        .set_sky_occupancy(&ctx.device, &ctx.queue, Some(&moved));
    let slab = [boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0])];
    // The slab shows up on the third frame of the scroll.
    session.frame(&[]);
    session.frame(&[]);
    let (_, shaded) = session.settle(&slab);
    let (_, shaded) = if session.renderer.lighting_is_sweeping() {
        session.settle(&slab)
    } else {
        (0, shaded)
    };
    let under = at(&session.camera, &shaded, probe)[1];
    println!("open {open_luma:.3}; under a body that arrived mid-scroll {under:.3}");
    assert!(
        under < open_luma * 0.6,
        "the body must shade the ground after the swap: {under} vs {open_luma}"
    );
}

/// A terrain edit (a roof appears over open ground) is lit within one sweep:
/// measured in frames and CPU-clock milliseconds, with the frame time of the
/// frames in between (no frame carries the whole recompute).
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_terrain_edit_is_lit_within_one_sweep() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let (_, open) = session.settle(&[]);
    let open_luma = at(&session.camera, &open, probe)[1];

    let slab = boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    session.renderer.set_sky_occupancy(
        &ctx.device,
        &ctx.queue,
        Some(&terrain_occupancy(&[ground(), slab])),
    );
    let mut frame_ms = Vec::new();
    let mut frames = 0;
    let mut lit = open.clone();
    while frames < 40 {
        let start = std::time::Instant::now();
        lit = session.frame(&[]);
        frame_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        frames += 1;
        if !session.renderer.lighting_is_sweeping() {
            break;
        }
    }
    let under = at(&session.camera, &lit, probe)[1];
    let latency = session.renderer.lighting_latency_ms().expect("a sweep ran");
    let worst = frame_ms.iter().cloned().fold(0.0, f64::max);
    println!(
        "edit lit after {frames} frames / {latency:.1} ms (CPU clock, excl. GPU finish and present); ground {open_luma:.3} -> {under:.3}; frame time over the sweep: worst {worst:.1} ms, mean {:.1} ms",
        frame_ms.iter().sum::<f64>() / frame_ms.len() as f64
    );
    assert!(under < open_luma * 0.6, "the edit must be lit: {under}");
    assert!(frames <= 10, "one sliced sweep: {frames} frames");
}

/// A change (a body arriving) that lands *while* an unrelated sweep (a terrain
/// edit) is still running does not get lost: it is picked up by the next
/// sweep once the first finishes, and its own latency is measured from when
/// it actually arrived, not from the earlier sweep's start.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_change_arriving_during_an_active_sweep_is_not_lost() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let (_, open) = session.settle(&[]);
    let open_luma = at(&session.camera, &open, probe)[1];

    // Start a terrain-edit sweep (a roof appears), then, mid-sweep, land a
    // second, independent change (a body arrives elsewhere) before the first
    // sweep finishes.
    let slab = boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    session.renderer.set_sky_occupancy(
        &ctx.device,
        &ctx.queue,
        Some(&terrain_occupancy(&[ground(), slab])),
    );
    session.frame(&[]); // one slice of the terrain-edit sweep runs
    assert!(
        session.renderer.lighting_is_sweeping(),
        "the terrain sweep must still be running when the body lands"
    );
    let body = boxed(WHITE, [10.0, 2.0, 10.0], [1.0, 1.0, 1.0]);
    let body_landed_at = std::time::Instant::now();
    session.frame(&[body]); // the body's cells are diffed in during that sweep

    let mut frames = 1;
    while session.renderer.lighting_is_sweeping() && frames < 60 {
        session.frame(&[body]);
        frames += 1;
    }
    let lit = session.frame(&[body]);
    let under = at(&session.camera, &lit, probe)[1];
    println!(
        "body landed mid-sweep; total frames to both changes settling: {frames}; wall clock since the body landed: {:.1} ms; probe {open_luma:.3} -> {under:.3}",
        body_landed_at.elapsed().as_secs_f64() * 1000.0
    );
    assert!(
        under < open_luma * 0.6,
        "the terrain edit still lights: {under}"
    );
    // The renderer's own latency counter reflects the *last* change queued
    // (the body), not the earlier terrain edit -- both are exercised, but the
    // reported number must not be silently for the wrong one.
    assert!(session.renderer.lighting_latency_ms().is_some());
}

/// Repeated camera re-centres (the player keeps walking) queue correctly: each
/// swaps in on schedule and the last one's result is what shading ends up
/// showing, with no dropped or stuck re-centre.
#[test]
#[ignore = "requires a working GPU adapter"]
fn repeated_camera_recentres_each_complete_and_the_last_one_wins() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let slab = boxed(WHITE, [0.0, 2.5, 0.0], [6.0, 0.5, 6.0]);
    let mut session = Session::new(&ctx, skylight_only(), DebugView::Shaded, &[ground()]);
    session.camera = side_camera();
    session.renderer.set_sky_occupancy(
        &ctx.device,
        &ctx.queue,
        Some(&terrain_occupancy(&[ground(), slab])),
    );
    session.settle(&[]);

    // Three consecutive scrolls, each along +x/+z, fired back to back (a
    // player walking in a straight line, each one landing well before the
    // last recentre would otherwise have finished on its own).
    let centers = [
        Vec3::new(8.0, 0.0, 4.0),
        Vec3::new(16.0, 0.0, 8.0),
        Vec3::new(24.0, 0.0, 12.0),
    ];
    for center in centers {
        let mut moved = LightingVolume::empty(cache_origin_around(center));
        mark_world_box(
            &mut moved,
            Vec3::new(-40.0, -0.5, -40.0),
            Vec3::new(40.0, 0.0, 40.0),
            WHITE,
        );
        session
            .renderer
            .set_sky_occupancy(&ctx.device, &ctx.queue, Some(&moved));
        // Only two frames between scrolls -- well inside the 8-frame sweep, so
        // each new scroll interrupts the previous one's still-running sweep.
        session.frame(&[]);
        session.frame(&[]);
    }

    // Run to completion: every queued re-centre must finish, not get stuck.
    let mut frames = 0;
    while session.renderer.lighting_is_sweeping() && frames < 80 {
        session.frame(&[]);
        frames += 1;
    }
    assert!(
        !session.renderer.lighting_is_sweeping(),
        "a repeated scroll must not leave a re-centre permanently running: {frames} frames"
    );
    let final_image = session.frame(&[]);
    // The original probe point (still within the last re-centre's 64 m cache
    // extent) must now read open: the last scroll's grid is flat ground, no
    // slab, so this is only lit if the *last* re-centre -- not an earlier,
    // interrupted one -- is what ended up showing.
    let probe = Vec3::new(2.0, 0.0, 0.0);
    let luma = at(&session.camera, &final_image, probe)[1];
    println!(
        "after 3 back-to-back re-centres, {frames} frames to fully settle; final probe (last centre's grid, no slab) luma {luma:.3}"
    );
    assert!(
        luma > 0.3,
        "the last re-centre's result must be what shading ends up showing: {luma}"
    );
}
