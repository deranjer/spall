//! End-to-end offscreen capture against a real GPU.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render -- --ignored` on a host with a working adapter.

use std::fs;
use std::process::Command;

use glam::{Mat4, Vec3};
use spall_mesh::fixtures::{acceptance_shapes, mesh_shape};
use spall_mesh::{Mesh, MeshStrategy, Vertex};
use spall_render::{
    Camera, CaptureOptions, DebugView, RenderContext, Scene, SceneItem, capture_scene,
    colored_rooms, emitter_occlusion_scenes,
};

#[test]
#[ignore = "requires a working GPU adapter"]
fn every_acceptance_shape_captures_all_t12_debug_images() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored capture test");

    let out_root = std::env::temp_dir().join(format!("spall-capture-gpu-{}", std::process::id()));
    let opts = CaptureOptions {
        width: 320,
        height: 240,
        ..Default::default()
    };

    for shape in acceptance_shapes() {
        let vm = mesh_shape(&shape.volume, MeshStrategy::Greedy);
        assert!(!vm.mesh.is_empty(), "{}: mesh", shape.name);

        let mut scene = Scene::new(spall_render::Camera {
            aspect: opts.width as f32 / opts.height as f32,
            ..Default::default()
        });
        scene = scene.with_item(SceneItem::new(
            shape.name,
            vm.mesh,
            Mat4::from_rotation_y(shape.yaw),
        ));
        scene.frame_all(Vec3::new(0.8, 0.55, 1.0));

        let dir = out_root.join(shape.name);
        let report = capture_scene(&ctx, &scene, &dir, &opts).expect("capture");

        // CPU/GPU timings are reported separately and never conflated.
        assert!(
            report.timing.cpu_total_millis > 0.0,
            "{}: cpu capture time recorded",
            shape.name
        );
        if ctx.supports_gpu_timestamps() {
            let gpu = report
                .timing
                .gpu_render_millis
                .expect("timestamp-capable adapter reports a GPU pass time");
            assert!(gpu >= 0.0, "{}: non-negative GPU pass time", shape.name);
        } else {
            assert!(
                report.timing.gpu_render_millis.is_none(),
                "{}: GPU timing must be unavailable without timestamp support",
                shape.name
            );
        }

        assert_eq!(
            report.images.len(),
            6,
            "{}: shaded + albedo + normals + depth + cascades + roughness",
            shape.name
        );
        assert_eq!(
            report.items_drawn, 1,
            "{}: item survived culling",
            shape.name
        );
        assert!(report.triangles > 0, "{}: rasterised triangles", shape.name);

        for image in &report.images {
            let meta = fs::metadata(&image.path).expect("png written");
            assert!(
                meta.len() > 128,
                "{}: {:?} too small",
                shape.name,
                image.path
            );
        }

        // The shaded image must contain lit surface pixels, not just the clear
        // colour.
        let shaded = report
            .images
            .iter()
            .find(|i| i.view == DebugView::Shaded)
            .unwrap();
        let decoded = image::open(&shaded.path)
            .expect("decode shaded png")
            .to_rgb8();
        let bright = decoded
            .pixels()
            .filter(|p| p.0.iter().any(|&c| c > 90))
            .count();
        assert!(
            bright > decoded.pixels().len() / 50,
            "{}: shaded image looks empty ({bright} lit px)",
            shape.name
        );

        // Stone's linear 0.42 reflectance should land around sRGB 173 in the
        // albedo view. Values near 107 indicate a missing transfer; values
        // above 205 indicate it was applied twice.
        if shape.name == "cube" {
            let albedo = report
                .images
                .iter()
                .find(|image| image.view == DebugView::Albedo)
                .unwrap();
            let pixels = image::open(&albedo.path).unwrap().to_rgb8();
            let correctly_encoded = pixels
                .pixels()
                .filter(|pixel| {
                    let [r, g, b] = pixel.0;
                    (150..=195).contains(&r) && (150..=200).contains(&g) && (150..=205).contains(&b)
                })
                .count();
            assert!(
                correctly_encoded > pixels.pixels().len() / 50,
                "linear albedo was not encoded to sRGB exactly once"
            );
        }
    }

    let _ = fs::remove_dir_all(&out_root);
}

