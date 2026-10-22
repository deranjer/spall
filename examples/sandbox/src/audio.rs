//! Game-owned ambience catalogue. Audio playback is an optional client service;
//! these biome-to-file choices stay out of the Spall engine crates.

use std::path::{Path, PathBuf};

use spall_client::audio::{BiomeAudioConfig, WeatherLayer};
use spall_client::breathing::{BreathingConfig, BreathingRow, BreathingState};
use spall_client::footsteps::{FootstepConfig, FootstepSurface};
use spall_core::MaterialId;
use spall_protocol::WeatherKind;
use spall_worldgen::Biome;

use crate::game::materials;

/// Precipitation (0..=1) from which snow counts as heavy; light and medium
/// snow (0.25, 0.5) lie below it and heavy (0.85) above.
const HEAVY_SNOW: f32 = 0.7;

pub const TRACK_NAMES: [&str; 13] = [
    "meadow-pipit.ogg",
    "alpine-chiffchaff.ogg",
    "swamp-ambience.ogg",
    "dry-grass-wind.ogg",
    "ocean-waves.ogg",
    "beach-waves.ogg",
    "riparian-stream.ogg",
    "temperate-forest.ogg",
    "pine-wind.ogg",
    "", // Scree stays quiet; gravel is foley, not ambience.
    "", // No snowfield wind recording was accepted.
    "dry-grass-wind.ogg",
    "pine-wind.ogg",
];

/// How far each recording carries. Water is audible from much further than wind
/// or birdsong, so a stream is heard well before the bank and fades gradually.
fn track_range_m(name: &str) -> f32 {
    match name {
        "riparian-stream.ogg" => 85.0,
        "ocean-waves.ogg" => 130.0,
        "beach-waves.ogg" => 110.0,
        "swamp-ambience.ogg" => 60.0,
        _ => spall_client::audio::DEFAULT_RANGE_M,
    }
}

/// One footstep surface. Recordings live in `foley/` as `<name>-walk.ogg`,
/// optionally `<name>-run.ogg` for run steps; a surface without a walk
/// recording plays the generic default (row 0), so adding a surface is the
/// recording plus, for a new surface, one row here.
struct SurfaceRow {
    name: &'static str,
    /// Recording stem when it differs from `name`.
    stem: Option<&'static str>,
    gain: f32,
    pitch: f32,
    materials: &'static [MaterialId],
}

const fn row(name: &'static str, gain: f32, materials: &'static [MaterialId]) -> SurfaceRow {
    SurfaceRow {
        name,
        stem: None,
        gain,
        pitch: 1.0,
        materials,
    }
}

/// Quiet rustle of plants layered under grass steps.
const GRASS_RUSTLE: &str = "tall-grass-rustle.ogg";
const GRASS_RUSTLE_GAIN: f32 = 0.25;

/// Row 0 is the quiet generic default; the rest follow the terrain materials.
/// Snow and water are not materials: they are picked by snow depth and by
/// standing in water (see `FootstepConfig::select_surface`). `tile-walk.ogg`
/// and `tile-run.ogg` are shipped for built blocks but no sandbox material is
/// tile-like yet, so they have no row.
const SURFACES: [SurfaceRow; 9] = [
    SurfaceRow {
        stem: Some("gravel"),
        pitch: 0.8,
        ..row("generic", 0.3, &[])
    },
    SurfaceRow {
        stem: Some("snow"),
        ..row("snow", 1.0, &[materials::SNOW])
    },
    row("gravel", 1.0, &[materials::GRAVEL]),
    row("grass", 0.8, &[MaterialId(10), materials::MOSS]),
    row(
        "mud",
        0.9,
        &[materials::DIRT, materials::MUD, materials::CLAY],
    ),
    row(
        "rock",
        1.0,
        &[
            materials::STONE,
            materials::BEDROCK,
            materials::DEEP_STONE,
            MaterialId(13),
            MaterialId(14),
        ],
    ),
    row("sand", 0.8, &[materials::SAND]),
    row("wood", 1.0, &[materials::WOOD]),
    SurfaceRow {
        stem: Some("water"),
        ..row("water", 0.9, &[])
    },
];

