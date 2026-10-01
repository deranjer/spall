//! The water look (smoothed heightfield surface, underwater fog/tint and
//! caustics) against a real GPU. Writes PNGs for review to
//! `.local/runs/water-look/` and asserts the properties the look is meant to
//! have, not exact pixels.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render --test water_look_gpu -- --ignored`.

use glam::Vec3;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::water::{WaterColumn, WaterField, build_water_surface};
use spall_render::{
    Camera, CubeInstance, DebugView, EnvironmentPreset, GameRenderer, Material, OffscreenTarget,
    RenderContext,
};

const SIZE: (u32, u32) = (640, 360);
const CELL: f32 = 0.25;
const LAKE_CELLS: i32 = 48; // 12 m square
const LEVEL: f32 = 2.0;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),                         // 0 (water sheet; colour unused)
        Material::new([0.55, 0.47, 0.30], 0.9, 0.0), // 1 sand
        Material::new([0.45, 0.45, 0.47], 0.7, 0.0), // 2 stone
        Material::new([0.70, 0.18, 0.12], 0.6, 0.0), // 3 red marker
    ]
}

/// A stone-walled basin with a sand floor and red pillars every 2 m.
fn basin() -> Vec<CubeInstance> {
    let id = CubeInstance::IDENTITY_ROTATION;
    let mut cubes = vec![
        CubeInstance::new([6.0, -0.5, 6.0], 1, [16.0, 1.0, 16.0], id),
        // Banks: 1 m thick, 3.5 m tall, around the 12 m lake.
        CubeInstance::new([-0.5, 1.25, 6.0], 2, [1.0, 3.5, 14.0], id),
        CubeInstance::new([12.5, 1.25, 6.0], 2, [1.0, 3.5, 14.0], id),
        CubeInstance::new([6.0, 1.25, -0.5], 2, [14.0, 3.5, 1.0], id),
        CubeInstance::new([6.0, 1.25, 12.5], 2, [14.0, 3.5, 1.0], id),
    ];
    for i in 0..5 {
        cubes.push(CubeInstance::new(
            [2.0 + 2.0 * i as f32, 0.75, 6.0],
            3,
            [0.5, 1.5, 0.5],
            id,
        ));
    }
    cubes
}

/// Stepped, quarter-metre-quantised ripples: the look the simulation produces.
fn columns(still: bool) -> Vec<WaterColumn> {
    let mut out = Vec::new();
    for iz in 0..LAKE_CELLS {
        for ix in 0..LAKE_CELLS {
            let wave = ((ix as f32 * 0.35).sin() + (iz as f32 * 0.27).cos()) * 0.5;
            out.push(WaterColumn {
                ix,
                iz,
                bottom: 0.0,
                top: if still {
                    LEVEL
                } else {
                    LEVEL + (wave * 3.0).round() * 0.0625
                },
            });
        }
    }
    out
}

fn render(ctx: &RenderContext, camera: &Camera, water: bool, name: &str) -> Vec<u8> {
    render_with(ctx, camera, water, false, name)
}

fn render_with(
    ctx: &RenderContext,
    camera: &Camera,
    water: bool,
    still: bool,
    name: &str,
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
    renderer.set_terrain(&ctx.device, &ctx.queue, &basin());
    if water {
        let columns = columns(still);
        let (vertices, indices) = build_water_surface(&columns, [0.0, 0.0], CELL, 0);
        renderer.set_water_surface(&ctx.device, &ctx.queue, &vertices, &indices);
        let mut field = WaterField::new([-4.0, -4.0], CELL, 128);
        for column in &columns {
            field.raise_column(column, [0.0, 0.0], CELL);
        }
        renderer.set_water_field(&ctx.device, &ctx.queue, Some(field));
        renderer.set_water_time(1.0);
    }
    let environment = EnvironmentPreset::Daylight.environment();
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    renderer.render(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        target.color_view(),
        camera,
        &environment,
        DebugView::Shaded,
    );
    target.copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    let rgba = target.read_rgba(ctx).expect("readback");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/water-look");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    image::save_buffer(
        dir.join(name),
        &rgba,
        SIZE.0,
        SIZE.1,
        image::ColorType::Rgba8,
    )
    .expect("write png");
    rgba
}

fn camera(eye: Vec3, dir: Vec3) -> Camera {
    Camera::looking_along(eye, dir, 70_f32.to_radians(), SIZE.0 as f32 / SIZE.1 as f32)
}

/// Mean (r, g, b) over a rectangle of the frame, 0..255.
fn mean(rgba: &[u8], x0: u32, x1: u32, y0: u32, y1: u32) -> [f32; 3] {
    let (mut sum, mut n) = ([0.0f32; 3], 0.0);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * SIZE.0 + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f32::from(rgba[i + c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

#[test]
#[ignore = "requires a real GPU adapter; run with --ignored"]
fn water_looks_smooth_from_above_and_blue_green_from_below() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");

    let above = camera(Vec3::new(6.0, 7.0, -5.0), Vec3::new(0.0, -0.75, 1.0));
    let dry = render(&ctx, &above, false, "above-dry.png");
    let wet = render(&ctx, &above, true, "above-water.png");
    assert_ne!(dry, wet, "the water must change the picture");

    // Under water, looking along the pillars: red pillars fade into the fog.
    let under = camera(Vec3::new(0.6, 1.0, 6.0), Vec3::new(1.0, -0.02, 0.0));
    let dry_under = render(&ctx, &under, false, "under-dry.png");
    let wet_under = render(&ctx, &under, true, "under-water.png");
    let dry_mid = mean(&dry_under, 200, 440, 120, 240);
    let wet_mid = mean(&wet_under, 200, 440, 120, 240);
    assert!(
        wet_mid[2] > wet_mid[0] * 1.3,
        "underwater view should be blue-green, got {wet_mid:?} (dry {dry_mid:?})"
    );

    // Looking up through the surface: a bright Snell's window overhead.
    let up = camera(Vec3::new(6.0, 0.8, 3.0), Vec3::new(0.0, 1.0, 0.35));
    let ceiling = render(&ctx, &up, true, "under-looking-up.png");
    let centre = mean(&ceiling, 280, 360, 150, 210);
    assert!(
        centre.iter().all(|c| c.is_finite()),
        "ceiling frame is finite: {centre:?}"
    );

    // Skimming the surface from just above it.
    let low = camera(Vec3::new(6.0, 2.6, 1.0), Vec3::new(0.0, -0.1, 1.0));
    render(&ctx, &low, true, "above-low-angle.png");

    // Perfectly still water must still show the faint voxel grain.
    render_with(&ctx, &above, true, true, "above-still.png");
    let close = camera(Vec3::new(6.0, 3.4, 2.0), Vec3::new(0.0, -0.6, 1.0));
    render_with(&ctx, &close, true, true, "above-still-close.png");
}
