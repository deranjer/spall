//! Sky visibility against a real GPU: skylight must reach open ground and stop
//! at roofs and walls; unknown space must block it; nothing outdoors changes.
//! See `docs/reports/ENG-96.md`.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render --test sky_visibility_gpu -- --ignored`.

use glam::{Quat, Vec3};
use spall_render::indirect::LightingVolume;
use spall_render::instances::unit_cube;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, GpuVertex,
    Material, MeshChunk, OffscreenTarget, RenderContext, UNKNOWN_CELL, ViewportFrame,
    ViewportRenderer, cache_origin_around, mark_world_box,
};

const SIZE: (u32, u32) = (480, 360);
const IDENTITY: [f32; 4] = CubeInstance::IDENTITY_ROTATION;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),
        Material::new([0.7, 0.7, 0.7], 0.9, 0.0),
    ]
}

/// Skylight only: no sun, so any light on a surface came from the sky.
fn sky_only() -> Environment {
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_intensity = 0.0;
    environment
}

fn boxed(center: [f32; 3], size: [f32; 3]) -> CubeInstance {
    CubeInstance::new(center, 1, size, IDENTITY)
}

fn ground() -> CubeInstance {
    boxed([0.0, -0.25, 0.0], [60.0, 0.5, 60.0])
}

#[derive(Clone, Copy, PartialEq)]
enum Room {
    Closed,
    DoorOpen,
    NoRoof,
}

/// A 6 m x 6 m room with 0.25 m walls (one voxel thick), 3 m high.
fn room(kind: Room) -> Vec<CubeInstance> {
    let mut boxes = vec![
        ground(),
        boxed([0.0, 1.5, -3.125], [6.5, 3.0, 0.25]), // north
        boxed([3.125, 1.5, 0.0], [0.25, 3.0, 6.0]),  // east
        boxed([-3.125, 1.5, 0.0], [0.25, 3.0, 6.0]), // west
    ];
    match kind {
        Room::DoorOpen => {
            // A 1.5 m doorway in the south wall.
            boxes.push(boxed([-2.0, 1.5, 3.125], [2.5, 3.0, 0.25]));
            boxes.push(boxed([2.0, 1.5, 3.125], [2.5, 3.0, 0.25]));
        }
        _ => boxes.push(boxed([0.0, 1.5, 3.125], [6.5, 3.0, 0.25])),
    }
    if kind != Room::NoRoof {
        boxes.push(boxed([0.0, 3.125, 0.0], [6.5, 0.25, 6.5]));
    }
    boxes
}

/// Occupancy of axis-aligned boxes (the interactive client marks voxels the
/// same way).
fn occupancy(boxes: &[CubeInstance], centre: Vec3) -> LightingVolume {
    let mut volume = LightingVolume::empty(cache_origin_around(centre));
    for b in boxes {
        let (c, h) = (Vec3::from_array(b.offset), Vec3::from_array(b.size) * 0.5);
        mark_world_box(&mut volume, c - h, c + h, 1);
    }
    volume
}

fn render(
    ctx: &RenderContext,
    camera: &Camera,
    environment: &Environment,
    view: DebugView,
    boxes: &[CubeInstance],
    sky: Option<&LightingVolume>,
) -> Vec<u8> {
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials(),
        SIZE,
        None,
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, boxes);
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, sky);
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
    target.read_rgba(ctx).expect("readback")
}

fn save_png(name: &str, rgba: &[u8]) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-96-sky");
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

fn luma_at(camera: &Camera, rgba: &[u8], p: Vec3) -> f32 {
    let ndc = camera.project(p).expect("in front of the camera");
    let x = (((ndc[0] * 0.5 + 0.5) * SIZE.0 as f32) as u32).min(SIZE.0 - 1);
    let y = (((0.5 - ndc[1] * 0.5) * SIZE.1 as f32) as u32).min(SIZE.1 - 1);
    let i = ((y * SIZE.0 + x) * 4) as usize;
    (0.2126 * f32::from(rgba[i])
        + 0.7152 * f32::from(rgba[i + 1])
        + 0.0722 * f32::from(rgba[i + 2]))
        / 255.0
}