#[test]
#[ignore = "requires a working GPU adapter"]
fn a_transformed_body_casts_a_shadow_onto_static_geometry() {
    let ctx = RenderContext::headless().expect("GPU adapter");
    let shapes = acceptance_shapes();
    let cube = shapes.iter().find(|shape| shape.name == "cube").unwrap();
    let mesh = mesh_shape(&cube.volume, MeshStrategy::Greedy).mesh;
    let mut scene = Scene::new(Camera {
        aspect: 1.0,
        ..Default::default()
    })
    .with_item(SceneItem::new(
        "ground",
        mesh.clone(),
        Mat4::from_scale(Vec3::new(4.0, 0.2, 4.0)),
    ))
    .with_item(SceneItem::new(
        "moving-body",
        mesh,
        Mat4::from_translation(Vec3::new(0.4, 1.4, 0.2)) * Mat4::from_rotation_y(0.55),
    ));
    scene.frame_all(Vec3::new(1.1, 0.8, 1.2));
    let dir = std::env::temp_dir().join(format!("spall-t12-shadow-{}", std::process::id()));
    let report = capture_scene(
        &ctx,
        &scene,
        &dir,
        &CaptureOptions {
            width: 512,
            height: 512,
            views: vec![DebugView::Shaded, DebugView::ShadowCascades],
            ..Default::default()
        },
    )
    .expect("capture");
    assert_eq!(report.items_drawn, 2);
    assert!(report.timing.gpu_passes.is_some());
    let shadow = report
        .images
        .iter()
        .find(|image| image.view == DebugView::ShadowCascades)
        .unwrap();
    let pixels = image::open(&shadow.path).unwrap().to_rgb8();
    let background = pixels.get_pixel(0, 0).0;
    let dark = pixels
        .pixels()
        .filter(|pixel| {
            let rgb = pixel.0;
            let differs_from_background = rgb
                .iter()
                .zip(background)
                .any(|(&channel, bg)| channel.abs_diff(bg) > 20);
            let min = rgb.iter().copied().min().unwrap_or(0);
            let max = rgb.iter().copied().max().unwrap_or(0);
            differs_from_background && max - min > 20 && max < 150
        })
        .count();
    assert!(dark > 32, "expected an observable shadowed region");
    let _ = fs::remove_dir_all(&dir);
}

/// A plane at constant view-space Z must show constant depth across off-axis
/// pixels (ENG-45). The old shader used `length(world_pos - camera_pos)`, which
/// grows toward the edges of a camera-facing plane.
#[test]
#[ignore = "requires a working GPU adapter"]
fn depth_debug_view_is_flat_across_a_camera_facing_plane() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored depth test");

    let (width, height) = (320u32, 320u32);
    // A big quad in the z = 0 plane, facing +Z (toward the camera). CCW from
    // the front so back-face culling keeps it.
    let n = [0.0, 0.0, 1.0];
    let corner = |x: f32, y: f32| Vertex {
        position: [x, y, 0.0],
        normal: n,
        material: 1,
        ao: 1.0,
        local_uv: [0.0, 0.0],
    };
    let mesh = Mesh {
        vertices: vec![
            corner(-3.0, -3.0),
            corner(3.0, -3.0),
            corner(3.0, 3.0),
            corner(-3.0, 3.0),
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
    };

    // Camera dead ahead of the plane: plane sits at linear eye depth 5.
    let camera = Camera {
        position: Vec3::new(0.0, 0.0, 5.0),
        yaw: 0.0,
        pitch: 0.0,
        aspect: width as f32 / height as f32,
        z_near: 1.0,
        z_far: 12.0,
        ..Camera::default()
    };
    let scene = Scene::new(camera).with_item(SceneItem::new("plane", mesh, Mat4::IDENTITY));

    let dir = std::env::temp_dir().join(format!("spall-depth-plane-{}", std::process::id()));
    let opts = CaptureOptions {
        width,
        height,
        views: vec![DebugView::Depth],
        ..Default::default()
    };
    let report = capture_scene(&ctx, &scene, &dir, &opts).expect("capture");
    let depth = report
        .images
        .iter()
        .find(|i| i.view == DebugView::Depth)
        .expect("depth image");
    let img = image::open(&depth.path)
        .expect("decode depth png")
        .to_rgb8();

    // Scan the centre row; the plane covers it edge to edge. Collect the grey
    // (drawn) pixels and check they barely vary.
    let y = height / 2;
    let mut greys: Vec<i32> = Vec::new();
    for x in 0..width {
        let p = img.get_pixel(x, y).0;
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        if (r - g).abs() <= 3 && (g - b).abs() <= 3 {
            greys.push(g);
        }
    }
    assert!(
        greys.len() > (width as usize) * 3 / 4,
        "expected the plane to fill the centre row, got {} px",
        greys.len()
    );
    let (lo, hi) = (*greys.iter().min().unwrap(), *greys.iter().max().unwrap());
    assert!(
        hi - lo <= 6,
        "depth varies across a camera-facing plane: {lo}..{hi} (radial-distance bug)"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Mean sRGB luminance over the fractional image-x band `[band.0, band.1)`,
/// full height. Geometry is identical across the compared captures, so clear
/// pixels cancel and the band mean tracks receiver-wall brightness.
fn band_mean_luminance(path: &std::path::Path, band: (f32, f32)) -> f64 {
    let image = image::open(path).unwrap().to_rgb8();
    let (w, h) = (image.width(), image.height());
    let x0 = (band.0 * w as f32).round() as u32;
    let x1 = ((band.1 * w as f32).round() as u32).min(w);
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for y in 0..h {
        for x in x0..x1 {
            let [r, g, b] = image.get_pixel(x, y).0;
            sum += 0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b);
            count += 1;
        }
    }
    sum / count.max(1) as f64
}

/// Count receiver-band pixels whose sRGB luminance in `lit` exceeds the same
/// pixel in `reference` by more than `delta` (8-bit units). With `reference`
/// captured from an identical scene minus the emissive term, this is a direct
/// pixel count of transported emitter light on the non-emissive receiver.
fn transported_pixels(
    lit: &std::path::Path,
    reference: &std::path::Path,
    band: (f32, f32),
    delta: f64,
) -> (u64, u64) {
    let a = image::open(lit).unwrap().to_rgb8();
    let b = image::open(reference).unwrap().to_rgb8();
    assert_eq!(a.dimensions(), b.dimensions());
    let (w, h) = a.dimensions();
    let x0 = (band.0 * w as f32).round() as u32;
    let x1 = ((band.1 * w as f32).round() as u32).min(w);
    let luma =
        |p: [u8; 3]| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]);
    let mut brighter = 0u64;
    let mut total = 0u64;
    for y in 0..h {
        for x in x0..x1 {
            total += 1;
            if luma(a.get_pixel(x, y).0) - luma(b.get_pixel(x, y).0) > delta {
                brighter += 1;
            }
        }
    }
    (brighter, total)
}

