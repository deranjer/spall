//! `ViewportRenderer` against a real GPU.
//!
//! Ignored by default so CPU-only CI stays green; run with
//! `cargo test -p spall_render --test viewport_gpu -- --ignored` on a host
//! with a working adapter.

use glam::Vec3;
use spall_render::{
    Camera, Environment, GpuVertex, Material, MeshChunk, RenderContext, ViewportFrame,
    ViewportRenderer,
};

fn quad(material: u32) -> (Vec<GpuVertex>, Vec<u32>) {
    // A 2 m square in the z = 0 plane facing +Z (counter-clockwise from +Z).
    let corner = |x: f32, y: f32| GpuVertex {
        position: [x, y, 0.0],
        normal: [0.0, 0.0, 1.0],
        local_uv: [0.5, 0.5],
        ao: 1.0,
        material,
    };
    (
        vec![
            corner(-1.0, -1.0),
            corner(1.0, -1.0),
            corner(1.0, 1.0),
            corner(-1.0, 1.0),
        ],
        vec![0, 1, 2, 0, 2, 3],
    )
}

fn pixel(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * width + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2]]
}

fn render_and_read(
    ctx: &RenderContext,
    viewport: &ViewportRenderer,
    frame: &ViewportFrame,
) -> Vec<u8> {
    viewport.render(ctx, frame);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    viewport.target().copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    viewport.target().read_rgba(ctx).expect("readback")
}

#[test]
#[ignore = "requires a working GPU adapter"]
fn the_viewport_draws_geometry_over_the_clear_colour_and_follows_resizes() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let mut viewport = ViewportRenderer::new(&ctx, &[]);
    assert_eq!(viewport.target_generation(), 0);
    assert!(viewport.resize(&ctx, 160, 120));
    assert!(
        !viewport.resize(&ctx, 160, 120),
        "same size keeps the texture"
    );
    assert_eq!(viewport.target_generation(), 1);
    assert_eq!(viewport.size(), (160, 120));

    let mut red = Material::new([0.9, 0.05, 0.05], 0.9, 0.0);
    red.emissive = 0.0;
    viewport.set_materials(&ctx, &[Material::default(), red]);
    let (vertices, indices) = quad(1);
    viewport
        .set_meshes(
            &ctx,
            &[MeshChunk {
                vertices: &vertices,
                indices: &indices,
            }],
        )
        .expect("upload");
    assert_eq!(viewport.triangle_count(), 2);

    let camera = Camera {
        position: Vec3::new(0.0, 0.0, 6.0),
        ..Camera::default()
    };
    let frame = ViewportFrame {
        camera,
        environment: Environment {
            background: [10, 20, 200],
            ..Environment::default()
        },
    };
    let rgba = render_and_read(&ctx, &viewport, &frame);
    let centre = pixel(&rgba, 160, 80, 60);
    let corner = pixel(&rgba, 160, 2, 2);
    assert!(
        centre[0] > centre[2] + 40,
        "the quad should read red at the centre, got {centre:?}"
    );
    assert!(
        corner[2] > corner[0] + 40,
        "the clear colour should read blue at the corner, got {corner:?}"
    );

    // No geometry: the whole target is the clear colour.
    viewport.set_meshes(&ctx, &[]).expect("clear geometry");
    let rgba = render_and_read(&ctx, &viewport, &frame);
    let centre = pixel(&rgba, 160, 80, 60);
    assert!(centre[2] > centre[0] + 40, "empty scene, got {centre:?}");
}
