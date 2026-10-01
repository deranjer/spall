//! Render generated specimens with the production soft-geometry builder.
use glam::{Quat, Vec3};
use spall_ecology::living::Season;
use spall_render::{
    Camera, CubeInstance, DebugView, EnvironmentPreset, GameRenderer, OffscreenTarget,
    RenderContext,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".local/vegetation-gallery".into());
    std::fs::create_dir_all(&output)?;
    let size = std::env::args()
        .nth(2)
        .map(|v| v.parse::<u32>())
        .transpose()?
        .unwrap_or(512);
    let scene = sandbox::worldgen_scene::generate("showcase", 1, size)?;
    let state = scene.vegetation();
    let mut materials = spall_render::materials_from_manifest(&sandbox::game::manifest());
    let colour_base = materials.len() as u32;
    for colour in spall_ecology::living::PALETTE {
        materials.push(spall_render::Material::new(
            colour.map(|c| (f32::from(c) / 255.).powf(2.2)),
            0.9,
            0.,
        ));
    }
    let ctx = RenderContext::headless()?;
    let (width, height) = (1920, 800);
    let target = OffscreenTarget::new(&ctx.device, width, height);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        (width, height),
        None,
    );
    let mut summary = Vec::new();
    for (season, name) in [
        (Season::Spring, "spring"),
        (Season::Summer, "summer"),
        (Season::Autumn, "autumn"),
        (Season::Winter, "winter"),
    ] {
        for tree in [true, false] {
            let mut geometry = Vec::new();
            let mut ids = Vec::new();
            let spacing = if tree { 5.5 } else { 1.0 };
            for (slot, s) in state.species.iter().filter(|s| s.tree == tree).enumerate() {
                let Some(p) = state
                    .organisms
                    .iter()
                    .filter(|p| p.species == s.id)
                    .max_by_key(|p| p.committed)
                else {
                    return Err(format!("missing specimen {}", s.id).into());
                };
                ids.push(s.id);
                let delta = Vec3::new(
                    slot as f32 * spacing - p.root[0] as f32 * 0.25,
                    -p.root[1] as f32 * 0.25,
                    -p.root[2] as f32 * 0.25,
                );
                for at in p.wood.iter().take(usize::from(p.committed)) {
                    geometry.push(CubeInstance::new(
                        (Vec3::from_array(at.map(|v| v as f32 * 0.25 + 0.125)) + delta).to_array(),
                        u32::from(s.wood),
                        [0.25; 3],
                        CubeInstance::IDENTITY_ROTATION,
                    ));
                }
                let mut frame = state.visual(&scene.world().terrain);
                frame.season = season;
                frame.plants.retain(|plant| plant.id == p.id);
                for part in frame.parts([p.root[0] as f32 * 0.25, 0., p.root[2] as f32 * 0.25], 30.)
                {
                    geometry.push(CubeInstance::new(
                        (Vec3::from_array(part.center) + delta).to_array(),
                        colour_base + u32::from(part.colour),
                        part.size,
                        (Quat::from_rotation_y(part.yaw) * Quat::from_rotation_z(part.lean))
                            .to_array(),
                    ));
                }
            }
            geometry.push(CubeInstance::new(
                [4.5 * spacing, -0.125, 0.],
                104,
                [10. * spacing + 4., 0.25, 8.],
                CubeInstance::IDENTITY_ROTATION,
            ));
            let center = Vec3::new(4.5 * spacing, if tree { 4.0 } else { 0.65 }, 0.);
            let eye =
                center + Vec3::new(0., if tree { 5. } else { 1.8 }, if tree { 29. } else { 6. });
            let camera = Camera::looking_along(
                eye,
                center - eye,
                50_f32.to_radians(),
                width as f32 / height as f32,
            );
            let (solid, soft): (Vec<_>, Vec<_>) = geometry
                .iter()
                .copied()
                .partition(|part| part.material < colour_base);
            renderer.set_terrain(&ctx.device, &ctx.queue, &solid);
            renderer.set_vegetation(&ctx.device, &ctx.queue, &soft);
            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("seasonal-vegetation-evidence"),
                });
            renderer.render(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                target.color_view(),
                &camera,
                &EnvironmentPreset::Daylight.environment(),
                DebugView::Shaded,
            );
            target.copy_to_readback(&mut encoder);
            ctx.queue.submit([encoder.finish()]);
            let rgba = target.read_rgba(&ctx)?;
            let group = if tree { "trees" } else { "ground" };
            image::save_buffer(
                std::path::Path::new(&output).join(format!("{group}-{name}.png")),
                &rgba,
                width,
                height,
                image::ColorType::Rgba8,
            )?;
            summary.push(serde_json::json!({"group":group,"season":name,"species":ids,"instances":geometry.len()}));
        }
    }
    let eye = Vec3::from_array(scene.player_spawns()[0].map(|v| v as f32)) + Vec3::Y * 1.7;
    let specimen = state
        .organisms
        .iter()
        .filter(|p| p.committed > 0)
        .min_by(|a, b| {
            let distance = |p: &spall_ecology::living::Organism| {
                (Vec3::from_array(p.root.map(|v| v as f32 * 0.25)) - eye).length_squared()
            };
            distance(a).total_cmp(&distance(b))
        })
        .ok_or("no established tree")?;
    let look = Vec3::from_array(specimen.root.map(|v| v as f32 * 0.25)) + Vec3::Y * 2.5;
    let camera = Camera::looking_along(
        eye,
        look - eye,
        65_f32.to_radians(),
        width as f32 / height as f32,
    );
    let terrain =
        spall_client::build_instances(&scene.world().terrain, eye.to_array().map(f64::from));
    renderer.set_terrain(&ctx.device, &ctx.queue, &terrain);
    for (season, name) in [
        (Season::Summer, "summer"),
        (Season::Autumn, "autumn"),
        (Season::Winter, "winter"),
    ] {
        let mut frame = state.visual(&scene.world().terrain);
        frame.season = season;
        let soft: Vec<_> = frame
            .parts(eye.to_array(), 48.)
            .into_iter()
            .map(|part| {
                CubeInstance::new(
                    part.center,
                    colour_base + u32::from(part.colour),
                    part.size,
                    (Quat::from_rotation_y(part.yaw) * Quat::from_rotation_z(part.lean)).to_array(),
                )
            })
            .collect();
        renderer.set_vegetation(&ctx.device, &ctx.queue, &soft);
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("generated-living-world-evidence"),
            });
        renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &camera,
            &EnvironmentPreset::Daylight.environment(),
            DebugView::Shaded,
        );
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        image::save_buffer(
            std::path::Path::new(&output).join(format!("world-{name}.png")),
            &target.read_rgba(&ctx)?,
            width,
            height,
            image::ColorType::Rgba8,
        )?;
        summary.push(serde_json::json!({"group":"generated-world","season":name,"terrain_instances":terrain.len(),"soft_instances":soft.len(),"eye":eye.to_array()}));
    }
    let counts:Vec<_>=state.species.iter().map(|s|serde_json::json!({"id":s.id,"name":sandbox::vegetation::NAMES[usize::from(s.id)-1],"count":state.organisms.iter().filter(|p|p.species==s.id).count()})).collect();
    std::fs::write(
        std::path::Path::new(&output).join("summary.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"frames":summary,"counts":counts,"state_bytes":state.encode()?.len()}),
        )?,
    )?;
    Ok(())
}
