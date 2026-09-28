//! Renders a project's scene the way the editor viewport does (exact authored
//! colours, its saved environment, skylight and bounce) from the game's spawn
//! camera, for comparison with `cargo xtask play --editor-scene <project>`.
//!
//! `cargo run -p spall_editor --example parity_capture -- fixtures/terrain-trees-forest out.png [environment]`
use glam::Vec3;
use spall_editor::EditorModel;
use spall_editor::scene_mesh::build_scene_mesh;
use spall_render::{
    Camera, EnvironmentPreset, MeshChunk, RenderContext, ViewportFrame, ViewportRenderer,
};

const SIZE: (u32, u32) = (1280, 720);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let project = args
        .next()
        .unwrap_or_else(|| "fixtures/terrain-trees-forest".into());
    let out = args.next().unwrap_or_else(|| "parity-editor.png".into());
    let model = EditorModel::load(&project)?;
    let (composite, owners) = model.scene_composite();
    let mesh = build_scene_mesh(&composite, &owners, None, false);
    // The saved environment, or the one named by the third argument (the game
    // runs a scene with no saved environment in daylight).
    let key = args
        .next()
        .unwrap_or_else(|| model.scene.environment.clone());
    let environment = EnvironmentPreset::from_key(&key)
        .unwrap_or_default()
        .environment();

    let ctx = RenderContext::headless()?;
    let mut viewport = ViewportRenderer::new(&ctx, &mesh.materials);
    viewport.resize(&ctx, SIZE.0, SIZE.1);
    let chunks: Vec<_> = mesh
        .chunks
        .iter()
        .map(|c| MeshChunk {
            vertices: &c.vertices,
            indices: &c.indices,
        })
        .collect();
    viewport.set_meshes(&ctx, &chunks)?;
    viewport.set_sky_occupancy(&ctx, mesh.sky_occupancy().as_ref());

    // The game's spawn view: the lawn centre, eye 1.6 m up, looking along -Z.
    let mut camera = Camera::looking_along(
        Vec3::new(19.875, 0.5 + 1.6, 19.875),
        Vec3::new(0.3, -0.05, -1.0),
        75_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    camera.z_far = 300.0;
    // Several frames so the time-sliced lighting sweep has finished.
    for _ in 0..12 {
        viewport.render(
            &ctx,
            &ViewportFrame {
                camera,
                environment,
            },
        );
    }
    let rgba = viewport.read_pixels(&ctx)?;
    image::save_buffer(&out, &rgba, SIZE.0, SIZE.1, image::ColorType::Rgba8)?;
    println!("wrote {out} ({} materials)", mesh.materials.len());
    Ok(())
}
