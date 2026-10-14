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
    render_field(ctx, camera, field(still, agitation), time, name)
}

fn render_field(
    ctx: &RenderContext,
    camera: &Camera,
    water: WaterField,
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
    renderer.set_water_field(&ctx.device, &ctx.queue, Some(water));
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

/// Height of a terraced lakebed: concentric 0.25 m steps shelving toward the
/// shore, like the voxel terrain under a real lake.
fn bed_height(ix: i32, iz: i32) -> f32 {
    let (dx, dz) = ((ix - 24) as f32, (iz - 24) as f32);
    let r = (dx * dx + dz * dz).sqrt();
    (0.25 * (r / 5.0).floor()).min(1.5)
}

fn terraced_basin() -> Vec<CubeInstance> {
    let id = CubeInstance::IDENTITY_ROTATION;
    let mut cubes = vec![
        CubeInstance::new([-0.5, 1.25, 6.0], 2, [1.0, 3.5, 14.0], id),
        CubeInstance::new([12.5, 1.25, 6.0], 2, [1.0, 3.5, 14.0], id),
        CubeInstance::new([6.0, 1.25, -0.5], 2, [14.0, 3.5, 1.0], id),
        CubeInstance::new([6.0, 1.25, 12.5], 2, [14.0, 3.5, 1.0], id),
    ];
    for iz in 0..LAKE_CELLS {
        for ix in 0..LAKE_CELLS {
            let top = bed_height(ix, iz);
            cubes.push(CubeInstance::new(
                [
                    (ix as f32 + 0.5) * CELL,
                    top - 1.0,
                    (iz as f32 + 0.5) * CELL,
                ],
                1,
                [CELL, 2.0, CELL],
                id,
            ));
        }
    }
    cubes
}

fn bed_field() -> WaterField {
    let mut f = field(true, 0.0);
    let off = (-ORIGIN / CELL) as i32;
    // FILM=1 turns the right third of the bed into what a drained area leaves
    // behind: a few millimetres of unmoving water on dry ground.
    let film = std::env::var("FILM").is_ok();
    for iz in 0..LAKE_CELLS {
        for ix in 0..LAKE_CELLS {
            let i = ((ix + off) + DIM as i32 * (iz + off)) as usize;
            f.bottoms[i] = bed_height(ix, iz);
            if film && ix >= 32 {
                f.heights[i] = f.bottoms[i] + 0.004;
                f.smooth[i] = f.heights[i];
            }
        }
    }
    f
}

#[test]
#[ignore = "needs a GPU adapter; writes review PNGs"]
fn the_lakebed_seen_from_above() {
    let Some(ctx) = RenderContext::headless().ok() else {
        return;
    };
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials(),
        SIZE,
        None,
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &terraced_basin());
    // WATER=off renders the bed alone, to tell terrain artifacts from water ones.
    let water = std::env::var("WATER").map_or(true, |v| v != "off");
    if water {
        renderer.set_water_field(&ctx.device, &ctx.queue, Some(bed_field()));
        renderer.set_water_grid_surface(true);
    }
    renderer.set_water_time(2.0);
    let environment = EnvironmentPreset::Daylight.environment();
    for (name, eye, at) in [
        (
            if water {
                "bed-top.png"
            } else {
                "bed-dry-top.png"
            },
            Vec3::new(6.0, 12.0, 6.01),
            Vec3::new(6.0, 0.0, 6.0),
        ),
        (
            "bed-oblique.png",
            Vec3::new(2.0, 7.0, -1.0),
            Vec3::new(6.0, 0.5, 7.0),
        ),
    ] {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &camera(eye, at),
            &environment,
            DebugView::Shaded,
        );
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        let rgba = target.read_rgba(&ctx).expect("readback");
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/water-grid");
        std::fs::create_dir_all(&dir).unwrap();
        image::save_buffer(
            dir.join(name),
            &rgba,
            SIZE.0,
            SIZE.1,
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}

/// A lake with a uniform current of `flow` (units of 1/16 m/s) in every wet cell.
fn flowing_field(flow: [i8; 2]) -> WaterField {
    let mut f = field(true, 0.0);
    for i in 0..f.heights.len() {
        if f.heights[i] > NO_WATER * 0.5 {
            f.flow[i] = flow;
        }
    }
    f
}

/// Mean absolute difference of the water region of two renders.
fn difference(a: &[u8], b: &[u8]) -> f32 {
    let (w, h) = SIZE;
    let mut sum = 0.0f32;
    let mut n = 0.0;
    for y in h / 2..3 * h / 4 {
        for x in w / 3..2 * w / 3 {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                sum += (f32::from(a[i + c]) - f32::from(b[i + c])).abs();
                n += 1.0;
            }
        }
    }
    sum / n
}