/// Inside the room, looking toward the doorway wall.
fn inside_camera() -> Camera {
    Camera::looking_along(
        Vec3::new(0.0, 1.7, -2.4),
        Vec3::new(0.0, -0.47, 1.0),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

#[test]
#[ignore = "requires a working GPU adapter"]
fn closed_rooms_are_dark_and_opening_them_lets_skylight_in() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = sky_only();
    let camera = inside_camera();
    // Floor points: just inside the doorway, and mid-room.
    let near_door = Vec3::new(0.0, 0.0, 2.2);
    let mid_room = Vec3::new(0.0, 0.0, -0.2);
    // A wall point beside the doorway, inside.
    let side_wall = Vec3::new(-2.95, 1.0, 1.0);

    let mut samples = Vec::new();
    for (name, kind) in [
        ("closed", Room::Closed),
        ("door", Room::DoorOpen),
        ("noroof", Room::NoRoof),
    ] {
        let boxes = room(kind);
        let volume = occupancy(&boxes, Vec3::ZERO);
        let lit = render(
            &ctx,
            &camera,
            &environment,
            DebugView::Shaded,
            &boxes,
            Some(&volume),
        );
        let vis = render(
            &ctx,
            &camera,
            &environment,
            DebugView::SkyVisibility,
            &boxes,
            Some(&volume),
        );
        save_png(&format!("room-{name}-shaded.png"), &lit);
        save_png(&format!("room-{name}-sky-visibility.png"), &vis);
        let point = |image: &[u8], p| luma_at(&camera, image, p);
        samples.push((
            name,
            point(&lit, near_door),
            point(&lit, mid_room),
            point(&lit, side_wall),
            point(&vis, near_door),
        ));
    }
    // The legacy path (no occupancy): the same closed room, unconditional ambient.
    let boxes = room(Room::Closed);
    let legacy = render(&ctx, &camera, &environment, DebugView::Shaded, &boxes, None);
    save_png("room-closed-legacy-ambient.png", &legacy);
    let legacy_mid = luma_at(&camera, &legacy, mid_room);

    for (name, near, mid, wall, vis) in &samples {
        println!(
            "{name:>7}: near door {near:.3}, mid room {mid:.3}, side wall {wall:.3}, sky visibility at door {vis:.3}"
        );
    }
    println!("legacy closed room, mid room {legacy_mid:.3}");
    let [
        (_, c_near, c_mid, c_wall, c_vis),
        (_, d_near, d_mid, _, d_vis),
        (_, n_near, n_mid, n_wall, n_vis),
    ] = [samples[0], samples[1], samples[2]];

    // Closed: dark everywhere, far darker than the unconditional ambient was.
    assert!(
        c_vis < 0.05 && c_mid < 0.06 && c_near < 0.06 && c_wall < 0.06,
        "a closed room must be dark: {c_near} {c_mid} {c_wall} (vis {c_vis})"
    );
    assert!(
        legacy_mid > c_mid * 4.0,
        "the legacy ambient lit the closed room: {legacy_mid} vs {c_mid}"
    );
    // Opening the door lights the floor near it much more than the far floor.
    assert!(
        d_near > c_near * 3.0 && d_vis > c_vis + 0.15,
        "door: {d_near} vs closed {c_near}, vis {d_vis}"
    );
    assert!(
        d_near > d_mid,
        "light falls off away from the door: {d_near} vs {d_mid}"
    );
    // Removing the roof lights the whole room.
    assert!(
        n_mid > c_mid * 4.0 && n_wall > c_wall * 4.0 && n_vis > 0.3,
        "roofless room should be lit: {n_mid} {n_wall} (vis {n_vis})"
    );
    assert!(
        n_near > d_near * 0.9 || n_mid > d_mid,
        "roofless is at least as lit as the doorway"
    );
}

/// Unknown space blocks skylight; it is never treated as open sky.
#[test]
#[ignore = "requires a working GPU adapter"]
fn unknown_space_is_not_treated_as_open_sky() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = sky_only();
    let camera = Camera::looking_along(
        Vec3::new(0.0, 4.0, 6.0),
        Vec3::new(0.0, -0.6, -1.0),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let boxes = [ground()];
    let open = occupancy(&boxes, Vec3::ZERO);
    let mut unknown = open.clone();
    // A slab of never-loaded space 3 m above the ground.
    mark_world_box(
        &mut unknown,
        Vec3::new(-8.0, 3.0, -8.0),
        Vec3::new(8.0, 3.5, 8.0),
        UNKNOWN_CELL,
    );

    let target = Vec3::new(0.0, 0.0, 0.0);
    let bright = luma_at(
        &camera,
        &render(
            &ctx,
            &camera,
            &environment,
            DebugView::Shaded,
            &boxes,
            Some(&open),
        ),
        target,
    );
    let blocked = luma_at(
        &camera,
        &render(
            &ctx,
            &camera,
            &environment,
            DebugView::Shaded,
            &boxes,
            Some(&unknown),
        ),
        target,
    );
    println!("ground under open sky {bright:.3}, under unknown space {blocked:.3}");
    assert!(
        blocked < bright * 0.35,
        "unknown space must block skylight: {blocked} vs {bright}"
    );
}

/// Outdoors nothing changes: open ground lit by the sky is the same with and
/// without occupancy.
#[test]
#[ignore = "requires a working GPU adapter"]
fn open_ground_matches_the_legacy_ambient() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = sky_only();
    let camera = Camera::looking_along(
        Vec3::new(0.0, 5.0, 8.0),
        Vec3::new(0.0, -0.5, -1.0),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let boxes = [ground()];
    let volume = occupancy(&boxes, Vec3::ZERO);
    let with = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &boxes,
        Some(&volume),
    );
    let without = render(&ctx, &camera, &environment, DebugView::Shaded, &boxes, None);
    for p in [
        Vec3::new(0.0, 0.0, 0.0),
        Vec3::new(4.0, 0.0, -3.0),
        Vec3::new(-6.0, 0.0, 2.0),
    ] {
        let (a, b) = (luma_at(&camera, &with, p), luma_at(&camera, &without, p));
        assert!(
            (a - b).abs() < 0.02,
            "open ground changed at {p:?}: {a} vs {b}"
        );
    }
}

