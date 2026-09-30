//! The one-diffuse-bounce pass against a real GPU: colour bleed from sunlit
//! surfaces, emissive illumination, thin-wall behaviour, the furnace
//! calibration, and no double counting of skylight. See `docs/reports/ENG-97.md`.
//!
//! All comparisons use the indirect-only debug view (bounce alone, sun and
//! ambient off) at fixed exposure. Ignored by default; run with
//! `cargo test -p spall_render --test bounce_gpu -- --ignored --nocapture`.

use glam::{Quat, Vec3};
use spall_render::indirect::LightingVolume;
use spall_render::instances::unit_cube;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, GpuVertex,
    Material, MeshChunk, OffscreenTarget, RenderContext, ViewportFrame, ViewportRenderer,
    cache_origin_around, mark_world_box,
};

const SIZE: (u32, u32) = (480, 360);
const IDENTITY: [f32; 4] = CubeInstance::IDENTITY_ROTATION;

// Material ids.
const WHITE: u32 = 1;
const RED: u32 = 2;
const LAMP: u32 = 3;
const STONE: u32 = 4;
const GLOW: u32 = 5;

fn materials() -> Vec<Material> {
    vec![
        Material::default(),
        Material::new([0.8, 0.8, 0.8], 0.9, 0.0),
        Material::new([0.8, 0.05, 0.05], 0.9, 0.0),
        Material::new([1.0, 0.45, 0.1], 0.9, 0.0).emissive(6.0),
        Material::new([0.5, 0.5, 0.5], 0.9, 0.0),
        Material::new([0.5, 0.5, 0.5], 0.9, 0.0).emissive(1.0),
    ]
}

fn boxed(material: u32, center: [f32; 3], size: [f32; 3]) -> CubeInstance {
    CubeInstance::new(center, material, size, IDENTITY)
}

fn ground() -> CubeInstance {
    boxed(WHITE, [0.0, -0.25, 0.0], [60.0, 0.5, 60.0])
}

fn occupancy(boxes: &[CubeInstance]) -> LightingVolume {
    let mut volume = LightingVolume::empty(cache_origin_around(Vec3::ZERO));
    for b in boxes {
        let (c, h) = (Vec3::from_array(b.offset), Vec3::from_array(b.size) * 0.5);
        mark_world_box(&mut volume, c - h, c + h, b.material);
    }
    volume
}

fn render(
    ctx: &RenderContext,
    camera: &Camera,
    environment: &Environment,
    view: DebugView,
    boxes: &[CubeInstance],
    bounce: bool,
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
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&occupancy(boxes)));
    renderer.set_bounce_enabled(&ctx.queue, bounce);
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
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-97-bounce");
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

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear RGB at the pixel a world point projects to (indirect-only view is
/// written linear and encoded once by the target).
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

fn top_camera() -> Camera {
    Camera::looking_along(
        Vec3::new(2.0, 14.0, 6.0),
        Vec3::new(0.0, -1.0, -0.35),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

/// Sun travelling toward -x, so a wall at x = 0 is lit on its +x face.
fn sunny() -> Environment {
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_dir = Vec3::new(-0.8, -0.55, 0.0).normalize();
    environment
}

/// A tall red wall at x = 0 (thickness 0.5 m) on a white floor.
fn red_wall_scene() -> Vec<CubeInstance> {
    vec![ground(), boxed(RED, [0.0, 2.0, 0.0], [0.5, 4.0, 14.0])]
}

/// Colour bleed comes from the actual lighting: the sunlit side of a red wall
/// tints the floor beside it red; the shadowed side does not; and turning the
/// sun off removes it.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_sunlit_red_wall_bleeds_red_onto_the_floor_beside_it() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let boxes = red_wall_scene();
    let environment = sunny();

    let bounce = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &boxes,
        true,
    );
    save_png("red-wall-indirect-only.png", &bounce);
    let shaded = render(&ctx, &camera, &environment, DebugView::Shaded, &boxes, true);
    save_png("red-wall-shaded.png", &shaded);
    let shaded_off = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &boxes,
        false,
    );
    save_png("red-wall-shaded-bounce-off.png", &shaded_off);

    let lit_side = Vec3::new(1.0, 0.0, 0.0);
    let far_lit_side = Vec3::new(9.0, 0.0, 0.0);
    let shadow_side = Vec3::new(-1.0, 0.0, 0.0);
    let near = at(&camera, &bounce, lit_side);
    let far = at(&camera, &bounce, far_lit_side);
    let behind = at(&camera, &bounce, shadow_side);
    println!(
        "bounce beside the lit face {near:?}, 9 m away {far:?}, beside the shadowed face {behind:?}"
    );
    assert!(
        near[0] > 0.02 && near[0] > near[1] * 3.0,
        "floor beside a sunlit red wall should be red: {near:?}"
    );
    assert!(
        near[0] > far[0] * 3.0,
        "bleed should fall off with distance: {near:?} vs {far:?}"
    );
    assert!(
        near[0] > behind[0] * 3.0,
        "the shadowed face must not bleed as strongly: {near:?} vs {behind:?}"
    );

    // The same scene with the sun off: the wall is no longer lit, so the
    // bounce is the sky-lit remainder only.
    let mut dark = environment;
    dark.sun_intensity = 0.0;
    let no_sun = render(&ctx, &camera, &dark, DebugView::IndirectOnly, &boxes, true);
    let dim = at(&camera, &no_sun, lit_side);
    println!("same spot with the sun off {dim:?}");
    assert!(
        near[0] > dim[0] * 4.0,
        "bounce must follow the actual light, not a fixed brightness: {near:?} vs {dim:?}"
    );

    // In the full render the bounce lifts the floor beside the wall and tints
    // it warm, compared with the same frame with the bounce off.
    let (on, off) = (
        at(&camera, &shaded, lit_side),
        at(&camera, &shaded_off, lit_side),
    );
    println!("shaded floor beside the wall: bounce on {on:?}, off {off:?}");
    assert!(
        on[0] > off[0] && (on[0] - off[0]) > (on[2] - off[2]) * 2.0,
        "{on:?} vs {off:?}"
    );
}

