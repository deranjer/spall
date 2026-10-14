//! The World Gen dialog's data: parsing, the column-map preview, and the
//! launcher arguments that run the previewed world.
//!
//! Everything here is plain CPU data so it can be tested without a window; the
//! dialog (`ui.rs`) draws it and `main.rs` owns the GPU texture.

use spall_worldgen::debug::{
    ColumnStats, LayerRange, PreviewLayer, biome_color, column_stats, layer_range, layer_rgba,
    layer_swatches,
};
use spall_worldgen::{Biome, ColumnMap, Preset, WorldGenSpec, WorldgenPalette};

/// The worlds the dialog can preview and run.
pub(crate) const PRESETS: [Preset; 3] = [Preset::Natural, Preset::Showcase, Preset::Gallery];

pub(crate) fn preset_label(preset: Preset) -> &'static str {
    match preset {
        Preset::Natural => "Natural world",
        Preset::Showcase => "Showcase arena",
        Preset::Gallery => "Biome gallery",
    }
}

/// One line on what a preset generates.
pub(crate) fn preset_hint(preset: Preset) -> &'static str {
    match preset {
        Preset::Natural => {
            "Ranges, rivers from springs, lakes and a sea; thirteen biomes from climate."
        }
        Preset::Showcase => "A small arena: mountains, hills, a swamp basin with lake, a desert.",
        Preset::Gallery => {
            "One pad for each biome with every plant that grows there; the sea in the corner."
        }
    }
}

/// Arena edge choices in cells (0.25 m each).
pub(crate) const SIZES: [u32; 4] = [512, 1024, 2048, 4096];
pub(crate) const DEFAULT_SIZE: u32 = 4096;
pub(crate) const SEASONS: [&str; 4] = ["spring", "summer", "autumn", "winter"];

/// A generated column map, ready to show.
pub(crate) struct Preview {
    pub preset: Preset,
    pub seed: u64,
    pub size_cells: u32,
    /// The map the world is built from; layers are drawn from it on demand.
    pub columns: ColumnMap,
    pub layer: PreviewLayer,
    /// `size_cells x size_cells` RGBA8 of `layer`, row-major with `z` as the row.
    pub rgba: Vec<u8>,
    pub stats: ColumnStats,
    /// Full-world generation may reject a valid preview at its capacity gate.
    pub launch_error: Option<String>,
}

impl Preview {
    /// Switches the picture to another layer of the same map.
    pub(crate) fn set_layer(&mut self, layer: PreviewLayer) {
        if self.layer != layer {
            self.rgba = layer_rgba(&self.columns, layer);
            self.layer = layer;
        }
    }

    /// What the current layer spans on this map, for the legend.
    pub(crate) fn range(&self) -> Option<LayerRange> {
        layer_range(&self.columns, self.layer)
    }
}

pub(crate) struct WorldgenPanel {
    pub seed_text: String,
    pub preset: Preset,
    pub layer: PreviewLayer,
    pub size_cells: u32,
    pub season: String,
    pub preset_menu_open: bool,
    pub layer_menu_open: bool,
    pub size_menu_open: bool,
    pub season_menu_open: bool,
    pub preview: Option<Preview>,
    pub error: Option<String>,
}

