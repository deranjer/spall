//! The baseline sun-shadow technique (cascaded maps + normal-offset bias +
//! PCSS) against a real GPU. See `docs/reports/ENG-96.md` for the technique and
//! the measured limits these tests pin down.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render --test shadow_technique_gpu -- --ignored`.

use glam::Vec3;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, Material,
    OffscreenTarget, RenderContext,
};

const SIZE: (u32, u32) = (640, 480);
const IDENTITY: [f32; 4] = CubeInstance::IDENTITY_ROTATION;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),
        Material::new([0.8, 0.8, 0.8], 0.9, 0.0),
    ]
}

fn render(
    ctx: &RenderContext,
    camera: &Camera,
    environment: &Environment,
    view: DebugView,
    terrain: &[CubeInstance],
    bodies: &[CubeInstance],
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
    target.read_rgba(ctx).expect("readback")
}

fn save_png(name: &str, rgba: &[u8]) {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-96-shadows");
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

fn screen(camera: &Camera, p: Vec3) -> (f32, f32) {
    let ndc = camera.project(p).expect("in front of the camera");
    (
        (ndc[0] * 0.5 + 0.5) * SIZE.0 as f32,
        (0.5 - ndc[1] * 0.5) * SIZE.1 as f32,
    )
}

fn red(rgba: &[u8], x: u32, y: u32) -> f32 {
    f32::from(rgba[((y * SIZE.0 + x) * 4) as usize]) / 255.0
}

fn ground() -> CubeInstance {
    CubeInstance::new([0.0, -0.25, 0.0], 1, [60.0, 0.5, 60.0], IDENTITY)
}

/// 10 %-90 % transition width, in pixels, of an edge along one image row
/// centred on `edge_x`; either side may be the brighter one.
fn edge_width_px(rgba: &[u8], row: u32, edge_x: f32, half_span: f32) -> f32 {
    let lo = (edge_x - half_span).max(1.0) as u32;
    let hi = ((edge_x + half_span) as u32).min(SIZE.0 - 2);
    let profile: Vec<f32> = (lo..=hi).map(|x| red(rgba, x, row)).collect();
    let start = profile[..4].iter().sum::<f32>() / 4.0;
    let end = profile[profile.len() - 4..].iter().sum::<f32>() / 4.0;
    assert!(
        (start - end).abs() > 0.04,
        "no lit/shadow contrast at row {row}: {start} vs {end}"
    );
    // Progress from `start` to `end`, 0..1.
    let progress = |v: f32| (v - start) / (end - start);
    let first_at = |level: f32| profile.iter().position(|v| progress(*v) >= level).unwrap() as f32;
    first_at(0.9) - first_at(0.1)
}

/// The penumbra is physically motivated: the same-sized occluder casts a much
/// softer edge when it is far above the receiver than when it is close.
#[test]
#[ignore = "requires a working GPU adapter"]
fn penumbra_widens_with_occluder_distance() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    // A bright sun with an exaggerated angular size, so the penumbra is wide
    // enough to measure and the contrast survives it.
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_angular_diameter_deg = 6.0;
    // A low sun so each strip's shadow lands clear of the strip itself.
    environment.sun_dir = Vec3::new(-0.9, -0.44, 0.0).normalize();
    // Two thin 4 m strips, one close to the ground and one high above it.
    let strip = |y: f32, z: f32| CubeInstance::new([0.0, y, z], 1, [4.0, 0.05, 6.0], IDENTITY);
    let (low_y, high_y, low_z, high_z) = (0.6, 6.0, -4.0, 4.0);
    let bodies = [strip(low_y, low_z), strip(high_y, high_z)];
    let camera = Camera::looking_along(
        Vec3::new(-8.0, 24.0, 0.0),
        Vec3::new(0.0, -1.0, -0.1),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let image = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &[ground()],
        &bodies,
    );
    save_png("penumbra-6deg-sun.png", &image);

    // The -x edge of each strip's shadow: lit to its left, shadowed to its right.
    let sun = environment.sun_dir;
    let edge = |y: f32, z: f32| {
        let x = -2.0 + sun.x * (-y / sun.y);
        screen(&camera, Vec3::new(x, 0.0, z))
    };
    let (low_x, low_row) = edge(low_y, low_z);
    let (high_x, high_row) = edge(high_y, high_z);
    let low = edge_width_px(&image, low_row as u32, low_x, 10.0);
    let high = edge_width_px(&image, high_row as u32, high_x, 60.0);
    println!("penumbra width: low occluder {low:.1} px, high occluder {high:.1} px");
    assert!(
        high > low * 3.0,
        "penumbra should widen with occluder distance: low {low}, high {high}"
    );
    assert!(
        low < 12.0,
        "a close occluder keeps a crisp edge, got {low} px"
    );
}

/// Shadows start at the contact: no gap between an object's base and the
/// shadow it casts (no "peter-panning"), and no acne on lit faces.
#[test]
#[ignore = "requires a working GPU adapter"]
fn contact_shadows_start_at_the_contact_without_acne() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_dir = Vec3::new(-0.6, -0.8, 0.0);
    let terrain = [
        ground(),
        CubeInstance::new([0.0, 0.5, 0.0], 1, [1.0, 1.0, 1.0], IDENTITY),
    ];
    let camera = Camera::looking_along(
        Vec3::new(-2.5, 3.2, 3.2),
        Vec3::new(2.5, -3.2, -3.2),
        50_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let vis = render(
        &ctx,
        &camera,
        &environment,
        DebugView::ShadowVisibility,
        &terrain,
        &[],
    );
    let shaded = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &terrain,
        &[],
    );
    save_png("contact-shadow-visibility.png", &vis);
    save_png("contact-shaded.png", &shaded);

    let at = |p: Vec3| {
        let (x, y) = screen(&camera, p);
        (x as u32, y as u32)
    };
    // Ground 3 cm from the cube's -x face, in the cube's shadow (light travels
    // toward -x); the camera looks at that side of the cube.
    let (x, y) = at(Vec3::new(-0.53, 0.0, 0.0));
    println!("contact probe at pixel ({x}, {y})");
    let contact = red(&vis, x, y);
    // Ground well clear of the cube, in the sun.
    let (x, y) = at(Vec3::new(-2.5, 0.0, -1.5));
    let open = red(&vis, x, y);
    // The cube's own sunlit -x face and top face: no self-shadow acne.
    let (x, y) = at(Vec3::new(0.0, 1.0, 0.0));
    let top = red(&vis, x, y);
    println!("visibility (sRGB-encoded): contact {contact:.2}, open {open:.2}, cube top {top:.2}");
    assert!(
        contact < 0.2,
        "shadow should reach the contact line, got {contact}"
    );
    assert!(open > 0.95, "open ground should be fully lit, got {open}");
    assert!(
        top > 0.95,
        "the lit top face must not self-shadow (acne), got {top}"
    );
}