/// An emissive object illuminates nearby non-emissive surfaces, with no sun and
/// no sky to help.
#[test]
#[ignore = "requires a working GPU adapter"]
fn an_emissive_object_lights_its_neighbours() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    let lamp = |material| vec![ground(), boxed(material, [0.0, 0.5, 0.0], [1.0, 1.0, 1.0])];

    let lit = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &lamp(LAMP),
        true,
    );
    save_png("emissive-lamp-indirect-only.png", &lit);
    let shaded = render(
        &ctx,
        &camera,
        &environment,
        DebugView::Shaded,
        &lamp(LAMP),
        true,
    );
    save_png("emissive-lamp-shaded.png", &shaded);
    let control = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &lamp(STONE),
        true,
    );

    let (near_p, far_p) = (Vec3::new(2.0, 0.0, 0.0), Vec3::new(9.0, 0.0, 0.0));
    let (near, far) = (at(&camera, &lit, near_p), at(&camera, &lit, far_p));
    let none = at(&camera, &control, near_p);
    println!(
        "emissive lamp: floor 2 m away {near:?}, 9 m away {far:?}; non-emissive control {none:?}"
    );
    assert!(
        near[0] > 0.01,
        "the lamp should light the floor beside it: {near:?}"
    );
    assert!(
        near[0] > far[0] * 4.0,
        "light falls off with distance: {near:?} vs {far:?}"
    );
    assert!(
        near[0] > none[0] * 20.0 + 1e-4,
        "control without emission stays dark: {none:?}"
    );
    assert!(
        near[0] > near[2] * 2.0,
        "the lamp is orange, so is its light: {near:?}"
    );
}

/// The bounce adds no skylight: open ground under a bright sky has nothing to
/// bounce, so the indirect-only view is black (skylight is the opaque pass's
/// visibility term, and counting it here too would double count it).
#[test]
#[ignore = "requires a working GPU adapter"]
fn the_bounce_does_not_double_count_skylight() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let image = render(
        &ctx,
        &camera,
        &sunny(),
        DebugView::IndirectOnly,
        &[ground()],
        true,
    );
    let peak = image
        .chunks_exact(4)
        .map(|p| p[0].max(p[1]).max(p[2]))
        .max()
        .unwrap();
    println!("open ground, indirect-only peak {peak}/255");
    assert!(peak <= 2, "open ground must have no bounce, got {peak}/255");
}

