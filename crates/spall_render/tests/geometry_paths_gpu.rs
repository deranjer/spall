//! Geometry-submission paths compared on one terrain: instanced surface cubes
//! (what the interactive game draws) against a greedy mesh of the same voxels
//! (what `docs/architecture.md` specifies and the editor draws). Records
//! triangle counts, build cost, memory and per-frame GPU wall time so the
//! choice rests on measurements. See `docs/reports/ENG-96.md`.
//!
//! Ignored by default; run with
//! `cargo test --release -p spall_render --test geometry_paths_gpu -- --ignored --nocapture`.

use std::time::Instant;

use glam::Vec3;
use spall_core::{BRICK_EDGE, BrickCoord, CellSizeCode, MaterialId, Revision, VolumeId};
use spall_mesh::MeshStrategy;
use spall_mesh::fixtures::mesh_shape;
use spall_render::pipeline::COLOR_FORMAT;
use spall_render::{
    Camera, CubeInstance, DebugView, Environment, EnvironmentPreset, GameRenderer, Material,
    MeshChunk, OffscreenTarget, RenderContext, ViewportFrame, ViewportRenderer, to_gpu,
};
use spall_voxel::{Brick, Volume};

const CELL: f32 = 0.25;
const GRID: i64 = 242;

/// Rolling terrain: column height in cells (2..=10), plus 3 m "tree" columns.
fn column_height(x: i64, z: i64) -> i64 {
    let (fx, fz) = (x as f32 * CELL, z as f32 * CELL);
    let base = 6.0 + ((0.35 * (fx * 0.35).sin() + 0.3 * (fz * 0.27).cos()) / CELL);
    let mut h = base.round() as i64;
    // A tree trunk every ~9 m, 12 cells (3 m) tall, 2 cells wide.
    if x.rem_euclid(36) < 2 && z.rem_euclid(36) < 2 {
        h += 12;
    }
    h.clamp(2, 28)
}

fn build_volume() -> Volume {
    let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
    let edge = i64::from(BRICK_EDGE);
    let bricks = (GRID + edge - 1) / edge;
    for bz in 0..bricks {
        for bx in 0..bricks {
            let mut cells = vec![MaterialId::AIR; (edge * edge * edge) as usize];
            for lz in 0..edge {
                for lx in 0..edge {
                    let (x, z) = (bx * edge + lx, bz * edge + lz);
                    if x >= GRID || z >= GRID {
                        continue;
                    }
                    let h = column_height(x, z).min(edge);
                    for ly in 0..h {
                        cells[(lx + edge * (ly + edge * lz)) as usize] = MaterialId(1);
                    }
                }
            }
            volume
                .insert_brick(
                    BrickCoord { x: bx, y: 0, z: bz },
                    Brick::restored(&cells, Revision::ZERO, false),
                )
                .expect("insert brick");
        }
    }
    volume
}

/// Surface cells only: a solid cell with at least one air face neighbour.
fn surface_cubes() -> Vec<CubeInstance> {
    let solid = |x: i64, y: i64, z: i64| {
        (0..GRID).contains(&x) && (0..GRID).contains(&z) && y >= 0 && y < column_height(x, z)
    };
    let mut cubes = Vec::new();
    for z in 0..GRID {
        for x in 0..GRID {
            for y in 0..column_height(x, z) {
                let buried = solid(x + 1, y, z)
                    && solid(x - 1, y, z)
                    && solid(x, y + 1, z)
                    && (y == 0 || solid(x, y - 1, z))
                    && solid(x, y, z + 1)
                    && solid(x, y, z - 1);
                if !buried {
                    cubes.push(CubeInstance::new(
                        [
                            (x as f32 + 0.5) * CELL - 30.0,
                            (y as f32 + 0.5) * CELL,
                            (z as f32 + 0.5) * CELL - 30.0,
                        ],
                        1,
                        [CELL; 3],
                        CubeInstance::IDENTITY_ROTATION,
                    ));
                }
            }
        }
    }
    cubes
}

