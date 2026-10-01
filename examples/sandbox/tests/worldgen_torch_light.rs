#![cfg(feature = "client")]
//! Does a torch (one emissive lamp voxel) actually light the generated terrain
//! around it, and without filling the hole it sits in? Renders the meadow at
//! night through the same pipeline the
//! interactive window uses (`build_instances` + sky occupancy + `GameRenderer`),
//! once without and once with a lamp on the ground, and compares the brightness
//! of the ground a metre from it. Writes both images to
//! `.local/runs/torch-light/` for a look.
//!
//! Needs a GPU adapter. Run: `cargo test -p sandbox --features client --test worldgen_torch_light -- --nocapture`.

use sandbox::game::{self, materials};
use sandbox::worldgen_scene;
use spall_client::sky::build_sky_occupancy;
use spall_client::window::{build_instances, emitter_point_lights};
use spall_core::GlobalCell;
use spall_render::{
    Camera, DebugView, EnvironmentPreset, GameRenderer, OffscreenTarget, RenderContext,
};
use spall_voxel::EditPlan;

const SIZE: (u32, u32) = (640, 360);

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Mean linear luma of a `radius`-pixel square around the projection of `p`.
fn luma_around(camera: &Camera, rgba: &[u8], p: glam::Vec3, radius: i64) -> f32 {
    let ndc = camera.project(p).expect("in front of the camera");
    let cx = ((ndc[0] * 0.5 + 0.5) * SIZE.0 as f32) as i64;
    let cy = ((0.5 - ndc[1] * 0.5) * SIZE.1 as f32) as i64;
    let (mut sum, mut n) = (0.0, 0.0);
    for y in (cy - radius).max(0)..=(cy + radius).min(i64::from(SIZE.1) - 1) {
        for x in (cx - radius).max(0)..=(cx + radius).min(i64::from(SIZE.0) - 1) {
            let i = ((y as u32 * SIZE.0 + x as u32) * 4) as usize;
            sum += srgb_to_linear(rgba[i + 1]);
            n += 1.0;
        }
    }
    sum / n
}

fn render(
    ctx: &RenderContext,
    volume: &spall_voxel::Volume,
    center_m: [f64; 3],
    camera: &Camera,
    preset: EnvironmentPreset,
) -> Vec<u8> {
    let manifest = game::manifest();
    let materials = spall_client::window::terrain_render_materials(&manifest);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        SIZE,
        None,
    );
    let instances = build_instances(volume, center_m);
    renderer.set_terrain(&ctx.device, &ctx.queue, &instances);
    // The window lights emissive voxels with point lights; so does this.
    let eye = glam::Vec3::new(center_m[0] as f32, center_m[1] as f32, center_m[2] as f32);
    println!(
        "lamp instances: {} of {}; first few materials {:?}",
        instances
            .iter()
            .filter(|i| i.material == u32::from(materials::LAMP.0))
            .count(),
        instances.len(),
        instances
            .iter()
            .map(|i| i.material)
            .take(5)
            .collect::<Vec<_>>()
    );
    let lights = emitter_point_lights(&instances, &materials, eye);
    println!(
        "point lights: {} ({} emissive materials, lamp emissive {})",
        lights.len(),
        materials.iter().filter(|m| m.emissive > 0.0).count(),
        materials[usize::from(materials::LAMP.0)].emissive
    );
    renderer.set_point_lights(&lights);
    let (sky, _) = build_sky_occupancy(volume, center_m, true);
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&sky));
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let environment = preset.environment();
    let mut rgba = Vec::new();
    let mut frames = 0;
    // A few frames, and until the lighting cache has finished sweeping.
    while frames < 4 || (renderer.lighting_is_sweeping() && frames < 60) {
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
        rgba = target.read_rgba(ctx).expect("readback");
        frames += 1;
    }
    rgba
}