/// Furnace calibration: a sealed room whose every surface emits the same
/// radiance L. A surface sees L in (almost) every direction, so the reflected
/// bounce should be about `albedo * L`.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_uniformly_glowing_room_bounces_albedo_times_radiance() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    // 8 m cube room, one-cell (0.5 m) walls, every surface the same glowing
    // material: albedo 0.5, emission = 0.5 * 1.0 = 0.5.
    let boxes = vec![
        boxed(GLOW, [0.0, -0.25, 0.0], [12.0, 0.5, 12.0]),
        boxed(GLOW, [0.0, 8.25, 0.0], [12.0, 0.5, 12.0]),
        boxed(GLOW, [-4.25, 4.0, 0.0], [0.5, 8.0, 12.0]),
        boxed(GLOW, [4.25, 4.0, 0.0], [0.5, 8.0, 12.0]),
        boxed(GLOW, [0.0, 4.0, -4.25], [9.0, 8.0, 0.5]),
        boxed(GLOW, [0.0, 4.0, 4.25], [9.0, 8.0, 0.5]),
    ];
    let camera = Camera::looking_along(
        Vec3::new(0.0, 4.0, 3.5),
        Vec3::new(0.0, -0.55, -1.0),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let image = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &boxes,
        true,
    );
    save_png("furnace-indirect-only.png", &image);
    let floor = at(&camera, &image, Vec3::new(0.5, 0.0, -1.0));
    let expected = 0.5 * 0.5; // albedo * L
    println!("furnace floor bounce {floor:?}, expected about {expected}");
    assert!(
        (floor[0] - expected).abs() < expected * 0.35,
        "furnace calibration: got {}, expected about {expected}",
        floor[0]
    );
}

/// Thin walls: one voxel (0.25 m, half a cache cell), one cell (0.5 m), and no
/// wall between an emitter and a receiving floor patch. The cache marks a cell
/// occupied if any voxel in it is, so even a one-voxel wall blocks; this
/// measures how much still leaks.
#[test]
#[ignore = "requires a working GPU adapter"]
fn thin_walls_block_the_bounce() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = Camera::looking_along(
        Vec3::new(3.0, 12.0, 4.0),
        Vec3::new(0.0, -1.0, -0.3),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    let scene = |wall: Option<f32>| {
        let mut boxes = vec![ground(), boxed(LAMP, [-2.5, 0.5, 0.0], [1.0, 1.0, 1.0])];
        if let Some(thickness) = wall {
            // A tall wall between the lamp and the receiver, at x = -1.25.
            boxes.push(boxed(STONE, [-1.25, 2.0, 0.0], [thickness, 4.0, 14.0]));
        }
        boxes
    };
    let probe = Vec3::new(-0.25, 0.0, 0.0);
    let measure = |wall| {
        let image = render(
            &ctx,
            &camera,
            &environment,
            DebugView::IndirectOnly,
            &scene(wall),
            true,
        );
        save_png(
            &format!(
                "thin-wall-{}.png",
                wall.map_or("none".into(), |t| format!("{t}"))
            ),
            &image,
        );
        at(&camera, &image, probe)[0]
    };
    let (open, one_voxel, one_cell) = (measure(None), measure(Some(0.25)), measure(Some(0.5)));
    println!(
        "receiver 1 m behind the wall: no wall {open:.4}, 0.25 m wall {one_voxel:.4} ({:.1} % leaks), 0.5 m wall {one_cell:.4} ({:.1} % leaks)",
        100.0 * one_voxel / open,
        100.0 * one_cell / open
    );
    assert!(open > 0.005, "the unobstructed control must be lit: {open}");
    assert!(
        one_voxel < open * 0.35,
        "a one-voxel wall should block most of the bounce: {one_voxel} vs {open}"
    );
    assert!(
        one_cell < open * 0.35,
        "a one-cell wall should block most of the bounce: {one_cell} vs {open}"
    );
}

/// The editor's mesh path and the game's cube path light a scene with bounce
/// identically.
#[test]
#[ignore = "requires a working GPU adapter"]
fn game_and_editor_paths_agree_with_bounce() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let environment = sunny();
    let mut boxes = red_wall_scene();
    boxes.push(boxed(LAMP, [4.0, 0.5, 2.0], [1.0, 1.0, 1.0]));
    let game = render(&ctx, &camera, &environment, DebugView::Shaded, &boxes, true);

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
    viewport.set_sky_occupancy(&ctx, Some(&occupancy(&boxes)));
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
        "game vs editor with bounce: mean |diff| {mean:.3}/255, {:.3}% > 12",
        big * 100.0
    );
    assert!(
        mean < 0.6 && big < 0.004,
        "paths diverge: mean {mean}, {big}"
    );
}