fn frame_ms(ctx: &RenderContext, frames: usize, mut render: impl FnMut()) -> (f64, f64) {
    let mut samples = Vec::new();
    for frame in 0..frames + 20 {
        let start = Instant::now();
        render();
        ctx.wait().expect("device idle");
        if frame >= 20 {
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    samples.sort_by(f64::total_cmp);
    (
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
    )
}

#[test]
#[ignore = "requires a working GPU adapter; records measurements"]
fn instanced_cubes_versus_greedy_mesh() {
    let ctx = RenderContext::headless().expect("a GPU adapter for the --ignored test");
    let environment: Environment = EnvironmentPreset::Daylight.environment();
    let materials = vec![
        Material::default(),
        Material::new([0.2, 0.45, 0.15], 0.85, 0.0),
    ];
    let (width, height) = (1920, 1080);
    let camera = Camera::looking_along(
        Vec3::new(0.0, 5.0, 6.0),
        Vec3::new(0.2, -0.2, -1.0),
        75_f32.to_radians(),
        width as f32 / height as f32,
    );

    let volume = build_volume();
    // Instanced path inputs.
    let t = Instant::now();
    let cubes = surface_cubes();
    let cube_build_ms = t.elapsed().as_secs_f64() * 1000.0;
    // Mesh path inputs (built in volume-local metres, shifted like the cubes).
    let t = Instant::now();
    let meshed = mesh_shape(&volume, MeshStrategy::Greedy);
    let mesh_build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (mut vertices, indices) = to_gpu(
        &meshed.mesh,
        glam::Mat4::from_translation(Vec3::new(-30.0, 0.0, -30.0)),
    );
    for v in &mut vertices {
        v.material = 1;
    }
    let mesh_triangles = indices.len() / 3;
    let cube_triangles = cubes.len() * 12;
    let mesh_bytes =
        vertices.len() * std::mem::size_of::<spall_render::GpuVertex>() + indices.len() * 4;
    let cube_bytes = cubes.len() * std::mem::size_of::<CubeInstance>();

    // Game (instanced) path.
    let target = OffscreenTarget::new(&ctx.device, width, height);
    let mut game = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        COLOR_FORMAT,
        &materials,
        (width, height),
        None,
    );
    game.set_terrain(&ctx.device, &ctx.queue, &cubes);
    let (cube_p50, cube_p95) = frame_ms(&ctx, 100, || {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        game.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &camera,
            &environment,
            DebugView::Shaded,
        );
        ctx.queue.submit([encoder.finish()]);
    });

    // Mesh path.
    let mut viewport = ViewportRenderer::new(&ctx, &materials);
    viewport.resize(&ctx, width, height);
    viewport
        .set_meshes(
            &ctx,
            &[MeshChunk {
                vertices: &vertices,
                indices: &indices,
            }],
        )
        .expect("upload mesh");
    let (mesh_p50, mesh_p95) = frame_ms(&ctx, 100, || {
        viewport.render(
            &ctx,
            &ViewportFrame {
                camera,
                environment,
            },
        );
    });

    println!(
        "surface cubes: {} ({} tris, {:.1} MiB), built in {cube_build_ms:.0} ms",
        cubes.len(),
        cube_triangles,
        cube_bytes as f64 / 1048576.0
    );
    println!(
        "greedy mesh:   {} quads, {mesh_triangles} tris, {:.1} MiB, built in {mesh_build_ms:.0} ms",
        meshed.stats.quad_count,
        mesh_bytes as f64 / 1048576.0
    );
    println!(
        "frame wall time (GPU + submit, 1080p): cubes p50 {cube_p50:.2} / p95 {cube_p95:.2} ms; mesh p50 {mesh_p50:.2} / p95 {mesh_p95:.2} ms"
    );

    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.local/runs/eng-96-perf");
    std::fs::create_dir_all(&dir).expect("evidence directory");
    let json = format!(
        "{{\n  \"adapter\": \"{}\",\n  \"backend\": \"{:?}\",\n  \"build\": \"{}\",\n  \"resolution\": [{width}, {height}],\n  \"solid_bricks\": {},\n  \"instanced_cubes\": {{\"instances\": {}, \"triangles\": {cube_triangles}, \"bytes\": {cube_bytes}, \"build_ms\": {cube_build_ms:.1}, \"frame_ms_p50\": {cube_p50:.3}, \"frame_ms_p95\": {cube_p95:.3}}},\n  \"greedy_mesh\": {{\"quads\": {}, \"triangles\": {mesh_triangles}, \"bytes\": {mesh_bytes}, \"build_ms\": {mesh_build_ms:.1}, \"frame_ms_p50\": {mesh_p50:.3}, \"frame_ms_p95\": {mesh_p95:.3}}}\n}}\n",
        ctx.adapter_name(),
        ctx.backend(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        volume.resident_brick_count(),
        cubes.len(),
        meshed.stats.quad_count,
    );
    std::fs::write(dir.join("geometry-paths.json"), json).expect("write");
}
