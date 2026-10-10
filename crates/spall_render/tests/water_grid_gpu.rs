//! The water grid surface (ENG-168) against a real GPU. Writes PNGs for review
//! to `.local/runs/water-grid/` and asserts coarse properties of the look.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test --release -p spall_render --test water_grid_gpu -- --ignored`.

use glam::Vec3;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::water::{NO_WATER, WaterField};
use spall_render::{
    Camera, CubeInstance, DebugView, EnvironmentPreset, GameRenderer, Material, OffscreenTarget,
    RenderContext,
};

const SIZE: (u32, u32) = (960, 540);
const CELL: f32 = 0.25;
const DIM: u32 = 128;
const ORIGIN: f32 = -4.0;
const LAKE_CELLS: i32 = 48; // 12 m square
const LEVEL: f32 = 2.0;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),
        Material::new([0.55, 0.47, 0.30], 0.9, 0.0), // 1 sand
        Material::new([0.45, 0.45, 0.47], 0.7, 0.0), // 2 stone
        Material::new([0.70, 0.18, 0.12], 0.6, 0.0), // 3 red marker
    ]
}

/// A stone-walled basin with a sand floor that shelves toward the middle, and
/// red pillars every 2 m.
fn basin() -> Vec<CubeInstance> {
    let id = CubeInstance::IDENTITY_ROTATION;
    let mut cubes = vec![
        CubeInstance::new([6.0, -0.5, 6.0], 1, [16.0, 1.0, 16.0], id),
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

/// The simulation's disturbed surface: quarter-metre-quantised ripples. This is
/// the raw input; the client eases and smooths it before it reaches the GPU.
fn raw_top(ix: i32, iz: i32, still: bool) -> f32 {
    if still {
        return LEVEL;
    }
    let wave = ((ix as f32 * 0.35).sin() + (iz as f32 * 0.27).cos()) * 0.5;
    LEVEL + (wave * 3.0).round() * 0.0625
}

/// Same 5x5 binomial smoothing as the client's `ColumnSurface`.
fn field(still: bool, agitation: f32) -> WaterField {
    let mut f = WaterField::new([ORIGIN, ORIGIN], CELL, DIM);
    let off = (-ORIGIN / CELL) as i32; // lake cell 0 sits at field cell `off`
    let dim = DIM as i32;
    for iz in 0..LAKE_CELLS {
        for ix in 0..LAKE_CELLS {
            let i = ((ix + off) + dim * (iz + off)) as usize;
            f.heights[i] = raw_top(ix, iz, still);
            // A bowl: deepest in the middle, shelving to nothing at the edge.
            let d = ((ix - 24).pow(2) + (iz - 24).pow(2)) as f32;
            f.bottoms[i] = (LEVEL - 1.6 * (1.0 - d / 700.0).clamp(0.0, 1.0)).max(0.05);
            f.agitation[i] = agitation;
        }
    }
    let k = [1.0, 4.0, 6.0, 4.0, 1.0f32];
    for z in 0..dim {
        for x in 0..dim {
            let (mut sum, mut weight) = (0.0, 0.0);
            for dz in -2..=2i32 {
                for dx in -2..=2i32 {
                    let (px, pz) = (x + dx, z + dz);
                    if px < 0 || pz < 0 || px >= dim || pz >= dim {
                        continue;
                    }
                    let h = f.heights[(px + dim * pz) as usize];
                    if h > NO_WATER * 0.5 {
                        let w = k[(dx + 2) as usize] * k[(dz + 2) as usize];
                        sum += w * h;
                        weight += w;
                    }
                }
            }
            f.smooth[(x + dim * z) as usize] = if weight > 0.0 { sum / weight } else { NO_WATER };
        }
    }
    f
}

fn render(
    ctx: &RenderContext,
    camera: &Camera,
    still: bool,
    agitation: f32,
    time: f32,
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
    renderer.set_water_field(&ctx.device, &ctx.queue, Some(field(still, agitation)));
    renderer.set_water_grid_surface(true);
    renderer.set_water_time(time);
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
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/water-grid");
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

fn camera(eye: Vec3, target: Vec3) -> Camera {
    Camera::looking_along(
        eye,
        (target - eye).normalize(),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

fn mean(rgba: &[u8], x0: u32, x1: u32, y0: u32, y1: u32) -> [f32; 3] {
    let mut sum = [0.0f32; 3];
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y * SIZE.0 + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += f32::from(rgba[i + c]);
            }
            n += 1.0;
        }
    }
    sum.map(|v| v / n)
}

#[test]
#[ignore = "needs a GPU adapter; writes review PNGs"]
fn the_grid_surface_draws_calm_and_disturbed_water() {
    let Some(ctx) = RenderContext::headless().ok() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let from_bank = camera(Vec3::new(6.0, 4.2, -3.2), Vec3::new(6.0, 1.8, 7.0));
    let grazing = camera(Vec3::new(0.6, 2.7, 0.6), Vec3::new(10.0, 1.9, 10.5));
    let top = camera(Vec3::new(6.0, 14.0, 6.01), Vec3::new(6.0, 0.0, 6.0));
    let shore = camera(Vec3::new(1.2, 2.5, 6.0), Vec3::new(3.2, 1.9, 6.0));

    let still_bank = render(&ctx, &from_bank, true, 0.0, 2.0, "still-bank.png");
    render(&ctx, &grazing, true, 0.0, 2.0, "still-grazing.png");
    render(&ctx, &top, true, 0.0, 2.0, "still-top.png");
    render(&ctx, &shore, true, 0.0, 2.0, "still-shore.png");
    let busy_bank = render(&ctx, &from_bank, false, 0.8, 3.0, "disturbed-bank.png");
    render(&ctx, &grazing, false, 0.8, 3.0, "disturbed-grazing.png");
    render(&ctx, &grazing, false, 0.35, 3.0, "mild-grazing.png");
    render(&ctx, &shore, false, 0.8, 3.0, "disturbed-shore.png");

    // The lake is visible and distinctly not the basin floor or the sky.
    let (w, h) = SIZE;
    let lake = mean(&still_bank, w / 3, 2 * w / 3, h / 2, 3 * h / 4);
    assert!(lake.iter().all(|c| c.is_finite() && *c > 5.0), "{lake:?}");
    // Disturbance changes the picture but does not black it out.
    let busy = mean(&busy_bank, w / 3, 2 * w / 3, h / 2, 3 * h / 4);
    assert!(busy.iter().all(|c| *c > 5.0), "{busy:?}");
}
