//! `GameRenderer` (the interactive game's instanced-cube path) against a real
//! GPU, and against the editor's mesh path.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render --test game_renderer_gpu -- --ignored` on a host
//! with a working adapter.

use glam::{Quat, Vec3};
use spall_render::instances::unit_cube;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, GpuVertex,
    Material, MeshChunk, OffscreenTarget, RenderContext, ViewportFrame, ViewportRenderer, to_gpu,
};

const SIZE: (u32, u32) = (320, 240);

fn materials() -> Vec<Material> {
    vec![
        Material::default(),                                    // 0
        Material::new([0.18, 0.45, 0.12], 0.85, 0.0),           // 1 grass-ish
        Material::new([0.6, 0.55, 0.5], 0.4, 0.0),              // 2 pale stone
        Material::new([0.9, 0.3, 0.1], 0.6, 0.0).emissive(4.0), // 3 lamp
    ]
}

/// A slab, a pillar that shadows it, and a rotated cube in the air.
fn scene() -> (Vec<CubeInstance>, Vec<CubeInstance>) {
    let terrain = vec![
        CubeInstance::new(
            [0.0, -0.25, 0.0],
            1,
            [14.0, 0.5, 14.0],
            CubeInstance::IDENTITY_ROTATION,
        ),
        CubeInstance::new(
            [0.5, 1.0, 0.0],
            2,
            [1.0, 2.0, 1.0],
            CubeInstance::IDENTITY_ROTATION,
        ),
    ];
    let q = Quat::from_rotation_y(0.6) * Quat::from_rotation_x(0.3);
    let bodies = vec![CubeInstance::new(
        [-2.5, 1.6, 1.0],
        2,
        [0.8, 0.8, 0.8],
        q.to_array(),
    )];
    (terrain, bodies)
}

fn save_png(name: &str, rgba: &[u8]) {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-94-parity");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    image::save_buffer(
        dir.join(name),
        rgba,
        SIZE.0,
        SIZE.1,
        image::ColorType::Rgba8,
    )
    .expect("write evidence png");
}

fn camera() -> Camera {
    Camera::looking_along(
        Vec3::new(-5.0, 4.5, 7.0),
        Vec3::new(4.5, -3.2, -6.5),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

/// The same cubes as world-space mesh vertices, `ao = 1` like the cube path.
fn as_mesh(instances: &[CubeInstance]) -> (Vec<GpuVertex>, Vec<u32>) {
    let (cube, cube_indices) = unit_cube();
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    for instance in instances {
        let q = Quat::from_array(instance.rotation);
        let base = vertices.len() as u32;
        for v in &cube {
            let local = Vec3::from_array(v.position) * Vec3::from_array(instance.size);
            vertices.push(GpuVertex {
                position: (q * local + Vec3::from_array(instance.offset)).to_array(),
                normal: (q * Vec3::from_array(v.normal)).to_array(),
                local_uv: v.local_uv,
                ao: 1.0,
                material: instance.material,
            });
        }
        indices.extend(cube_indices.iter().map(|i| base + u32::from(*i)));
    }
    (vertices, indices)
}

fn render_game(
    ctx: &RenderContext,
    environment: &Environment,
    view: DebugView,
    terrain: &[CubeInstance],
    bodies: &[CubeInstance],
    materials: &[Material],
) -> Vec<u8> {
    render_game_from(
        ctx,
        &camera(),
        environment,
        view,
        terrain,
        bodies,
        materials,
    )
}

fn render_game_from(
    ctx: &RenderContext,
    camera: &Camera,
    environment: &Environment,
    view: DebugView,
    terrain: &[CubeInstance],
    bodies: &[CubeInstance],
    materials: &[Material],
) -> Vec<u8> {
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        materials,
        SIZE,
        ctx.supports_gpu_timestamps()
            .then(|| ctx.timestamp_period_ns()),
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, terrain);
    renderer.set_bodies(&ctx.device, &ctx.queue, bodies);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    renderer.render(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        target.color_view(),
        camera,
        environment,
        view,
    );
    target.copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    renderer.finish_timing();
    target.read_rgba(ctx).expect("readback")
}

fn render_mesh(
    ctx: &RenderContext,
    environment: &Environment,
    all: &[CubeInstance],
    materials: &[Material],
) -> Vec<u8> {
    let mut viewport = ViewportRenderer::new(ctx, materials);
    viewport.resize(ctx, SIZE.0, SIZE.1);
    let (vertices, indices) = as_mesh(all);
    viewport
        .set_meshes(
            ctx,
            &[MeshChunk {
                vertices: &vertices,
                indices: &indices,
            }],
        )
        .expect("upload");
    viewport.render(
        ctx,
        &ViewportFrame {
            camera: camera(),
            environment: *environment,
        },
    );
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    viewport.target().copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    viewport.target().read_rgba(ctx).expect("readback")
}

#[test]
#[ignore = "requires a real GPU adapter; run with --ignored"]
fn game_resident_greedy_mesh_matches_the_shared_viewport_pipeline() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Daylight.environment();
    let materials = materials();
    let volume = spall_mesh::fixtures::cube([0, 0, 0], 4);
    let built = spall_mesh::build_volume_mesh(
        &volume,
        spall_jobs::Generation::START,
        spall_jobs::TopologyEpoch::START,
        Default::default(),
    )
    .expect("greedy terrain mesh");
    let (vertices, indices) = to_gpu(&built.mesh, glam::Mat4::IDENTITY);
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut game = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials,
        SIZE,
        ctx.supports_gpu_timestamps()
            .then(|| ctx.timestamp_period_ns()),
    );
    game.update_terrain_meshes(
        &ctx.device,
        &[(
            spall_core::BrickCoord::new(0, 0, 0),
            vertices.clone(),
            indices.clone(),
        )],
        &[],
    )
    .expect("terrain chunk upload");
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    game.render(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        target.color_view(),
        &camera(),
        &environment,
        DebugView::Shaded,
    );
    target.copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    let game_pixels = target.read_rgba(&ctx).expect("game mesh readback");

    let mut viewport = ViewportRenderer::new(&ctx, &materials);
    viewport.resize(&ctx, SIZE.0, SIZE.1);
    viewport
        .set_meshes(
            &ctx,
            &[MeshChunk {
                vertices: &vertices,
                indices: &indices,
            }],
        )
        .expect("viewport upload");
    viewport.render(
        &ctx,
        &ViewportFrame {
            camera: camera(),
            environment,
        },
    );
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    viewport.target().copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    let viewport_pixels = viewport
        .target()
        .read_rgba(&ctx)
        .expect("viewport readback");

    let mean = game_pixels
        .iter()
        .zip(&viewport_pixels)
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum::<u64>() as f64
        / game_pixels.len() as f64;
    assert_eq!(
        mean, 0.0,
        "game and viewport greedy geometry must remain pixel-identical"
    );
}

