//! The World Gen dialog's data: parsing, the column-map preview, and the
//! `cargo xtask play` arguments that run the previewed world.
//!
//! Everything here is plain CPU data so it can be tested without a window; the
//! dialog (`ui.rs`) draws it and `main.rs` owns the GPU texture.

use spall_worldgen::debug::{ColumnStats, biome_color, column_stats, top_down_rgba};
use spall_worldgen::{Biome, ColumnMap, Preset, WorldGenSpec, WorldgenPalette};

/// Arena edges the dialog offers, in cells (0.25 m each): 128 m and 256 m.
pub(crate) const SIZES: [u32; 2] = [512, 1024];
pub(crate) const DEFAULT_SIZE: u32 = 1024;

/// A generated column map, ready to show.
pub(crate) struct Preview {
    pub seed: u64,
    pub size_cells: u32,
    /// `size_cells x size_cells` RGBA8, row-major with `z` as the row.
    pub rgba: Vec<u8>,
    pub stats: ColumnStats,
}

pub(crate) struct WorldgenPanel {
    pub seed_text: String,
    pub size_cells: u32,
    pub preview: Option<Preview>,
    pub error: Option<String>,
}

impl Default for WorldgenPanel {
    fn default() -> Self {
        Self {
            seed_text: "1".into(),
            size_cells: DEFAULT_SIZE,
            preview: None,
            error: None,
        }
    }
}

/// Parses the seed field: any `u64`, trimmed. An empty field is seed 1, the
/// game's default.
pub(crate) fn parse_seed(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(1);
    }
    text.parse::<u64>()
        .map_err(|_| format!("Seed must be a whole number from 0 to {}", u64::MAX))
}

/// Generates the biome/height/water map for `(seed, size_cells)`. The same
/// inputs give the same picture as the world the server will build, because
/// both come from [`ColumnMap::compute`].
pub(crate) fn generate_preview(seed: u64, size_cells: u32) -> Result<Preview, String> {
    // The palette only names materials; the preview colours come from biomes.
    let spec = WorldGenSpec::new(
        Preset::Showcase,
        seed,
        size_cells,
        WorldgenPalette::sequential(1),
    );
    let columns = ColumnMap::compute(&spec).map_err(|e| e.to_string())?;
    Ok(Preview {
        seed,
        size_cells,
        rgba: top_down_rgba(&columns),
        stats: column_stats(&columns),
    })
}

/// Arguments after `cargo` that play the world: it is built by the sandbox
/// server from the same seed and size as the preview.
pub(crate) fn play_args(seed: u64, size_cells: u32) -> Vec<String> {
    vec![
        "xtask".into(),
        "play".into(),
        "--worldgen".into(),
        Preset::Showcase.name().into(),
        "--seed".into(),
        seed.to_string(),
        "--worldgen-size".into(),
        size_cells.to_string(),
    ]
}

/// A seed for the Randomize button, from the clock.
pub(crate) fn random_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64 % 1_000_000)
}

/// Legend rows: label, colour, and share of the map, in [`Biome::ALL`] order.
pub(crate) fn legend(stats: &ColumnStats) -> Vec<(&'static str, [u8; 3], f64)> {
    Biome::ALL
        .iter()
        .map(|&b| {
            let name = match b {
                Biome::Meadow => "Meadow hills",
                Biome::Alpine => "Alpine mountains",
                Biome::Swamp => "Swamp",
                Biome::Desert => "Desert",
            };
            (name, biome_color(b), stats.biome_share[b as usize])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_parsing_accepts_numbers_and_defaults_empty() {
        assert_eq!(parse_seed(" 42 "), Ok(42));
        assert_eq!(parse_seed(""), Ok(1));
        assert!(parse_seed("abc").is_err());
        assert!(parse_seed("-3").is_err());
    }

    #[test]
    fn preview_is_deterministic_and_covers_every_biome() {
        let a = generate_preview(1, 512).expect("generates");
        let b = generate_preview(1, 512).expect("generates");
        assert_eq!(a.rgba, b.rgba, "same seed, same picture");
        assert_eq!(a.rgba.len(), 512 * 512 * 4);
        let c = generate_preview(2, 512).expect("generates");
        assert_ne!(a.rgba, c.rgba, "a different seed changes the picture");
        let rows = legend(&a.stats);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|(_, _, share)| *share > 0.0), "{rows:?}");
        let total: f64 = rows.iter().map(|r| r.2).sum();
        assert!((total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn preview_matches_the_world_the_server_builds() {
        // The preview is only trustworthy if it is the server's own column map.
        let preview = generate_preview(7, 512).expect("generates");
        let spec = WorldGenSpec::new(Preset::Showcase, 7, 512, WorldgenPalette::sequential(1));
        let columns = ColumnMap::compute(&spec).expect("computes");
        assert_eq!(preview.rgba, top_down_rgba(&columns));
    }

    #[test]
    fn unsupported_sizes_are_reported_not_clamped() {
        assert!(generate_preview(1, 100).is_err());
    }

    #[test]
    fn play_arguments_name_the_previewed_world() {
        assert_eq!(
            play_args(9, 512),
            [
                "xtask",
                "play",
                "--worldgen",
                "showcase",
                "--seed",
                "9",
                "--worldgen-size",
                "512"
            ]
        );
    }
}