/// The recordings of a surface: `<stem>-walk.ogg` and `<stem>-run.ogg`, with
/// the two exceptions below for the older and the wading files.
fn recordings(row: &SurfaceRow, root: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let foley = root.join("foley");
    let stem = row.stem.unwrap_or(row.name);
    let existing = |name: String| Some(foley.join(name)).filter(|path| path.is_file());
    match stem {
        "snow" => (existing("snow-footsteps.ogg".into()), None),
        "water" => (existing("water-wade.ogg".into()), None),
        _ => (
            existing(format!("{stem}-walk.ogg")),
            existing(format!("{stem}-run.ogg")),
        ),
    }
}

/// The player's breathing, one looping recording per state under `player/`, and
/// the gain it plays at. Exhausted also plays a single heavy breath on entry. A
/// recording that is not shipped leaves its state (or that breath) silent.
const BREATHING: [(BreathingState, &str, f32, Option<&str>); 3] = [
    (BreathingState::Calm, "breathing-calm.ogg", 0.12, None),
    (BreathingState::Exerted, "breathing-exerted.ogg", 0.45, None),
    (
        BreathingState::Exhausted,
        "breathing-exhausted.ogg",
        0.5,
        Some("breathing-accent.ogg"),
    ),
];

/// The breathing table with each recording resolved against `root`.
pub fn breathing(root: &Path) -> BreathingConfig {
    let existing = |file: &str| Some(root.join("player").join(file)).filter(|path| path.is_file());
    BreathingConfig {
        rows: BREATHING
            .iter()
            .map(|&(state, file, gain, onset)| BreathingRow {
                state,
                path: existing(file),
                gain,
                looped: true,
                onset: onset.and_then(existing),
            })
            .collect(),
    }
}

/// The footstep table with each recording resolved against `root`.
pub fn footsteps(root: &Path) -> FootstepConfig {
    let index_of = |name: &str| SURFACES.iter().position(|row| row.name == name);
    FootstepConfig {
        surfaces: SURFACES
            .iter()
            .map(|row| {
                let (walk, run) = recordings(row, root);
                let grass = row.name == "grass";
                FootstepSurface {
                    name: row.name.to_owned(),
                    walk,
                    run,
                    layer: grass
                        .then(|| root.join("foley").join(GRASS_RUSTLE))
                        .filter(|path| path.is_file()),
                    layer_gain: if grass { GRASS_RUSTLE_GAIN } else { 0.0 },
                    gain: row.gain,
                    pitch: row.pitch,
                }
            })
            .collect(),
        material_surface: SURFACES
            .iter()
            .enumerate()
            .flat_map(|(index, row)| row.materials.iter().map(move |m| (m.0, index)))
            .collect(),
        snow_surface: index_of("snow"),
        water_surface: index_of("water"),
        snow_min_depth_m: 0.01,
        walk_step_m: crate::player_view::WALK_STRIDE_M / 2.0,
        run_step_m: crate::player_view::RUN_STRIDE_M / 2.0,
        walk_min_speed_m_s: crate::player_view::WALK_MIN_SPEED_M_S,
        run_min_speed_m_s: crate::player_view::RUN_MIN_SPEED_M_S,
    }
}

/// A looping weather bed that plays whatever the precipitation.
fn bed(kind: WeatherKind, path: PathBuf, gain: f32) -> WeatherLayer {
    band(kind, 0.0..f32::INFINITY, path, gain)
}

/// A weather bed for one band of precipitation (0..=1) of a weather kind.
fn band(
    kind: WeatherKind,
    precipitation: std::ops::Range<f32>,
    path: PathBuf,
    gain: f32,
) -> WeatherLayer {
    WeatherLayer {
        kind: kind as u8,
        precipitation_min: precipitation.start,
        precipitation_max: precipitation.end,
        path,
        gain,
    }
}