#[test]
#[ignore = "requires a working GPU adapter"]
fn colored_room_produces_real_indirect_only_pixels_and_separate_timings() {
    let ctx = RenderContext::headless().expect("GPU adapter");
    let mut fixtures = colored_rooms(16.0 / 9.0);
    let closed = fixtures.pop().unwrap();
    let open = fixtures.pop().unwrap();
    assert_eq!(open.name, "colored_room_open");
    assert_eq!(closed.name, "colored_room_closed");

    // CPU probe copy direction, restated so the rendered check below can be read
    // against the frozen decision-document numbers.
    assert!(open.metrics.closed_probe_luminance < open.metrics.open_probe_luminance);
    assert!(open.metrics.thin_wall_leakage_ratio <= 0.05);

    let opts = CaptureOptions {
        width: 640,
        height: 360,
        views: vec![DebugView::IndirectOnly],
        ..Default::default()
    };
    let root = std::env::temp_dir().join(format!("spall-t13-colored-room-{}", std::process::id()));
    let open_report =
        capture_scene(&ctx, &open.scene, &root.join("open"), &opts).expect("open indirect capture");
    let closed_report = capture_scene(&ctx, &closed.scene, &root.join("closed"), &opts)
        .expect("closed indirect capture");

    assert!(open_report.indirect_enabled);
    assert_eq!(open_report.indirect_cells, 128usize.pow(3));
    if ctx.supports_gpu_timestamps() {
        let passes = open_report.timing.gpu_passes.expect("timestamp timings");
        assert!(passes.indirect_trace_millis > 0.0);
        assert!(passes.indirect_denoise_millis > 0.0);
    } else {
        assert!(open_report.timing.gpu_passes.is_none());
    }

    let open_image = image::open(&open_report.images[0].path).unwrap().to_rgb8();
    let indirect_pixels = open_image
        .pixels()
        .filter(|pixel| pixel.0.iter().copied().max().unwrap_or(0) > 18)
        .count();
    assert!(
        indirect_pixels > open_image.pixels().len() / 20,
        "indirect-only image is empty"
    );

    // Rendered through GPU trace + GPU denoise + surface sample, the closed roof
    // darkens the room in the same direction the CPU probe copy reports.
    let lit_wall = (0.30f32, 0.70f32);
    let open_luminance = band_mean_luminance(&open_report.images[0].path, lit_wall);
    let closed_luminance = band_mean_luminance(&closed_report.images[0].path, lit_wall);
    assert!(
        closed_luminance < open_luminance,
        "closed room not darker in the render: open={open_luminance:.3} closed={closed_luminance:.3}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The T13 GPU-quality claim, validated on rendered pixels rather than the CPU
/// probe copy: a non-emissive receiver wall brightens where the represented
/// emitter can reach it, and one represented 0.5 m wall removes most of that
/// transported light. Both measurements come out of `DebugView::IndirectOnly`
/// captures, i.e. through the GPU trace, GPU denoise, and surface sample.
#[test]
#[ignore = "requires a working GPU adapter"]
fn indirect_only_render_shows_emitter_transport_and_wall_occlusion() {
    let ctx = RenderContext::headless().expect("GPU adapter");
    let scenes = emitter_occlusion_scenes(16.0 / 9.0);
    let band = (scenes.receiver_band[0], scenes.receiver_band[1]);
    let opts = CaptureOptions {
        width: 640,
        height: 360,
        views: vec![DebugView::IndirectOnly],
        ..Default::default()
    };
    let root = std::env::temp_dir().join(format!("spall-t13-occlusion-{}", std::process::id()));
    let capture = |name: &str, scene: &Scene| {
        capture_scene(&ctx, scene, &root.join(name), &opts)
            .expect("indirect capture")
            .images[0]
            .path
            .clone()
    };
    let lit = capture("lit", &scenes.lit);
    let occluded = capture("occluded", &scenes.occluded);
    let dark = capture("dark", &scenes.dark);

    let (lit_transport, band_pixels) = transported_pixels(&lit, &dark, band, 6.0);
    let (occluded_transport, _) = transported_pixels(&occluded, &dark, band, 6.0);

    // The emitter reaches the non-emissive receiver in the render at all.
    assert!(
        lit_transport > band_pixels / 12,
        "no rendered emitter transport on the receiver: {lit_transport}/{band_pixels} px"
    );
    // One represented 0.5 m wall between the panel and this band removes at
    // least half of the transported pixels (the CPU probe copy measures ~0).
    assert!(
        occluded_transport * 2 <= lit_transport,
        "represented 0.5 m wall did not occlude the render: lit={lit_transport} occluded={occluded_transport}"
    );
    // And it is darker on the mean, not just on a pixel count.
    let lit_mean = band_mean_luminance(&lit, band);
    let occluded_mean = band_mean_luminance(&occluded, band);
    let dark_mean = band_mean_luminance(&dark, band);
    assert!(
        occluded_mean < lit_mean && dark_mean <= occluded_mean + 1.0,
        "band means inconsistent: lit={lit_mean:.3} occluded={occluded_mean:.3} dark={dark_mean:.3}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// ENG-60 regression watch. On Windows the pinned wgpu 24 / naga 24 Vulkan path
/// crashes NVIDIA's driver (STATUS_ACCESS_VIOLATION) while compiling the T12
/// pipelines — see `docs/reports/ENG-60.md`. This spawns the `vulkan_shadow_probe`
/// example with the Vulkan backend forced and asserts it still crashes rather
/// than completing. When a `wgpu`/`naga` upgrade or a new driver fixes it, this
/// test starts failing: at that point re-run the full `capture_gpu` suite on
/// Vulkan and, if it passes, drop the Windows D3D12-only guard in
/// `RenderContext::headless` and delete this test.
///
/// Opt in with `SPALL_ENG60_RECHECK=1` because it shells out to `cargo run` and
/// deliberately provokes a native crash.
#[test]
#[ignore = "ENG-60: opt in with SPALL_ENG60_RECHECK=1; provokes a native driver crash"]
fn windows_vulkan_backend_still_crashes_compiling_t12_pipelines() {
    if std::env::var_os("SPALL_ENG60_RECHECK").is_none() {
        eprintln!("skipped: set SPALL_ENG60_RECHECK=1 to run the ENG-60 Vulkan recheck");
        return;
    }
    if !cfg!(target_os = "windows") {
        eprintln!("skipped: ENG-60 is a Windows/NVIDIA Vulkan driver fault");
        return;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "run",
            "--quiet",
            "-p",
            "spall_render",
            "--example",
            "vulkan_shadow_probe",
            "--",
            "case:pipelines",
        ])
        .env("SPALL_WGPU_BACKEND", "vulkan")
        .status()
        .expect("spawn vulkan_shadow_probe");

    assert!(
        !status.success(),
        "ENG-60: ScenePipeline::new() completed on the Vulkan backend — the driver \
         crash is gone. Re-validate the full capture_gpu suite on Vulkan and, if it \
         passes, restore Vulkan as an accepted Windows backend (see docs/reports/ENG-60.md)."
    );
}