/// The editor's mesh path and the game's cube path light a room identically.
#[test]
#[ignore = "requires a working GPU adapter"]
fn game_and_editor_paths_agree_with_sky_visibility() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Sunset.environment();
    let camera = inside_camera();
    let boxes = room(Room::DoorOpen);
    let volume = occupancy(&boxes, Vec3::ZERO);
    let game = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &boxes,
        Some(&volume),
    );

    let (cube, cube_indices) = unit_cube();
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    for instance in &boxes {
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
    let mut viewport = ViewportRenderer::new(&ctx, &materials());
    viewport.resize(&ctx, SIZE.0, SIZE.1);
    viewport
        .set_meshes(
            &ctx,
            &[MeshChunk {
                vertices: &vertices,
                indices: &indices,
            }],
        )
        .expect("upload");
    viewport.set_sky_occupancy(&ctx, Some(&volume));
    viewport.render(
        &ctx,
        &ViewportFrame {
            camera,
            environment,
        },
    );
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    viewport.target().copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    let mesh = viewport.target().read_rgba(&ctx).expect("readback");

    let diffs: Vec<u8> = game
        .chunks_exact(4)
        .zip(mesh.chunks_exact(4))
        .flat_map(|(a, b)| (0..3).map(move |c| a[c].abs_diff(b[c])))
        .collect();
    let mean = diffs.iter().map(|d| f32::from(*d)).sum::<f32>() / diffs.len() as f32;
    let big = diffs.iter().filter(|d| **d > 12).count() as f32 / diffs.len() as f32;
    println!(
        "game vs editor with sky visibility: mean |diff| {mean:.3}/255, {:.3}% > 12",
        big * 100.0
    );
    assert!(
        mean < 0.6 && big < 0.004,
        "paths diverge: mean {mean}, {big}"
    );
}