/// How narrow an opening the 0.5 m cache resolves: a wall with a gap of width
/// `g` between an emitter and a receiver, both in line with the gap. A cell is
/// occupied if any voxel in it is, so narrow gaps close (over-occlusion); this
/// records where they open. Gaps are placed so they are *not* grid-aligned.
#[test]
#[ignore = "requires a working GPU adapter"]
fn narrow_gaps_close_and_one_metre_gaps_open() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = Camera::looking_along(
        Vec3::new(3.0, 12.0, 4.0),
        Vec3::new(0.0, -1.0, -0.3),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    let probe = Vec3::new(-0.25, 0.0, 0.15);
    let measure = |gap: Option<f32>| {
        // Lamp long enough in z that the gap always faces it.
        let mut boxes = vec![ground(), boxed(LAMP, [-2.5, 0.5, 0.15], [1.0, 1.0, 3.0])];
        if let Some(g) = gap {
            // Wall segments at z in [-7, 0.15 - g/2] and [0.15 + g/2, 7]: the
            // gap is centred on z = 0.15, off the 0.5 m grid.
            let (lo, hi) = (0.15 - g / 2.0, 0.15 + g / 2.0);
            boxes.push(boxed(
                STONE,
                [-1.25, 2.0, (-7.0 + lo) / 2.0],
                [0.5, 4.0, lo + 7.0],
            ));
            boxes.push(boxed(
                STONE,
                [-1.25, 2.0, (hi + 7.0) / 2.0],
                [0.5, 4.0, 7.0 - hi],
            ));
        }
        let image = render(
            &ctx,
            &camera,
            &environment,
            DebugView::IndirectOnly,
            &boxes,
            true,
        );
        at(&camera, &image, probe)[0]
    };
    let open = measure(None);
    let results: Vec<(f32, f32)> = [0.25_f32, 0.5, 0.75, 1.0, 1.5]
        .into_iter()
        .map(|g| (g, measure(Some(g))))
        .collect();
    for (g, value) in &results {
        println!(
            "gap {g:.2} m: transmits {:.1} % of the unobstructed bounce",
            100.0 * value / open
        );
    }
    assert!(open > 0.005, "the unobstructed control must be lit: {open}");
    let transmitted = |g: f32| {
        results
            .iter()
            .find(|(w, _)| (*w - g).abs() < 1e-3)
            .unwrap()
            .1
            / open
    };
    assert!(
        transmitted(0.25) < 0.05,
        "a one-voxel gap is treated as closed: {}",
        transmitted(0.25)
    );
    assert!(
        transmitted(1.0) > 0.15,
        "a one-metre gap must let light through: {}",
        transmitted(1.0)
    );
    assert!(
        transmitted(1.5) > 0.15,
        "a wider gap must let light through: {}",
        transmitted(1.5)
    );
}

/// Light arriving at a low angle: a lamp several metres away on the same floor
/// subtends only a few degrees of elevation. It must still reach the floor, and
/// keep falling off with distance out to 9 m. Since far emission is lit
/// analytically (see `emitters.wgsl`) instead of by whichever rays happen to hit
/// the lamp, there is no longer a range where a small lamp falls between rays.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_distant_lamp_still_lights_the_floor_and_falls_off() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    let boxes = vec![ground(), boxed(LAMP, [-3.0, 0.5, 0.0], [1.0, 1.0, 1.0])];
    let image = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &boxes,
        true,
    );
    save_png("distant-lamp-indirect-only.png", &image);
    // Distances from the lamp's near face (x = -2.5).
    let at_distance = |d: f32| at(&camera, &image, Vec3::new(-2.5 + d, 0.0, 0.0))[0];
    let values: Vec<(f32, f32)> = [1.5_f32, 3.0, 4.5, 6.0, 9.0]
        .into_iter()
        .map(|d| (d, at_distance(d)))
        .collect();
    for (d, v) in &values {
        println!("floor {d:.1} m from the lamp: {v:.4}");
    }
    assert!(
        values[2].1 > 0.0005,
        "4.5 m away should still be lit: {values:?}"
    );
    assert!(
        values[4].1 > 0.0001,
        "9 m away should still be lit now that far emission is analytic: {values:?}"
    );
    for pair in values.windows(2) {
        assert!(
            pair[0].1 >= pair[1].1,
            "light must not increase with distance: {values:?}"
        );
    }
    // The floor sees the lamp at grazing incidence, so on top of the 1/d^2 of
    // distance the receiver cosine falls with it: steeper than 9x from 1.5 m to
    // 4.5 m, but not a cliff.
    let ratio = values[0].1 / values[2].1.max(1e-6);
    assert!(
        (2.0..200.0).contains(&ratio),
        "1.5 m vs 4.5 m should fall off steeply (1/d^2 alone would be 9x): {ratio}"
    );
}