/// Finds assets beside the executable first (portable/editor launches), then
/// falls back to the sandbox source tree (Cargo developer runs from any CWD).
pub fn config() -> BiomeAudioConfig {
    let root = audio_root();
    BiomeAudioConfig {
        tracks: TRACK_NAMES
            .iter()
            .map(|name| (!name.is_empty()).then(|| root.join("biomes").join(name)))
            .collect(),
        labels: Biome::EVERY
            .into_iter()
            .map(|biome| format!("{biome:?}"))
            .collect(),
        weather_layers: vec![
            bed(WeatherKind::Rain, root.join("weather/light-rain.ogg"), 1.0),
            bed(WeatherKind::Storm, root.join("weather/heavy-rain.ogg"), 1.0),
            // Every snow tier has the quiet night bed; heavy snow adds a little
            // wind under it, and a blizzard howls with wind beneath.
            bed(
                WeatherKind::Snow,
                root.join("weather/light-snow-night.ogg"),
                1.0,
            ),
            band(
                WeatherKind::Snow,
                HEAVY_SNOW..f32::INFINITY,
                root.join("weather/arctic-wind-bed.ogg"),
                0.6,
            ),
            bed(
                WeatherKind::HighWind,
                root.join("biomes/pine-wind.ogg"),
                1.0,
            ),
            bed(
                WeatherKind::Blizzard,
                root.join("weather/blizzard-howling-storm.ogg"),
                1.0,
            ),
            bed(
                WeatherKind::Blizzard,
                root.join("weather/arctic-wind-bed.ogg"),
                0.4,
            ),
        ],
        thunder: Some(root.join("weather/thunder-clap.ogg")),
        ranges_m: TRACK_NAMES.iter().map(|name| track_range_m(name)).collect(),
        water_tracks: TRACK_NAMES
            .iter()
            .map(|name| {
                matches!(
                    *name,
                    "riparian-stream.ogg" | "ocean-waves.ogg" | "beach-waves.ogg"
                )
            })
            .collect(),
        birdsong_tracks: TRACK_NAMES
            .iter()
            .map(|name| {
                matches!(
                    *name,
                    "meadow-pipit.ogg" | "alpine-chiffchaff.ogg" | "temperate-forest.ogg"
                )
            })
            .collect(),
        bird_species: vec!["crow".to_owned()],
        birdsong_pool: ["blackbird.ogg", "robin.ogg", "blackcap.ogg"]
            .iter()
            .map(|name| root.join("birds").join(name))
            .collect(),
        footsteps: footsteps(&root),
        breathing: breathing(&root),
    }
}

pub fn audio_root() -> PathBuf {
    let adjacent = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|path| path.join("assets").join("audio"));
    if let Some(path) = adjacent.filter(|path| path.is_dir()) {
        return path;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("audio")
}

#[cfg(test)]
fn source_audio_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("assets")
        .join("audio")
}

