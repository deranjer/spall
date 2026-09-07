//! Offscreen renderer capture for the T05 visible-voxel baseline.
//!
//! Meshes each acceptance shape (`spall_mesh::fixtures`), renders it to a
//! shaded PNG plus normal and depth debug images with `spall_render`, writes a
//! `summary.json`, and exits. Exit codes: `0` pass, `1` failure, `2` bad
//! arguments, `3` no GPU/capture capability.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use glam::{Mat4, Vec3};
use serde::Serialize;
use spall_mesh::MeshStrategy;
use spall_mesh::fixtures::{AcceptanceShape, acceptance_shapes, mesh_shape};
use spall_render::{CaptureOptions, RenderContext, RenderError, Scene, SceneItem, capture_scene};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Strategy {
    Greedy,
    Culled,
}

impl From<Strategy> for MeshStrategy {
    fn from(value: Strategy) -> Self {
        match value {
            Strategy::Greedy => MeshStrategy::Greedy,
            Strategy::Culled => MeshStrategy::Culled,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "sandbox-capture",
    about = "Spall T05 offscreen renderer capture"
)]
struct Args {
    /// Output directory. One subdirectory per shape plus a summary.json.
    #[arg(long, default_value = ".local/runs/t05-capture")]
    out: PathBuf,
    #[arg(long, default_value_t = 1280, value_parser = clap::value_parser!(u32).range(16..=7680))]
    width: u32,
    #[arg(long, default_value_t = 720, value_parser = clap::value_parser!(u32).range(16..=4320))]
    height: u32,
    #[arg(long, value_enum, default_value_t = Strategy::Greedy)]
    strategy: Strategy,
    /// Render only this shape (by name). Omit to render every acceptance shape.
    #[arg(long)]
    only: Option<String>,
}

#[derive(Serialize)]
struct ShapeSummary {
    name: String,
    strategy: String,
    quad_count: usize,
    triangle_count: usize,
    vertex_count: usize,
    surface_area_m2: f64,
    exposed_unit_faces: u64,
    unresolved_halo_faces: u64,
    items_drawn: usize,
    triangles_rasterised: u64,
    vertex_bytes: u64,
    index_bytes: u64,
    gpu_millis: f64,
    images: Vec<String>,
}

#[derive(Serialize)]
struct Summary {
    version: u32,
    adapter: String,
    backend: String,
    width: u32,
    height: u32,
    strategy: String,
    shapes: Vec<ShapeSummary>,
}

fn view_dir() -> Vec3 {
    // A 3/4 view from front-right-above.
    Vec3::new(0.8, 0.55, 1.0)
}

fn build_scene(shape: &AcceptanceShape, strategy: MeshStrategy) -> (Scene, spall_mesh::MeshStats) {
    let vm = mesh_shape(&shape.volume, strategy);
    let stats = vm.stats;

    // Rotate about the mesh centre so the framed view stays centred. The one
    // rotated shape spins about an oblique axis so the tumble is obvious.
    let centre = mesh_centre(&vm.mesh);
    let rot = if shape.yaw != 0.0 {
        Mat4::from_axis_angle(Vec3::new(0.22, 1.0, 0.12).normalize(), shape.yaw)
    } else {
        Mat4::IDENTITY
    };
    let model = Mat4::from_translation(centre) * rot * Mat4::from_translation(-centre);

    let mut scene = Scene::new(spall_render::Camera {
        aspect: 16.0 / 9.0,
        fov_y: 55_f32.to_radians(),
        ..Default::default()
    });
    scene = scene.with_item(SceneItem::new(shape.name, vm.mesh, model));
    scene.frame_all(view_dir());
    (scene, stats)
}

fn mesh_centre(mesh: &spall_mesh::Mesh) -> Vec3 {
    if mesh.vertices.is_empty() {
        return Vec3::ZERO;
    }
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for v in &mesh.vertices {
        let p = Vec3::from_array(v.position);
        min = min.min(p);
        max = max.max(p);
    }
    (min + max) * 0.5
}

fn run(args: &Args) -> Result<Summary, RenderError> {
    let ctx = RenderContext::headless()?;
    let strategy: MeshStrategy = args.strategy.into();

    let mut shapes = acceptance_shapes();
    if let Some(only) = &args.only {
        shapes.retain(|s| s.name == only);
        if shapes.is_empty() {
            return Err(RenderError::Gpu(format!(
                "no acceptance shape named {only:?}"
            )));
        }
    }

    let opts = CaptureOptions {
        width: args.width,
        height: args.height,
        ..Default::default()
    };

    let mut summaries = Vec::new();
    let (mut adapter, mut backend) = (String::new(), String::new());
    for shape in &shapes {
        let (scene, stats) = build_scene(shape, strategy);
        let out_dir = args.out.join(shape.name);
        let report = capture_scene(&ctx, &scene, &out_dir, &opts)?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();

        summaries.push(ShapeSummary {
            name: shape.name.to_string(),
            strategy: format!("{strategy:?}"),
            quad_count: stats.quad_count,
            triangle_count: stats.triangle_count,
            vertex_count: stats.vertex_count,
            surface_area_m2: stats.surface_area_m2,
            exposed_unit_faces: stats.exposed_unit_faces,
            unresolved_halo_faces: stats.unresolved_halo_faces,
            items_drawn: report.items_drawn,
            triangles_rasterised: report.triangles,
            vertex_bytes: report.vertex_bytes,
            index_bytes: report.index_bytes,
            gpu_millis: report.gpu_millis,
            images: report
                .images
                .iter()
                .map(|i| i.path.display().to_string())
                .collect(),
        });
    }

    Ok(Summary {
        version: 1,
        adapter,
        backend,
        width: args.width,
        height: args.height,
        strategy: format!("{strategy:?}"),
        shapes: summaries,
    })
}

fn main() -> ExitCode {
    sandbox::init_tracing();
    let args = Args::parse();

    if let Err(error) = std::fs::create_dir_all(&args.out) {
        eprintln!(
            "sandbox-capture: cannot create {}: {error}",
            args.out.display()
        );
        return ExitCode::from(1);
    }

    match run(&args) {
        Ok(summary) => {
            let path = args.out.join("summary.json");
            match serde_json::to_vec_pretty(&summary) {
                Ok(body) => {
                    if let Err(error) = std::fs::write(&path, body) {
                        eprintln!("sandbox-capture: cannot write {}: {error}", path.display());
                        return ExitCode::from(1);
                    }
                }
                Err(error) => {
                    eprintln!("sandbox-capture: cannot serialise summary: {error}");
                    return ExitCode::from(1);
                }
            }
            println!(
                "sandbox-capture: {} shape(s) captured to {}",
                summary.shapes.len(),
                args.out.display()
            );
            ExitCode::SUCCESS
        }
        Err(RenderError::NoAdapter) => {
            eprintln!(
                "sandbox-capture: no compatible GPU adapter; offscreen capture needs a working GPU/driver"
            );
            ExitCode::from(3)
        }
        Err(error) => {
            eprintln!("sandbox-capture: {error}");
            ExitCode::from(1)
        }
    }
}
