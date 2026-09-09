//! End-to-end offscreen capture against a real GPU.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render -- --ignored` on a host with a working adapter.

use std::fs;

use glam::{Mat4, Vec3};
use spall_mesh::fixtures::{acceptance_shapes, mesh_shape};
use spall_mesh::{Mesh, MeshStrategy, Vertex};
use spall_render::{
    Camera, CaptureOptions, DebugView, RenderContext, Scene, SceneItem, capture_scene,
    colored_rooms,
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

#[test]
#[ignore = "requires a working GPU adapter"]
fn colored_room_produces_real_indirect_only_pixels_and_separate_timings() {
    let ctx = RenderContext::headless().expect("GPU adapter");
    let fixture = colored_rooms(16.0 / 9.0).remove(0);
    assert!(fixture.metrics.closed_probe_luminance < fixture.metrics.open_probe_luminance);
    assert!(fixture.metrics.thin_wall_leakage_ratio <= 0.05);
    let dir = std::env::temp_dir().join(format!("spall-t13-colored-room-{}", std::process::id()));
    let report = capture_scene(
        &ctx,
        &fixture.scene,
        &dir,
        &CaptureOptions {
            width: 640,
            height: 360,
            views: vec![DebugView::IndirectOnly],
            ..Default::default()
        },
    )
    .expect("indirect capture");
    assert!(report.indirect_enabled);
    assert_eq!(report.indirect_cells, 128usize.pow(3));
    if ctx.supports_gpu_timestamps() {
        let passes = report.timing.gpu_passes.expect("timestamp timings");
        assert!(passes.indirect_trace_millis > 0.0);
        assert!(passes.indirect_denoise_millis > 0.0);
    } else {
        assert!(report.timing.gpu_passes.is_none());
    }
    let image = image::open(&report.images[0].path).unwrap().to_rgb8();
    let indirect_pixels = image
        .pixels()
        .filter(|pixel| pixel.0.iter().copied().max().unwrap_or(0) > 18)
        .count();
    assert!(
        indirect_pixels > image.pixels().len() / 20,
        "indirect-only image is empty"
    );
    let _ = fs::remove_dir_all(&dir);
}