fn luma(rgba: &[u8], x: u32, y: u32) -> f32 {
    let i = ((y * SIZE.0 + x) * 4) as usize;
    (0.2126 * f32::from(rgba[i])
        + 0.7152 * f32::from(rgba[i + 1])
        + 0.0722 * f32::from(rgba[i + 2]))
        / 255.0
}

fn mean_luma(rgba: &[u8]) -> f32 {
    let n = SIZE.0 * SIZE.1;
    (0..n)
        .map(|i| luma(rgba, i % SIZE.0, i / SIZE.0))
        .sum::<f32>()
        / n as f32
}

/// Same scene, camera and environment through the game path and the editor's
/// mesh path must agree: they share every shading pass and differ only in how
/// geometry is submitted.
#[test]
#[ignore = "requires a working GPU adapter"]
fn the_game_and_editor_paths_shade_the_same_scene_identically() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let (terrain, bodies) = scene();
    let all: Vec<_> = terrain.iter().chain(&bodies).copied().collect();
    for preset in EnvironmentPreset::ALL {
        let environment = preset.environment();
        let game = render_game(
            &ctx,
            &environment,
            DebugView::Shaded,
            &terrain,
            &bodies,
            &materials(),
        );
        let mesh = render_mesh(&ctx, &environment, &all, &materials());
        assert_eq!(game.len(), mesh.len());
        let diffs: Vec<u8> = game
            .chunks_exact(4)
            .zip(mesh.chunks_exact(4))
            .flat_map(|(a, b)| (0..3).map(move |c| a[c].abs_diff(b[c])))
            .collect();
        let mean = diffs.iter().map(|d| f32::from(*d)).sum::<f32>() / diffs.len() as f32;
        let big = diffs.iter().filter(|d| **d > 12).count() as f32 / diffs.len() as f32;
        save_png(&format!("{}-game.png", preset.key()), &game);
        save_png(&format!("{}-mesh.png", preset.key()), &mesh);
        let amplified: Vec<u8> = game
            .chunks_exact(4)
            .zip(mesh.chunks_exact(4))
            .flat_map(|(a, b)| {
                let d = |c: usize| a[c].abs_diff(b[c]).saturating_mul(8);
                [d(0), d(1), d(2), 255]
            })
            .collect();
        save_png(&format!("{}-diff-x8.png", preset.key()), &amplified);
        println!(
            "{}: mean |diff| {mean:.3}/255, {:.3}% of channels differ by >12",
            preset.key(),
            big * 100.0
        );
        // Sub-pixel silhouette rounding between CPU- and GPU-transformed
        // vertices is allowed; a lighting or material divergence is not.
        assert!(mean < 0.6, "{}: mean diff {mean}", preset.key());
        assert!(big < 0.004, "{}: {:.3}% differ", preset.key(), big * 100.0);
    }
}