/// Per-pass GPU cost of the R3 direct-light path at forest scale, with and
/// without sky visibility: ~58k terrain cubes with a scattering of tall
/// "canopy" cubes so the visibility rays have something to hit, 200 bodies,
/// 1920x1080. Writes `.local/runs/eng-96-perf/summary.json`. Recorded, not
/// gated; budgets are compared in `docs/reports/ENG-96.md`.
#[test]
#[ignore = "requires a working GPU adapter; records measurements"]
fn sky_visibility_cost_at_forest_scale() {
    const CELL: f32 = 0.25;
    const GRID: i32 = 242;
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Daylight.environment();
    let height = |x: i32, z: i32| {
        let (fx, fz) = (x as f32 * CELL, z as f32 * CELL);
        ((0.35 * (fx * 0.35).sin() + 0.3 * (fz * 0.27).cos()) / CELL).round() * CELL
    };
    let mut terrain: Vec<CubeInstance> = (0..GRID)
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
                IDENTITY,
            )
        })
        .collect();
    // Canopy: a 6x6 grid of 3 m cubes floating 6 m up.
    for i in 0..6 {
        for j in 0..6 {
            terrain.push(CubeInstance::new(
                [-12.0 + i as f32 * 4.5, 6.0, -12.0 + j as f32 * 4.5],
                1,
                [3.0, 1.0, 3.0],
                IDENTITY,
            ));
        }
    }
    let bodies: Vec<CubeInstance> = (0..200)
        .map(|i| {
            let a = i as f32 * 0.7;
            CubeInstance::new(
                [
                    a.cos() * (2.0 + i as f32 * 0.05),
                    1.0 + (i % 7) as f32 * 0.3,
                    a.sin() * (2.0 + i as f32 * 0.05),
                ],
                1,
                [CELL; 3],
                Quat::from_rotation_y(a).to_array(),
            )
        })
        .collect();
    let occupancy = occupancy(&terrain, Vec3::ZERO);

    let (width, height_px) = (1920, 1080);
    let target = OffscreenTarget::new(&ctx.device, width, height_px);
    let camera = Camera::looking_along(
        Vec3::new(0.0, 3.0, 12.0),
        Vec3::new(0.2, -0.25, -1.0),
        75_f32.to_radians(),
        width as f32 / height_px as f32,
    );
    let pct = |v: &mut Vec<f64>, p: f64| -> Option<f64> {
        (!v.is_empty()).then(|| {
            v.sort_by(f64::total_cmp);
            v[(((v.len() - 1) as f64) * p).round() as usize]
        })
    };
    let ms = |value: Option<f64>| value.map_or("null".to_owned(), |v| format!("{v:.4}"));

    let mut json = format!(
        "{{\n  \"adapter\": \"{}\",\n  \"backend\": \"{:?}\",\n  \"build\": \"{}\",\n  \"resolution\": [{width}, {height_px}],\n  \"terrain_cubes\": {},\n  \"body_cubes\": {},\n  \"gpu_timestamps\": {},\n",
        ctx.adapter_name(),
        ctx.backend(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        terrain.len(),
        bodies.len(),
        ctx.supports_gpu_timestamps(),
    );
    for (label, with_sky) in [("legacy_ambient", false), ("sky_visibility", true)] {
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
        renderer.set_bodies(&ctx.device, &ctx.queue, &bodies);
        if with_sky {
            renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&occupancy));
        }
        let (mut shadow, mut opaque, mut tone, mut cpu) = (vec![], vec![], vec![], vec![]);
        let mut sky_recompute = None;
        let mut bounce_recompute = None;
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
                &camera,
                &environment,
                DebugView::Shaded,
            );
            ctx.queue.submit([encoder.finish()]);
            renderer.finish_timing();
            let encode_ms = start.elapsed().as_secs_f64() * 1000.0;
            ctx.wait().expect("device idle");
            if let Some(t) = renderer.pass_timings() {
                sky_recompute = sky_recompute.or(t.sky_visibility_ms);
                bounce_recompute = bounce_recompute.or(t.bounce_ms);
                if frame >= 30 {
                    shadow.push(t.shadow_ms);
                    opaque.push(t.opaque_ms);
                    tone.push(t.tone_map_ms);
                }
            }
            if frame >= 30 {
                cpu.push(encode_ms);
            }
        }
        let mut block = format!("  \"{label}\": {{\n");
        for (name, v) in [
            ("shadow_ms", &mut shadow),
            ("opaque_ms", &mut opaque),
            ("tone_map_ms", &mut tone),
            ("cpu_record_submit_ms", &mut cpu),
        ] {
            let (p50, p95) = (ms(pct(v, 0.5)), ms(pct(v, 0.95)));
            println!("{label} {name}: p50 {p50} p95 {p95}");
            block.push_str(&format!(
                "    \"{name}\": {{\"p50\": {p50}, \"p95\": {p95}}},\n"
            ));
        }
        println!(
            "{label} sky recompute {}, bounce recompute {}",
            ms(sky_recompute),
            ms(bounce_recompute)
        );
        block.push_str(&format!(
            "    \"sky_recompute_ms\": {},
    \"bounce_recompute_ms\": {},
    \"sky_bytes\": {}
  }}{}
",
            ms(sky_recompute),
            ms(bounce_recompute),
            renderer.sky_bytes(),
            if with_sky { "" } else { "," }
        ));
        json.push_str(&block);
    }
    json.push_str("}\n");
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-96-perf");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    std::fs::write(dir.join("summary.json"), json).expect("write summary");
}

/// A sealed room stays black under the real sun (not just sky-only): no light
/// bleeds through one-voxel walls or the roof seam, at any shadow-cascade range.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_sealed_room_stays_black_under_the_sun() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment = EnvironmentPreset::Daylight.environment();
    let boxes = room(Room::Closed);
    let volume = occupancy(&boxes, Vec3::ZERO);
    for far in [60.0_f32, 300.0] {
        let mut camera = inside_camera();
        camera.z_far = far;
        let image = render(
            &ctx,
            &camera,
            &environment,
            DebugView::Shaded,
            &boxes,
            Some(&volume),
        );
        let brightest = image
            .chunks_exact(4)
            .map(|p| p[0].max(p[1]).max(p[2]))
            .max()
            .unwrap();
        assert!(
            brightest <= 2,
            "light bled into the sealed room (far {far}): {brightest}/255"
        );
    }
}
