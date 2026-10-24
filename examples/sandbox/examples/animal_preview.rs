//! Bounded headless captures of the authored articulated animal and player fixtures.
//!
//! `cargo run -p sandbox --features client --example animal-preview -- --bounded
//! --out .local/animal-preview` writes four stills per clip, one contact sheet
//! per specimen (a row per clip),
//! and a deterministic JSON inventory. It starts no game, world, or network.

use std::{collections::BTreeMap, error::Error, path::PathBuf};

use glam::{Vec3, vec3};
use image::{RgbaImage, imageops};
use sandbox::animal_visual::{AnimalCell, AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use spall_core::MaterialId;
use spall_render::{
    Camera, CubeInstance, DebugView, EnvironmentPreset, GameRenderer, Material, OffscreenTarget,
    RenderContext,
};

const WIDTH: u32 = 384;
const HEIGHT: u32 = 384;
const POSES_PER_CLIP: usize = 4;

struct Specimen {
    name: &'static str,
    /// Captured clips, one contact-sheet row each.
    clips: &'static [&'static str],
    bytes: &'static [u8],
}

fn main() -> Result<(), Box<dyn Error>> {
    let (output, bounded) = arguments()?;
    if !bounded {
        return Err("pass --bounded to run the fixed bounded preview".into());
    }
    std::fs::create_dir_all(&output)?;
    let manifest = sandbox::game::manifest();
    let mapping = material_mapping(&manifest);
    let specimens = [
        Specimen {
            name: "trout",
            clips: &["idle", "swim"],
            bytes: include_bytes!("../assets/animals/trout.spvox"),
        },
        Specimen {
            name: "crow",
            clips: &["idle", "flap", "hop", "peck", "land", "takeoff"],
            bytes: include_bytes!("../assets/animals/crow.spvox"),
        },
        Specimen {
            name: "deer",
            clips: &["idle", "walk", "run", "graze", "alert", "sleep"],
            bytes: include_bytes!("../assets/animals/deer.spvox"),
        },
        Specimen {
            name: "bear",
            clips: &["idle", "walk", "run", "swipe", "eat", "stand"],
            bytes: include_bytes!("../assets/animals/bear.spvox"),
        },
        Specimen {
            name: "leopard",
            clips: &["idle", "walk", "stalk", "pounce", "run"],
            bytes: include_bytes!("../assets/animals/leopard.spvox"),
        },
        Specimen {
            name: "wolf",
            clips: &["idle", "walk", "run", "stalk", "bite", "eat", "howl"],
            bytes: include_bytes!("../assets/animals/wolf.spvox"),
        },
        Specimen {
            name: "player",
            clips: &["idle", "walk", "run", "swim"],
            bytes: include_bytes!("../assets/characters/player.spvox"),
        },
    ];
    let frame_count: usize =
        specimens.iter().map(|s| s.clips.len()).sum::<usize>() * POSES_PER_CLIP;
    let visuals: Vec<_> = specimens
        .iter()
        .map(|s| {
            let asset = spall_asset::decode(s.bytes)?;
            if asset.name.to_lowercase() != s.name {
                return Err(format!("asset name mismatch: {}", asset.name).into());
            }
            Ok((
                s,
                AnimalVisual::new(asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)?,
            ))
        })
        .collect::<Result<_, Box<dyn Error>>>()?;

    let mut frames = Vec::with_capacity(frame_count);
    for (specimen, visual) in &visuals {
        for (row, &clip_name) in specimen.clips.iter().enumerate() {
            let duration = visual
                .clip_duration(clip_name)
                .ok_or("required clip is missing")?;
            for sample in 0..POSES_PER_CLIP {
                let tick = (u64::from(duration) * sample as u64 / POSES_PER_CLIP as u64) as f64;
                let cells = visual.sample(clip_name, tick)?;
                frames.push((specimen.name, row, clip_name, tick as u32, cells));
            }
        }
    }
    if frames.len() != frame_count {
        return Err("bounded frame count changed".into());
    }
    // One fixed camera/floor per animal exposes motion instead of cancelling
    // root translation by automatically following each sampled pose.
    let mut specimen_bounds = BTreeMap::<&str, (Vec3, Vec3)>::new();
    for (animal, _, _, _, cells) in &frames {
        let bounds = specimen_bounds
            .entry(animal)
            .or_insert((Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)));
        for cell in cells {
            let half = cell.size * 0.5;
            let extent = (cell.rotation * Vec3::X).abs() * half.x
                + (cell.rotation * Vec3::Y).abs() * half.y
                + (cell.rotation * Vec3::Z).abs() * half.z;
            bounds.0 = bounds.0.min(cell.center - extent);
            bounds.1 = bounds.1.max(cell.center + extent);
        }
    }

    let mut materials = spall_render::materials_from_manifest(&manifest);
    let mut tint_slots = BTreeMap::<(MaterialId, [u8; 3]), u32>::new();
    for (_, _, _, _, cells) in &frames {
        for cell in cells {
            if let Some(tint) = cell.tint_srgb {
                let key = (cell.material, tint);
                if let std::collections::btree_map::Entry::Vacant(entry) = tint_slots.entry(key) {
                    let base = *materials
                        .get(usize::from(cell.material.0))
                        .ok_or("mapped material ID is outside renderer table")?;
                    let mut tinted = base;
                    tinted.base_color = tint.map(srgb_to_linear);
                    let index = u32::try_from(materials.len())
                        .map_err(|_| "renderer material table too large")?;
                    materials.push(tinted);
                    entry.insert(index);
                }
            }
        }
    }
    let ground_material =
        u32::try_from(materials.len()).map_err(|_| "renderer material table too large")?;
    materials.push(Material::new([0.57, 0.63, 0.66], 0.96, 0.0));

    let ctx = RenderContext::headless()?;
    let target = OffscreenTarget::new(&ctx.device, WIDTH, HEIGHT);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        (WIDTH, HEIGHT),
        None,
    );
    let mut sheets = BTreeMap::<&str, RgbaImage>::new();
    let mut reports = Vec::with_capacity(frames.len());
    let rows: BTreeMap<&str, usize> = specimens.iter().map(|s| (s.name, s.clips.len())).collect();
    for (frame_index, (animal, row, clip, tick, cells)) in frames.iter().enumerate() {
        let (min, max) = specimen_bounds[animal];
        let center = (min + max) * 0.5;
        let radius = ((max - min).length() * 0.5).max(0.25);
        let floor_y = min.y - 0.08;
        let cubes = render_geometry(cells, ground_material, &tint_slots, center, radius, floor_y)?;
        renderer.set_terrain(&ctx.device, &ctx.queue, &cubes);
        let eye = center + vec3(radius * 1.15, radius * 0.75, radius * 3.0);
        let camera = Camera::looking_along(
            eye,
            center - eye,
            38_f32.to_radians(),
            WIDTH as f32 / HEIGHT as f32,
        );
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bounded-animal-preview"),
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
        let frame_path = output.join(format!("{animal}-{clip}-{tick:03}.png"));
        image::save_buffer(&frame_path, &rgba, WIDTH, HEIGHT, image::ColorType::Rgba8)?;
        let image = RgbaImage::from_raw(WIDTH, HEIGHT, rgba)
            .ok_or("renderer returned wrong image dimensions")?;
        let sheet = sheets.entry(animal).or_insert_with(|| {
            RgbaImage::new(WIDTH * POSES_PER_CLIP as u32, HEIGHT * rows[animal] as u32)
        });
        imageops::replace(
            sheet,
            &image,
            (frame_index % POSES_PER_CLIP) as i64 * i64::from(WIDTH),
            *row as i64 * i64::from(HEIGHT),
        );
        reports.push(serde_json::json!({
            "animal": animal,
            "clip": clip,
            "tick": tick,
            "cells": cells.len(),
            "tinted_cells": cells.iter().filter(|cell| cell.tint_srgb.is_some()).count(),
            "camera_radius_m": radius,
            "floor_y_m": floor_y,
            "file": frame_path.file_name().and_then(|n| n.to_str()),
        }));
        println!(
            "frame {}/{}: {animal} {clip} tick {tick}, {} cells",
            frame_index + 1,
            frame_count,
            cells.len()
        );
    }
    for (animal, sheet) in sheets {
        sheet.save(output.join(format!("{animal}-contact.png")))?;
    }
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "bounded": true,
            "frames": frame_count,
            "frame_size": [WIDTH, HEIGHT],
            "clips": specimens.iter().map(|s| (s.name, s.clips)).collect::<BTreeMap<_, _>>(),
            "render_path": "spall_render::GameRenderer headless offscreen",
            "adapter": ctx.adapter_name(),
            "backend": format!("{:?}", ctx.backend()),
            "camera": "fixed per animal over union of all sampled oriented-cell bounds",
            "frames_report": reports,
        }))?,
    )?;
    println!(
        "wrote {} bounded frame captures and {} contact sheets to {}",
        frame_count,
        specimens.len(),
        output.display()
    );
    Ok(())
}