/// Terrain and a body both cast shadows into the same shadow maps.
#[test]
#[ignore = "requires a working GPU adapter"]
fn terrain_and_moving_bodies_cast_shadows() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Daylight.environment();
    let (terrain, bodies) = scene();
    let with_bodies = render_game(
        &ctx,
        &environment,
        DebugView::Shaded,
        &terrain,
        &bodies,
        &materials(),
    );
    let without_bodies = render_game(
        &ctx,
        &environment,
        DebugView::Shaded,
        &terrain,
        &[],
        &materials(),
    );
    let without_pillar = render_game(
        &ctx,
        &environment,
        DebugView::Shaded,
        &terrain[..1],
        &bodies,
        &materials(),
    );

    // The body is drawn only in the first image.
    assert!(
        (mean_luma(&with_bodies) - mean_luma(&without_bodies)).abs() > 1e-4,
        "the body should change the image"
    );
    // Project sample points on the slab into the image and compare a point in
    // the pillar's shadow, and one in the body's, against the unshadowed slab.
    let cam = camera();
    let at = |p: Vec3| {
        let ndc = cam.project(p).expect("in front of the camera");
        (
            ((ndc[0] * 0.5 + 0.5) * SIZE.0 as f32) as u32,
            ((0.5 - ndc[1] * 0.5) * SIZE.1 as f32) as u32,
        )
    };
    let sun = environment.sun_dir;
    // Where the pillar top / body centre project along the sun onto y = 0.
    let ground = |p: Vec3| p - sun * (p.y / sun.y);
    let pillar_shadow = at(ground(Vec3::new(0.5, 1.6, 0.0)));
    let body_shadow = at(ground(Vec3::new(-2.5, 1.6, 1.0)));

    let lit = |image: &[u8], (x, y): (u32, u32)| luma(image, x, y);
    // Pillar shadow: present with the pillar, absent without it.
    assert!(
        lit(&with_bodies, pillar_shadow) < lit(&without_pillar, pillar_shadow) * 0.8,
        "terrain pillar shadow missing: {} vs {}",
        lit(&with_bodies, pillar_shadow),
        lit(&without_pillar, pillar_shadow)
    );
    // Body shadow: present with the body, absent without it.
    assert!(
        lit(&with_bodies, body_shadow) < lit(&without_bodies, body_shadow) * 0.8,
        "body shadow missing: {} vs {}",
        lit(&with_bodies, body_shadow),
        lit(&without_bodies, body_shadow)
    );
}