/// A current changes the water: ripples travel with it (so the pattern at a
/// later time differs from a still lake's by more than the slow still drift),
/// run in streaks, and a fast one breaks into foam. Writes review PNGs.
#[test]
#[ignore = "needs a GPU adapter; writes review PNGs"]
fn a_current_carries_the_ripples_and_a_rapid_breaks_into_foam() {
    let Some(ctx) = RenderContext::headless().ok() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let from_bank = camera(Vec3::new(6.0, 4.2, -3.2), Vec3::new(6.0, 1.8, 7.0));
    let still_a = render_field(
        &ctx,
        &from_bank,
        flowing_field([0, 0]),
        2.0,
        "flow-still-t2.png",
    );
    let still_b = render_field(
        &ctx,
        &from_bank,
        flowing_field([0, 0]),
        2.5,
        "flow-still-t2.5.png",
    );
    // 4 m/s along +x, then the same current half a second later.
    let river_a = render_field(
        &ctx,
        &from_bank,
        flowing_field([24, 0]),
        2.0,
        "flow-river-t2.png",
    );
    let river_b = render_field(
        &ctx,
        &from_bank,
        flowing_field([24, 0]),
        2.5,
        "flow-river-t2.5.png",
    );
    // A gentle 0.4 m/s drift.
    let creek_a = render_field(
        &ctx,
        &from_bank,
        flowing_field([6, 0]),
        2.0,
        "flow-creek-t2.png",
    );
    let creek_b = render_field(
        &ctx,
        &from_bank,
        flowing_field([6, 0]),
        2.5,
        "flow-creek-t2.5.png",
    );
    // Cross-stream current: a different direction draws a different pattern.
    let cross = render_field(
        &ctx,
        &from_bank,
        flowing_field([0, 24]),
        2.0,
        "flow-cross-t2.png",
    );

    let still_drift = difference(&still_a, &still_b);
    let river_change = difference(&river_a, &river_b);
    let creek_change = difference(&creek_a, &creek_b);
    eprintln!(
        "mean difference: still drift {still_drift:.2}, creek {creek_change:.2}, river {river_change:.2},          river vs still {:.2}, cross vs river {:.2}",
        difference(&river_a, &still_a),
        difference(&cross, &river_a)
    );
    assert!(
        river_change > still_drift * 1.5,
        "the pattern on a river moves faster than on still water: {river_change} vs {still_drift}"
    );
    assert!(
        creek_change > still_drift,
        "{creek_change} vs {still_drift}"
    );
    assert!(
        difference(&river_a, &still_a) > 1.0,
        "a rapid looks different from a calm lake"
    );
    assert!(
        difference(&cross, &river_a) > 1.0,
        "direction changes the pattern"
    );
    // The rapid is foamier (brighter, whiter) than the calm lake.
    let (w, h) = SIZE;
    let brightness = |img: &[u8]| {
        let m = mean(img, w / 3, 2 * w / 3, h / 2, 3 * h / 4);
        m[0] + m[1] + m[2]
    };
    assert!(
        brightness(&river_a) > brightness(&still_a),
        "foam brightens the rapid"
    );
}

/// A replacement field goes up a slice at a time and only then takes over: the
/// old field draws until the last row is in, and a rectangle update meanwhile is
/// for the new field.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_replacement_water_field_swaps_in_after_its_last_row() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials(),
        SIZE,
        None,
    );
    let field = |x: f32| spall_render::water::WaterField::new([x, 0.0], 0.25, 128);
    renderer.replace_water_field(&ctx.device, &ctx.queue, field(0.0));
    assert_eq!(
        renderer.water_field_origin(),
        Some([0.0, 0.0]),
        "the first field installs at once"
    );

    renderer.replace_water_field(&ctx.device, &ctx.queue, field(32.0));
    assert_eq!(renderer.water_field_origin(), Some([0.0, 0.0]));
    // A rectangle for the new field while it uploads is accepted, and does not
    // move the field being drawn.
    renderer.update_water_field_rect(&ctx.queue, 4, 4, 1, 1, &[[1.0, 0.0, 1.0, 0.0]]);
    assert!(
        renderer.advance_water_upload(&ctx.queue, 64),
        "64 of 128 rows"
    );
    assert_eq!(renderer.water_field_origin(), Some([0.0, 0.0]));
    assert!(
        !renderer.advance_water_upload(&ctx.queue, 64),
        "the last rows"
    );
    assert_eq!(renderer.water_field_origin(), Some([32.0, 0.0]));
    assert!(
        !renderer.advance_water_upload(&ctx.queue, 64),
        "nothing pending"
    );

    // A second replacement started before the first finished supersedes it.
    renderer.replace_water_field(&ctx.device, &ctx.queue, field(64.0));
    assert!(renderer.advance_water_upload(&ctx.queue, 64));
    renderer.replace_water_field(&ctx.device, &ctx.queue, field(96.0));
    assert!(renderer.advance_water_upload(&ctx.queue, 64));
    assert!(!renderer.advance_water_upload(&ctx.queue, 64));
    assert_eq!(renderer.water_field_origin(), Some([96.0, 0.0]));
}
