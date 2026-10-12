//! Prints (and writes to `.local/gallery-legend.md`) what the gallery world
//! shows on each biome's pad: where it is, which living plants stand on it and
//! which cover species can grow there.
//!
//! `cargo run --release -p sandbox --example gallery_legend -- [size]`

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sandbox::vegetation::{catalogue, cover_catalogue, name};
use spall_worldgen::gallery_patches;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let size: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let scene = sandbox::worldgen_scene::generate("gallery", 1, size)?;
    let cols = &scene.world().columns;
    let state = scene.vegetation();
    let palette = sandbox::game::terrain_palette();
    let cover = cover_catalogue();
    let living = catalogue();
    let mut out = String::new();
    writeln!(
        out,
        "# Gallery legend (size {size} cells, {} m)\n",
        f64::from(size) * 0.25
    )?;
    writeln!(
        out,
        "Pads run west to east, north to south. Fly with F and use the centre to find a pad; the first spawn is the meadow.\n"
    )?;
    for pad in gallery_patches(size) {
        let (cx, cz) = pad.centre();
        let ground = f64::from(cols.height(cx, cz)) * 0.25;
        writeln!(
            out,
            "## {:?}\n\ncentre x = {:.0} m, z = {:.0} m, ground y = {:.0} m; pad {:.0} m across\n",
            pad.biome,
            cx as f64 * 0.25,
            cz as f64 * 0.25,
            ground,
            (pad.x1 - pad.x0) as f64 * 0.25
        )?;
        let mut got: BTreeMap<u16, usize> = BTreeMap::new();
        for p in &state.organisms {
            if (pad.x0..pad.x1).contains(&p.root[0]) && (pad.z0..pad.z1).contains(&p.root[2]) {
                *got.entry(p.species).or_default() += 1;
            }
        }
        let trees: Vec<_> = got
            .iter()
            .filter(|(id, _)| living.iter().any(|s| s.id == **id && s.tree))
            .map(|(id, n)| format!("{} x{n}", name(*id)))
            .collect();
        let plants: Vec<_> = got
            .iter()
            .filter(|(id, _)| living.iter().any(|s| s.id == **id && !s.tree))
            .map(|(id, n)| format!("{} x{n}", name(*id)))
            .collect();
        writeln!(
            out,
            "- **Trees:** {}",
            if trees.is_empty() {
                "none".into()
            } else {
                trees.join(", ")
            }
        )?;
        writeln!(
            out,
            "- **Plants:** {}",
            if plants.is_empty() {
                "none".into()
            } else {
                plants.join(", ")
            }
        )?;
        // Cover: every species with 30+ sample columns on the pad that suit it.
        let mut hits = vec![0u32; cover.len()];
        for z in (pad.z0..pad.z1).step_by(3) {
            for x in (pad.x0..pad.x1).step_by(3) {
                let top = cols.stack(&palette, x, z).top.0;
                let m = cols.moisture(x, z);
                let t = cols.temperature_c(x, z).round() as i32;
                let depth = cols.water_depth(x, z).clamp(0, 255) as u8;
                for (i, c) in cover.iter().enumerate() {
                    if c.biomes & (1 << pad.biome as u16) != 0
                        && c.soils.contains(&top)
                        && (c.moisture[0]..=c.moisture[1]).contains(&m)
                        && (c.depth[0]..=c.depth[1]).contains(&depth)
                        && (i32::from(c.temperature[0])..=i32::from(c.temperature[1])).contains(&t)
                    {
                        hits[i] += 1;
                    }
                }
            }
        }
        let names: Vec<_> = cover
            .iter()
            .zip(&hits)
            .filter(|(_, n)| **n >= 30)
            .map(|(c, _)| name(c.id))
            .collect();
        writeln!(
            out,
            "- **Cover:** {}\n",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        )?;
    }
    println!("{out}");
    std::fs::create_dir_all(".local")?;
    std::fs::write(".local/gallery-legend.md", &out)?;
    Ok(())
}