fn arguments() -> Result<(PathBuf, bool), Box<dyn Error>> {
    let mut output = PathBuf::from(".local/animal-preview");
    let mut bounded = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bounded" => bounded = true,
            "--out" => output = args.next().ok_or("--out requires a directory")?.into(),
            "--help" | "-h" => {
                println!(
                    "animal-preview --bounded [--out DIRECTORY]\nCaptures four poses per clip and one contact sheet per specimen."
                );
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument {arg:?}").into()),
        }
    }
    Ok((output, bounded))
}

fn render_geometry(
    cells: &[AnimalCell],
    ground_material: u32,
    tint_slots: &BTreeMap<(MaterialId, [u8; 3]), u32>,
    center: Vec3,
    radius: f32,
    floor_y: f32,
) -> Result<Vec<CubeInstance>, Box<dyn Error>> {
    if cells.is_empty() {
        return Err("animal pose contains no visible cells".into());
    }
    let mut cubes = Vec::with_capacity(cells.len() + 1);
    for cell in cells {
        let material = if let Some(tint) = cell.tint_srgb {
            *tint_slots
                .get(&(cell.material, tint))
                .ok_or("preview tint was not registered")?
        } else {
            u32::from(cell.material.0)
        };
        cubes.push(CubeInstance::new(
            cell.center.to_array(),
            material,
            cell.size.to_array(),
            cell.rotation.to_array(),
        ));
    }
    cubes.push(CubeInstance::new(
        [center.x, floor_y, center.z],
        ground_material,
        [radius * 3.4, 0.08, radius * 3.4],
        CubeInstance::IDENTITY_ROTATION,
    ));
    Ok(cubes)
}

fn srgb_to_linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}