impl Default for WorldgenPanel {
    fn default() -> Self {
        Self {
            seed_text: random_seed().to_string(),
            preset: Preset::Natural,
            layer: PreviewLayer::Biome,
            size_cells: DEFAULT_SIZE,
            season: "summer".into(),
            preset_menu_open: false,
            layer_menu_open: false,
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
pub(crate) fn generate_preview(
    preset: Preset,
    seed: u64,
    size_cells: u32,
    layer: PreviewLayer,
) -> Result<Preview, String> {
    // The palette only names materials; the preview colours come from the map.
    let spec = WorldGenSpec::new(preset, seed, size_cells, WorldgenPalette::sequential(1));
    let columns = ColumnMap::compute(&spec).map_err(|e| e.to_string())?;
    let launch_error = spall_worldgen::generate::validate_water_budget(&columns)
        .err()
        .map(|error| error.to_string());
    Ok(Preview {
        preset,
        seed,
        size_cells,
        rgba: layer_rgba(&columns, layer),
        stats: column_stats(&columns),
        layer,
        columns,
        launch_error,
    })
}

/// Arguments after the launcher command that play the same seeded world shown
/// in the preview.
pub(crate) fn play_args(preset: Preset, seed: u64, size_cells: u32, season: &str) -> Vec<String> {
    vec![
        "xtask".into(),
        "play".into(),
        "--worldgen".into(),
        preset.name().into(),
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

/// What a layer's legend shows besides the biome list: a swatch and a label per
/// colour, and the range the layer spans on this map.
pub(crate) fn layer_legend(preview: &Preview) -> (Vec<([u8; 3], &'static str)>, Option<String>) {
    let range = preview
        .range()
        .map(|r| format!("{:.0} to {:.0} {}", r.min.min(r.max), r.max, r.unit));
    (layer_swatches(preview.layer), range)
}

/// Legend rows: label, colour, and share of the map, for the biomes the map
/// holds, in stored order (the four showcase biomes first).
pub(crate) fn legend(stats: &ColumnStats) -> Vec<(&'static str, [u8; 3], f64)> {
    Biome::EVERY
        .iter()
        .filter(|&&b| stats.biome_share[b as usize] > 0.0)
        .map(|&b| {
            let name = match b {
                Biome::Meadow => "Meadow hills",
                Biome::Alpine => "Alpine mountains",
                Biome::Swamp => "Swamp",
                Biome::Desert => "Desert",
                Biome::Ocean => "Ocean",
                Biome::Beach => "Beach",
                Biome::Riparian => "Riverbank",
                Biome::TemperateForest => "Temperate forest",
                Biome::BorealForest => "Boreal forest",
                Biome::Scree => "Scree",
                Biome::Snow => "Snow",
                Biome::Steppe => "Steppe",
                Biome::PineForest => "Pine forest",
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
        let a = generate_preview(Preset::Showcase, 1, 512, PreviewLayer::Biome).expect("generates");
        let b = generate_preview(Preset::Showcase, 1, 512, PreviewLayer::Biome).expect("generates");
        assert_eq!(a.rgba, b.rgba, "same seed, same picture");
        assert_eq!(a.rgba.len(), 512 * 512 * 4);
        let c = generate_preview(Preset::Showcase, 2, 512, PreviewLayer::Biome).expect("generates");
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
        let preview =
            generate_preview(Preset::Showcase, 7, 512, PreviewLayer::Biome).expect("generates");
        let spec = WorldGenSpec::new(Preset::Showcase, 7, 512, WorldgenPalette::sequential(1));
        let columns = ColumnMap::compute(&spec).expect("computes");
        assert_eq!(preview.rgba, spall_worldgen::debug::top_down_rgba(&columns));
    }

    #[test]
    fn unsupported_sizes_are_reported_not_clamped() {
        assert!(generate_preview(Preset::Showcase, 1, 100, PreviewLayer::Biome).is_err());
    }

    #[test]
    fn play_arguments_name_the_previewed_world() {
        assert_eq!(
            play_args(Preset::Showcase, 9, 512, "winter"),
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
        assert_eq!(DEFAULT_SIZE, 4096);
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
        let preview = generate_preview(Preset::Showcase, 1, 4096, PreviewLayer::Biome).unwrap();
        assert_eq!(preview.rgba.len(), 4096 * 4096 * 4);
        assert!(preview.launch_error.is_none(), "{:?}", preview.launch_error);
    }

    #[test]
    fn the_world_choices_include_the_natural_world_and_the_gallery() {
        assert_eq!(PRESETS.len(), 3);
        assert_eq!(WorldgenPanel::default().preset, Preset::Natural);
        for preset in PRESETS {
            assert!(!preset_label(preset).is_empty() && !preset_hint(preset).is_empty());
            assert_eq!(Preset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(
            play_args(Preset::Gallery, 3, 1024, "summer")[3],
            "gallery",
            "the launcher is told which preset to run"
        );
        assert_eq!(play_args(Preset::Natural, 3, 1024, "summer")[3], "natural");
    }

    #[test]
    fn a_natural_preview_lists_every_biome_the_map_holds_and_a_gallery_all_thirteen() {
        let natural = generate_preview(Preset::Natural, 4, 512, PreviewLayer::Biome).unwrap();
        let rows = legend(&natural.stats);
        assert_eq!(rows.len(), 13, "{rows:?}");
        let total: f64 = rows.iter().map(|r| r.2).sum();
        assert!((total - 1.0).abs() < 1e-9);
        let gallery = generate_preview(Preset::Gallery, 1, 512, PreviewLayer::Biome).unwrap();
        assert_eq!(legend(&gallery.stats).len(), 13);
    }

    #[test]
    fn switching_layers_redraws_the_same_map_and_is_reversible() {
        let mut preview = generate_preview(Preset::Natural, 5, 512, PreviewLayer::Biome).unwrap();
        let biome = preview.rgba.clone();
        for layer in PreviewLayer::ALL {
            preview.set_layer(layer);
            assert_eq!(preview.layer, layer);
            assert_eq!(preview.rgba, layer_rgba(&preview.columns, layer));
            assert_eq!(preview.rgba.len(), 512 * 512 * 4);
            let (swatches, range) = layer_legend(&preview);
            assert_eq!(swatches.is_empty(), layer == PreviewLayer::Biome);
            assert_eq!(
                range.is_some(),
                !matches!(layer, PreviewLayer::Biome | PreviewLayer::Water)
            );
        }
        preview.set_layer(PreviewLayer::Biome);
        assert_eq!(preview.rgba, biome, "back to the biome map exactly");
        // A preview made on another layer is the same picture of that layer.
        let direct = generate_preview(Preset::Natural, 5, 512, PreviewLayer::Moisture).unwrap();
        preview.set_layer(PreviewLayer::Moisture);
        assert_eq!(direct.rgba, preview.rgba);
    }

    #[test]
    fn the_preview_is_the_world_the_launcher_runs_for_each_preset() {
        for preset in PRESETS {
            let preview = generate_preview(preset, 8, 512, PreviewLayer::Biome).unwrap();
            assert_eq!(preview.preset, preset);
            let spec = WorldGenSpec::new(preset, 8, 512, WorldgenPalette::sequential(1));
            let columns = ColumnMap::compute(&spec).unwrap();
            assert_eq!(
                preview.rgba,
                layer_rgba(&columns, PreviewLayer::Biome),
                "{preset:?}"
            );
            assert!(play_args(preset, 8, 512, "summer").contains(&preset.name().to_owned()));
        }
    }

    /// Manual: writes every layer of a natural world to `.local/preview/` to look at.
    #[test]
    #[ignore = "manual: writes PNGs"]
    fn dump_the_preview_layers() {
        let dir = std::path::Path::new("../../.local/preview");
        std::fs::create_dir_all(dir).unwrap();
        let mut preview = generate_preview(Preset::Natural, 4, 1024, PreviewLayer::Biome).unwrap();
        for layer in PreviewLayer::ALL {
            preview.set_layer(layer);
            image::save_buffer(
                dir.join(format!("natural-{layer:?}.png")),
                &preview.rgba,
                1024,
                1024,
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
    }
}