#[test]
fn a_torch_lights_the_ground_around_it() {
    let scene = worldgen_scene::generate("showcase", 1, 256).expect("generate");
    let spawn = scene.player_spawns()[0];
    let (x, z) = (
        (spawn[0] / 0.25).floor() as i64,
        (spawn[2] / 0.25).floor() as i64,
    );
    let ground_y = (spawn[1] / 0.25).round() as i64 - 1;
    let dark = scene.world().terrain.clone();
    let mut lit = dark.clone();
    let torch = GlobalCell::new(x, ground_y + 1, z);
    // The window's torch size; TORCH_RADIUS overrides it to explore other sizes.
    let radius: i64 = std::env::var("TORCH_RADIUS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(spall_client::window::TORCH_RADIUS_CELLS);
    // Centred on the cell, like the server's placement brush.
    let unit = spall_core::units::BRUSH_UNIT;
    let plan = EditPlan::sphere(
        lit.id(),
        spall_core::SphereBrush::new(
            spall_core::units::BrushPoint::from_units(
                torch.x * unit + unit / 2,
                torch.y * unit + unit / 2,
                torch.z * unit + unit / 2,
            ),
            radius * unit,
        )
        .expect("brush"),
        materials::LAMP,
    );
    lit.apply_edit(&plan).expect("place the torch");

    let torch_m = glam::Vec3::new(
        (x as f32 + 0.5) * 0.25,
        (ground_y as f32 + 1.5) * 0.25,
        (z as f32 + 0.5) * 0.25,
    );
    // A player's eye 2.5 m back and 1.6 m up, looking at the torch.
    let eye = torch_m + glam::Vec3::new(0.0, 1.4, 2.5);
    let camera = Camera::looking_along(
        eye,
        torch_m - eye,
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let center = [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)];

    let ctx = RenderContext::headless().expect("a GPU adapter");
    let before = render(&ctx, &dark, center, &camera, EnvironmentPreset::Night);
    let after = render(&ctx, &lit, center, &camera, EnvironmentPreset::Night);

    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/torch-light");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, rgba) in [("dark.png", &before), ("torch.png", &after)] {
        image::save_buffer(
            dir.join(name),
            rgba,
            SIZE.0,
            SIZE.1,
            image::ColorType::Rgba8,
        )
        .unwrap();
    }

    // Ground a metre in front of and beside the torch.
    let mut worst_ratio = f32::MAX;
    for offset in [
        glam::Vec3::new(1.0, -0.125, 0.0),
        glam::Vec3::new(-1.0, -0.125, 0.0),
    ] {
        let p = torch_m + offset;
        let (b, a) = (
            luma_around(&camera, &before, p, 6),
            luma_around(&camera, &after, p, 6),
        );
        println!(
            "ground at {offset:?}: dark {b:.5} -> torch {a:.5} ({:.1}x)",
            a / b.max(1e-6)
        );
        worst_ratio = worst_ratio.min(a / b.max(1e-6));
    }
    assert!(
        worst_ratio > 1.3,
        "a torch must visibly light the ground a metre away (worst ratio {worst_ratio})"
    );
}

/// Looks into a hammered pit in daylight: the layers under the grass, the
/// pockets in them, and the per-voxel colour variation. Writes
/// `.local/runs/torch-light/pit.png` for a look; asserts that the pit wall
/// shows several materials.
#[test]
fn a_dug_pit_shows_layers_and_colour_variation() {
    let scene = worldgen_scene::generate("showcase", 1, 256).expect("generate");
    let spawn = scene.player_spawns()[0];
    let (x, z) = (
        (spawn[0] / 0.25).floor() as i64,
        (spawn[2] / 0.25).floor() as i64,
    );
    let ground_y = (spawn[1] / 0.25).round() as i64 - 1;
    let mut dug = scene.world().terrain.clone();
    let unit = spall_core::units::BRUSH_UNIT;
    // Two hammer swings deep: a radius-8 pit centred on the surface, then one 6 cells down.
    for dy in [0, -6] {
        let plan = EditPlan::sphere(
            dug.id(),
            spall_core::SphereBrush::new(
                spall_core::units::BrushPoint::from_units(
                    x * unit + unit / 2,
                    (ground_y + dy) * unit + unit / 2,
                    z * unit + unit / 2,
                ),
                8 * unit,
            )
            .expect("brush"),
            spall_core::MaterialId::AIR,
        );
        dug.apply_edit(&plan).expect("dig");
    }
    let centre = glam::Vec3::new(
        (x as f32 + 0.5) * 0.25,
        (ground_y as f32 - 4.0) * 0.25,
        (z as f32 + 0.5) * 0.25,
    );
    let eye = centre + glam::Vec3::new(1.2, 2.2, 3.4);
    let camera = Camera::looking_along(
        eye,
        centre - eye,
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let ctx = RenderContext::headless().expect("a GPU adapter");
    let rgba = render(
        &ctx,
        &dug,
        [f64::from(eye.x), f64::from(eye.y), f64::from(eye.z)],
        &camera,
        EnvironmentPreset::Daylight,
    );
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/torch-light");
    std::fs::create_dir_all(&dir).unwrap();
    image::save_buffer(
        dir.join("pit.png"),
        &rgba,
        SIZE.0,
        SIZE.1,
        image::ColorType::Rgba8,
    )
    .unwrap();

    // The pit wall exposes more than one material down its depth.
    let mut seen = std::collections::BTreeSet::new();
    for dy in 0..=10 {
        for dx in [-8, 8] {
            if let Ok(spall_voxel::Sample::Filled(m)) =
                dug.sample(GlobalCell::new(x + dx + dx.signum(), ground_y - dy, z))
            {
                seen.insert(m);
            }
        }
    }
    assert!(seen.len() >= 2, "a pit wall should show layers: {seen:?}");
}