/// Material identity reaches the shader: an emissive material glows in the
/// direct pass and its neighbours do not.
#[test]
#[ignore = "requires a working GPU adapter"]
fn emissive_materials_glow_and_debug_views_show_albedo() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Night.environment();
    let cube = |material| {
        vec![CubeInstance::new(
            [0.0, 0.0, 0.0],
            material,
            [1.5, 1.5, 1.5],
            CubeInstance::IDENTITY_ROTATION,
        )]
    };
    let close = Camera::looking_along(
        Vec3::new(0.0, 0.5, 3.0),
        Vec3::new(0.0, -0.15, -1.0),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let shot = |material| {
        render_game_from(
            &ctx,
            &close,
            &environment,
            DebugView::Shaded,
            &cube(material),
            &[],
            &materials(),
        )
    };
    let lamp = shot(3);
    let stone = shot(2);
    assert!(
        mean_luma(&lamp) > mean_luma(&stone) * 1.5,
        "lamp {} vs stone {}",
        mean_luma(&lamp),
        mean_luma(&stone)
    );

    // Albedo view: linear albedo encoded to sRGB exactly once.
    let albedo = render_game_from(
        &ctx,
        &close,
        &Environment::default(),
        DebugView::Albedo,
        &cube(1),
        &[],
        &materials(),
    );
    let centre = ((SIZE.1 / 2) * SIZE.0 + SIZE.0 / 2) as usize * 4;
    let encoded = |linear: f32| {
        let c = if linear <= 0.003_130_8 {
            linear * 12.92
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (c * 255.0).round() as i32
    };
    let grass = [0.18_f32, 0.45, 0.12];
    for channel in 0..3 {
        // The 8 % cell-edge grid line can darken a pixel slightly.
        let got = i32::from(albedo[centre + channel]);
        let want = encoded(grass[channel]);
        assert!(
            (got - want).abs() <= 22,
            "channel {channel}: {got} vs {want}"
        );
    }
}

/// Per-pass GPU cost of the game path at forest scale: ~58k terrain surface
/// cubes (the size of the ENG-94 forest fixture's instance set), 200 body
/// cubes, 1920x1080, a 300 m camera far plane. Writes
/// `.local/runs/eng-94-perf/summary.json`. Measured, not a pass/fail gate:
/// the provisional G2 budgets are recorded in `docs/reports/ENG-94.md`.
#[test]
#[ignore = "requires a working GPU adapter; records measurements"]
fn game_renderer_frame_cost_at_forest_scale() {
    const CELL: f32 = 0.25;
    const GRID: i32 = 242; // 242 * 242 = 58_564 surface cells
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Daylight.environment();
    let height = |x: i32, z: i32| {
        let (fx, fz) = (x as f32 * CELL, z as f32 * CELL);
        ((0.35 * (fx * 0.35).sin() + 0.3 * (fz * 0.27).cos()) / CELL).round() * CELL
    };
    let terrain: Vec<CubeInstance> = (0..GRID)
        .flat_map(|x| (0..GRID).map(move |z| (x, z)))
        .map(|(x, z)| {
            CubeInstance::new(
                [
                    (x - GRID / 2) as f32 * CELL,
                    height(x, z),
                    (z - GRID / 2) as f32 * CELL,
                ],
                1,
                [CELL; 3],
                CubeInstance::IDENTITY_ROTATION,
            )
        })
        .collect();
    let bodies: Vec<CubeInstance> = (0..200)
        .map(|i| {
            let a = i as f32 * 0.7;
            CubeInstance::new(
                [
                    a.cos() * (2.0 + i as f32 * 0.05),
                    1.0 + (i % 7) as f32 * 0.3,
                    a.sin() * (2.0 + i as f32 * 0.05),
                ],
                2,
                [CELL; 3],
                Quat::from_rotation_y(a).to_array(),
            )
        })
        .collect();

    let (width, height_px) = (1920, 1080);
    let target = OffscreenTarget::new(&ctx.device, width, height_px);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials(),
        (width, height_px),
        ctx.supports_gpu_timestamps()
            .then(|| ctx.timestamp_period_ns()),
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &terrain);
    let cam = Camera::looking_along(
        Vec3::new(0.0, 3.0, 12.0),
        Vec3::new(0.2, -0.25, -1.0),
        75_f32.to_radians(),
        width as f32 / height_px as f32,
    );
    let (mut shadow, mut opaque, mut tone, mut cpu) = (vec![], vec![], vec![], vec![]);
    for frame in 0..180 {
        let start = std::time::Instant::now();
        renderer.set_bodies(&ctx.device, &ctx.queue, &bodies);
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
        let encode_ms = start.elapsed().as_secs_f64() * 1000.0;
        ctx.wait().expect("device idle");
        if frame >= 30 {
            cpu.push(encode_ms);
            // Timings resolve a few frames behind; the steady scene makes the
            // lag irrelevant.
            if let Some(t) = renderer.pass_timings() {
                shadow.push(t.shadow_ms);
                opaque.push(t.opaque_ms);
                tone.push(t.tone_map_ms);
            }
        }
    }
    let pct = |v: &mut Vec<f64>, p: f64| -> Option<f64> {
        (!v.is_empty()).then(|| {
            v.sort_by(f64::total_cmp);
            v[(((v.len() - 1) as f64) * p).round() as usize]
        })
    };
    let mut report = String::new();
    for (name, v) in [
        ("shadow_ms", &mut shadow),
        ("opaque_ms", &mut opaque),
        ("tone_map_ms", &mut tone),
        ("cpu_record_submit_ms", &mut cpu),
    ] {
        let ms = |value: Option<f64>| value.map_or("null".to_owned(), |v| format!("{v:.4}"));
        let (p50, p95) = (ms(pct(v, 0.5)), ms(pct(v, 0.95)));
        println!("{name}: p50 {p50} p95 {p95} (n={})", v.len());
        report.push_str(&format!(
            "  \"{name}\": {{\"p50\": {p50}, \"p95\": {p95}, \"samples\": {}}},
",
            v.len()
        ));
    }
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-94-perf");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    let json = format!(
        "{{\n  \"adapter\": \"{}\",\n  \"backend\": \"{:?}\",\n  \"build\": \"{}\",\n  \"resolution\": [{width}, {height_px}],\n  \"terrain_cubes\": {},\n  \"body_cubes\": {},\n  \"instance_bytes\": {},\n  \"gpu_timestamps\": {},\n{report}  \"frames\": 150\n}}\n",
        ctx.adapter_name(),
        ctx.backend(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        terrain.len(),
        bodies.len(),
        renderer.instance_bytes(),
        ctx.supports_gpu_timestamps(),
    );
    std::fs::write(dir.join("summary.json"), json).expect("write summary");
}