pub fn track_name(biome: Biome) -> Option<&'static str> {
    let name = TRACK_NAMES[biome as usize];
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_biomes_are_mapped_and_snow_scree_are_silent() {
        for biome in Biome::EVERY {
            if matches!(biome, Biome::Snow | Biome::Scree) {
                assert_eq!(track_name(biome), None);
            } else {
                assert!(track_name(biome).is_some(), "missing mapping for {biome:?}");
            }
        }
    }

    #[test]
    fn every_mapped_runtime_file_exists() {
        let config = BiomeAudioConfig {
            tracks: TRACK_NAMES
                .iter()
                .map(|name| {
                    (!name.is_empty()).then(|| source_audio_root().join("biomes").join(name))
                })
                .collect(),
            ..Default::default()
        };
        for path in config.tracks.into_iter().flatten() {
            assert!(
                path.is_file(),
                "missing runtime ambience {}",
                path.display()
            );
        }
    }

    #[test]
    fn every_birdsong_recording_exists_and_decodes() {
        let pool = config().birdsong_pool;
        assert!(!pool.is_empty());
        for path in pool {
            spall_client::audio::validate_asset(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }

    #[test]
    fn weather_controls_only_select_approved_decodable_assets() {
        let config = config();
        assert!(config.weather_mix(WeatherKind::Clear as u8, 0.0).is_empty());
        for layer in &config.weather_layers {
            spall_client::audio::validate_asset(&layer.path).unwrap();
            assert!(
                !layer
                    .path
                    .to_string_lossy()
                    .to_lowercase()
                    .contains("snowfall")
            );
        }
        spall_client::audio::validate_asset(&config.thunder.unwrap()).unwrap();
    }

    #[test]
    fn snow_tiers_blizzard_and_rain_pick_their_beds() {
        let config = config();
        let names = |kind: WeatherKind, precipitation: f32| -> Vec<(String, f32)> {
            config
                .weather_mix(kind as u8, precipitation)
                .into_iter()
                .map(|(path, gain)| {
                    (
                        path.file_stem().unwrap().to_string_lossy().into_owned(),
                        gain,
                    )
                })
                .collect()
        };
        let night = ("light-snow-night".to_owned(), 1.0);
        assert_eq!(names(WeatherKind::Snow, 0.25), vec![night.clone()]);
        assert_eq!(names(WeatherKind::Snow, 0.5), vec![night.clone()]);
        assert_eq!(
            names(WeatherKind::Snow, 0.85),
            [night, ("arctic-wind-bed".to_owned(), 0.6)]
        );
        let blizzard = names(WeatherKind::Blizzard, 0.95);
        assert_eq!(blizzard[0].0, "blizzard-howling-storm");
        assert!(blizzard[1].0 == "arctic-wind-bed" && blizzard[1].1 < blizzard[0].1);
        assert_eq!(
            names(WeatherKind::Rain, 0.5),
            [("light-rain".to_owned(), 1.0)]
        );
    }

    #[test]
    fn footstep_table_resolves_shipped_recordings() {
        let config = footsteps(&source_audio_root());
        for surface in &config.surfaces {
            let paths = [&surface.walk, &surface.run, &surface.layer];
            for path in paths.into_iter().flatten() {
                assert!(path.is_file());
            }
            assert!(surface.walk.is_some(), "{} has no recording", surface.name);
            let has_run = matches!(
                surface.name.as_str(),
                "generic" | "gravel" | "rock" | "sand"
            );
            assert_eq!(surface.run.is_some(), has_run, "{}", surface.name);
            assert_eq!(surface.layer.is_some(), surface.name == "grass");
        }
        assert_eq!(config.surfaces[0].name, "generic");
        let surface_name = |material: Option<MaterialId>, snow: f32, water: bool| {
            let index = config.select_surface(material.map(|m| m.0), snow, water);
            config.surfaces[index].name.as_str()
        };
        assert_eq!(surface_name(Some(materials::GRAVEL), 0.0, false), "gravel");
        assert_eq!(surface_name(Some(MaterialId(10)), 0.0, false), "grass");
        assert_eq!(surface_name(Some(materials::MOSS), 0.0, false), "grass");
        assert_eq!(surface_name(Some(materials::DIRT), 0.0, false), "mud");
        assert_eq!(surface_name(Some(materials::MUD), 0.0, false), "mud");
        assert_eq!(surface_name(Some(materials::STONE), 0.0, false), "rock");
        assert_eq!(surface_name(Some(materials::SAND), 0.0, false), "sand");
        assert_eq!(surface_name(Some(materials::WOOD), 0.0, false), "wood");
        assert_eq!(surface_name(Some(materials::SNOW), 0.0, false), "snow");
        assert_eq!(surface_name(Some(materials::STONE), 0.05, false), "snow");
        assert_eq!(surface_name(Some(materials::SAND), 0.0, true), "water");
        assert_eq!(surface_name(Some(MaterialId(999)), 0.0, false), "generic");
        assert_eq!(surface_name(None, 0.0, false), "generic");
    }

    #[test]
    fn snow_recording_slices_into_its_twelve_steps_and_every_recording_slices() {
        let config = footsteps(&source_audio_root());
        let snow = config.surfaces.iter().find(|s| s.name == "snow").unwrap();
        let steps = spall_client::footsteps::step_count(snow.walk.as_deref().unwrap()).unwrap();
        assert_eq!(steps, 12);
        for surface in &config.surfaces {
            for path in [&surface.walk, &surface.run, &surface.layer]
                .into_iter()
                .flatten()
            {
                let steps = spall_client::footsteps::step_count(path).unwrap();
                assert!(steps >= 2, "{} slices into {steps}", path.display());
                println!("{} {steps}", path.display());
            }
        }
    }

    #[test]
    fn every_approved_runtime_asset_decodes_without_an_audio_device() {
        let root = source_audio_root();
        let mut checked = 0;
        for category in ["biomes", "weather", "foley", "player"] {
            for entry in std::fs::read_dir(root.join(category)).unwrap() {
                let path = entry.unwrap().path();
                assert_eq!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("ogg")
                );
                spall_client::audio::validate_asset(&path)
                    .unwrap_or_else(|error| panic!("could not decode {}: {error}", path.display()));
                checked += 1;
            }
        }
        assert_eq!(checked, 39, "all 39 user-approved recordings are packaged");
    }

    #[test]
    fn short_alpine_call_decodes_across_its_loop_boundary() {
        let path = source_audio_root()
            .join("biomes")
            .join("alpine-chiffchaff.ogg");
        spall_client::audio::validate_loop_boundary(&path).unwrap();
    }
}
