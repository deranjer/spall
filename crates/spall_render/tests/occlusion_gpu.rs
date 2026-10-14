//! Occlusion culling of terrain chunks, on a real GPU (`--ignored`).

use glam::Vec3;
use spall_core::BrickCoord;
use spall_render::instances::unit_cube;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, DebugView, EnvironmentPreset, GameRenderer, GpuVertex, Material, OffscreenTarget,
    RenderContext,
};

const SIZE: (u32, u32) = (320, 200);

/// An axis-aligned box as terrain mesh vertices in world metres.
fn block(min: Vec3, max: Vec3) -> (Vec<GpuVertex>, Vec<u32>) {
    let (cube, cube_indices) = unit_cube();
    let vertices = cube
        .iter()
        .map(|v| GpuVertex {
            position: (min + Vec3::from_array(v.position) * (max - min)).to_array(),
            normal: v.normal,
            local_uv: v.local_uv,
            ao: 1.0,
            material: 0,
        })
        .collect();
    (
        vertices,
        cube_indices.iter().map(|i| u32::from(*i)).collect(),
    )
}

fn frame(
    ctx: &RenderContext,
    renderer: &mut GameRenderer,
    target: &OffscreenTarget,
    camera: &Camera,
) {
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    renderer.render(
        &ctx.device,
        &ctx.queue,
        &mut encoder,
        target.color_view(),
        camera,
        &EnvironmentPreset::Daylight.environment(),
        DebugView::Shaded,
    );
    ctx.queue.submit([encoder.finish()]);
    renderer.finish_timing();
}

#[test]
#[ignore = "requires a real GPU adapter; run with --ignored"]
fn a_chunk_behind_a_wall_stops_being_drawn_and_returns_when_the_wall_goes() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let materials = vec![Material::new([0.6, 0.6, 0.6], 0.9, 0.0)];
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials,
        SIZE,
        None,
    );
    let camera = Camera::looking_along(
        Vec3::new(4.0, 4.0, 30.0),
        Vec3::new(0.0, 0.0, -1.0),
        70_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let wall = BrickCoord { x: 0, y: 0, z: 2 };
    let hidden = BrickCoord { x: 0, y: 0, z: 0 };
    let beside = BrickCoord { x: 1, y: 0, z: 2 };
    let (wv, wi) = block(Vec3::new(-6.0, -6.0, 20.0), Vec3::new(14.0, 14.0, 21.0));
    let (hv, hi) = block(Vec3::new(2.0, 2.0, 2.0), Vec3::new(6.0, 6.0, 6.0));
    // Off to the side of the wall, in view.
    let (bv, bi) = block(Vec3::new(10.0, 2.0, 18.0), Vec3::new(13.0, 5.0, 22.0));
    renderer
        .update_terrain_meshes(
            &ctx.device,
            &[(wall, wv, wi), (hidden, hv, hi), (beside, bv, bi)],
            &[],
        )
        .unwrap();
    for _ in 0..20 {
        frame(&ctx, &mut renderer, &target, &camera);
    }
    assert_eq!(
        renderer.terrain_chunks_drawn(),
        2,
        "the wall and the chunk beside it are drawn, the one behind the wall is not"
    );
    // Remove the wall: the chunk is drawn again within a few frames.
    renderer
        .update_terrain_meshes(&ctx.device, &[], &[wall])
        .unwrap();
    for _ in 0..20 {
        frame(&ctx, &mut renderer, &target, &camera);
    }
    assert_eq!(
        renderer.terrain_chunks_drawn(),
        2,
        "the chunk that was behind the wall shows again, beside the other"
    );
}
