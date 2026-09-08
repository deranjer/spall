//! End-to-end offscreen capture against a real GPU.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render -- --ignored` on a host with a working adapter.

use std::fs;

use glam::{Mat4, Vec3};
use spall_mesh::MeshStrategy;
use spall_mesh::fixtures::{acceptance_shapes, mesh_shape};
use spall_render::{CaptureOptions, DebugView, RenderContext, Scene, SceneItem, capture_scene};

#[test]
#[ignore = "requires a working GPU adapter"]
fn every_acceptance_shape_captures_three_non_empty_images() {
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
            3,
            "{}: shaded + normals + depth",
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
    }

    let _ = fs::remove_dir_all(&out_root);
}