/// A small lamp far from a wall used to light it by luck: a cell's 66 rays mostly
/// miss a 1 m target 14 m away, so a few cells got a full-strength hit and their
/// neighbours nothing (random bright spots). Lit analytically, the wall's
/// brightness varies smoothly. This measures the variation across a grid of
/// wall points; a speckled wall has a coefficient of variation above 1.
#[test]
#[ignore = "requires a working GPU adapter"]
fn a_small_distant_lamp_lights_a_wall_smoothly() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    // Looking at the wall's +x face; the lamp is behind the camera, out of view.
    let camera = Camera::looking_along(
        Vec3::new(8.0, 2.0, 0.0),
        Vec3::new(-1.0, 0.0, 0.0),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    let boxes = vec![
        ground(),
        boxed(WHITE, [0.0, 2.0, 0.0], [0.5, 4.0, 20.0]),
        boxed(LAMP, [14.0, 1.5, 0.0], [1.0, 1.0, 1.0]),
    ];
    let image = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &boxes,
        true,
    );
    save_png("distant-lamp-wall-indirect-only.png", &image);
    let mut samples = Vec::new();
    for zi in -4..=4 {
        for yi in [1.0_f32, 2.0, 3.0] {
            let p = Vec3::new(0.26, yi, zi as f32);
            samples.push(at(&camera, &image, p)[0]);
        }
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let variance =
        samples.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / samples.len() as f32;
    let cv = variance.sqrt() / mean.max(1e-9);
    println!("wall 14 m from a 1 m lamp: mean {mean:.5}, coefficient of variation {cv:.3}");
    assert!(mean > 0.0005, "the wall must be lit by the lamp: {mean}");
    assert!(
        cv < 0.35,
        "a distant lamp must light the wall smoothly, not as random spots: cv {cv}"
    );
}

/// More emissive bins than the emitter list holds: the bounce pass must fall
/// back to sampling emission with rays, not silently drop lamps.
#[test]
#[ignore = "requires a working GPU adapter"]
fn too_many_emitters_fall_back_to_rays_instead_of_going_dark() {
    use spall_render::sky_visibility::EMITTER_CAPACITY;
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let camera = top_camera();
    let mut environment = EnvironmentPreset::Night.environment();
    environment.sun_intensity = 0.0;
    environment.sky = [0.0; 3];
    environment.ground = [0.0; 3];
    // One lamp per 2 m bin, more than the list can hold, all well away from the
    // probe so the probe's light is the sum of many distant lamps.
    let per_axis = ((EMITTER_CAPACITY as f32).sqrt().ceil() as i32) + 2;
    let mut boxes = vec![ground()];
    for ix in 0..per_axis {
        for iz in 0..per_axis {
            let x = -28.0 + 2.0 * ix as f32 + 1.0;
            let z = -28.0 + 2.0 * iz as f32 + 1.0;
            boxes.push(boxed(GLOW, [x, 0.5, z], [0.5, 1.0, 0.5]));
        }
    }
    assert!(
        (per_axis * per_axis) as u32 > EMITTER_CAPACITY,
        "the scene must overflow the emitter list"
    );
    let image = render(
        &ctx,
        &camera,
        &environment,
        DebugView::IndirectOnly,
        &boxes,
        true,
    );
    save_png("emitter-overflow-indirect-only.png", &image);
    let lit = at(&camera, &image, Vec3::new(0.0, 0.0, 0.0));
    println!("floor amid {} lamps: {lit:?}", per_axis * per_axis);
    assert!(
        lit[0] > 0.001,
        "an overflowing emitter list must fall back to rays, not go dark: {lit:?}"
    );
}
