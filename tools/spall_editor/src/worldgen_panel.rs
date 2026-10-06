//! The World Gen dialog's data: parsing, the column-map preview, and the
//! launcher arguments that run the previewed world.
//!
//! Everything here is plain CPU data so it can be tested without a window; the
//! dialog (`ui.rs`) draws it and `main.rs` owns the GPU texture.

use spall_worldgen::debug::{ColumnStats, biome_color, column_stats, top_down_rgba};
use spall_worldgen::{Biome, ColumnMap, Preset, WorldGenSpec, WorldgenPalette};

/// Arena edge choices in cells (0.25 m each).
pub(crate) const SIZES: [u32; 4] = [512, 1024, 2048, 4096];
pub(crate) const DEFAULT_SIZE: u32 = 1024;
pub(crate) const SEASONS: [&str; 4] = ["spring", "summer", "autumn", "winter"];

/// A generated column map, ready to show.
pub(crate) struct Preview {
    pub seed: u64,
    pub size_cells: u32,
    /// `size_cells x size_cells` RGBA8, row-major with `z` as the row.
    pub rgba: Vec<u8>,
    pub stats: ColumnStats,
    /// Full-world generation may reject a valid preview at its capacity gate.
    pub launch_error: Option<String>,
}

pub(crate) struct WorldgenPanel {
    pub seed_text: String,
    pub size_cells: u32,
    pub season: String,
    pub size_menu_open: bool,
    pub season_menu_open: bool,
    pub preview: Option<Preview>,
    pub error: Option<String>,
}

impl Default for WorldgenPanel {
    fn default() -> Self {
        Self {
            seed_text: random_seed().to_string(),
            size_cells: DEFAULT_SIZE,
            season: "summer".into(),
            size_menu_open: false,
            season_menu_open: false,
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
    let launch_error = spall_worldgen::generate::validate_water_budget(&columns)
        .err()
        .map(|error| error.to_string());
    Ok(Preview {
        seed,
        size_cells,
        rgba: top_down_rgba(&columns),
        stats: column_stats(&columns),
        launch_error,
    })
}

/// Arguments after the launcher command that play the same seeded world shown
/// in the preview.
pub(crate) fn play_args(seed: u64, size_cells: u32, season: &str) -> Vec<String> {
    vec![
        "xtask".into(),
        "play".into(),
        "--worldgen".into(),
        Preset::Showcase.name().into(),
        "--seed".into(),
        seed.to_string(),
        "--worldgen-size".into(),
        size_cells.to_string(),
        "--season".into(),
        season.to_owned(),
    ]
}

/// A seed for the Randomize button, from the clock.
pub(crate) fn random_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64 ^ u64::from(std::process::id()))
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
            play_args(9, 512, "winter"),
            [
                "xtask",
                "play",
                "--worldgen",
                "showcase",
                "--seed",
                "9",
                "--worldgen-size",
                "512",
                "--season",
                "winter"
            ]
        );
    }

    #[test]
    fn size_choices_match_server_bounds_and_default_to_a_large_world() {
        assert_eq!(DEFAULT_SIZE, 1024);
        assert_eq!(SIZES, [512, 1024, 2048, 4096]);
        assert!(
            SIZES
                .iter()
                .all(|size| (128..=4096).contains(size) && size % 32 == 0)
        );
    }

    #[test]
    fn launch_options_start_with_a_seed_and_valid_season() {
        let options = WorldgenPanel::default();
        assert!(options.seed_text.parse::<u64>().is_ok());
        assert!(SEASONS.contains(&options.season.as_str()));
    }

    #[test]
    #[ignore = "large-world preview capacity regression; no terrain allocation"]
    fn large_preview_passes_the_extended_capacity_check() {
        let preview = generate_preview(1, 4096).unwrap();
        assert_eq!(preview.rgba.len(), 4096 * 4096 * 4);
        assert!(preview.launch_error.is_none(), "{:?}", preview.launch_error);
    }
}
