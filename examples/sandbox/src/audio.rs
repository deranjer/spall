//! Game-owned ambience catalogue. Audio playback is an optional client service;
//! these biome-to-file choices stay out of the Spall engine crates.

use std::path::{Path, PathBuf};

use spall_client::audio::{BiomeAudioConfig, WeatherLayer};
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

/// One footstep surface. Recordings live in `foley/`; a row whose file is not
/// shipped yet plays the generic default (row 0), so adding a surface is the
/// recording plus, for a new surface, one row here.
struct SurfaceRow {
    name: &'static str,
    recording: &'static str,
    gain: f32,
    pitch: f32,
    materials: &'static [MaterialId],
}

/// Row 0 is the quiet generic default; the rest follow the terrain materials.
/// Snow and water are not materials: they are picked by snow depth and by
/// standing in water (see `FootstepConfig::select_surface`).
const SURFACES: [SurfaceRow; 9] = [
    SurfaceRow {
        name: "generic",
        recording: "gravel-footsteps.ogg",
        gain: 0.3,
        pitch: 0.8,
        materials: &[],
    },
    SurfaceRow {
        name: "snow",
        recording: "snow-footsteps.ogg",
        gain: 1.0,
        pitch: 1.0,
        materials: &[materials::SNOW],
    },
    SurfaceRow {
        name: "gravel",
        recording: "gravel-footsteps.ogg",
        gain: 1.0,
        pitch: 1.0,
        materials: &[materials::GRAVEL],
    },
    SurfaceRow {
        name: "grass",
        recording: "grass-footsteps.ogg",
        gain: 0.8,
        pitch: 1.0,
        materials: &[MaterialId(10), materials::MOSS],
    },
    SurfaceRow {
        name: "mud",
        recording: "mud-footsteps.ogg",
        gain: 0.9,
        pitch: 1.0,
        materials: &[materials::DIRT, materials::MUD, materials::CLAY],
    },
    SurfaceRow {
        name: "rock",
        recording: "rock-footsteps.ogg",
        gain: 1.0,
        pitch: 1.0,
        materials: &[
            materials::STONE,
            materials::BEDROCK,
            materials::DEEP_STONE,
            MaterialId(13),
            MaterialId(14),
        ],
    },
    SurfaceRow {
        name: "sand",
        recording: "sand-footsteps.ogg",
        gain: 0.8,
        pitch: 1.0,
        materials: &[materials::SAND],
    },
    SurfaceRow {
        name: "wood",
        recording: "wood-footsteps.ogg",
        gain: 1.0,
        pitch: 1.0,
        materials: &[materials::WOOD],
    },
    SurfaceRow {
        name: "water",
        recording: "water-footsteps.ogg",
        gain: 0.9,
        pitch: 1.0,
        materials: &[],
    },
];

/// The footstep table with each recording resolved against `root`.
pub fn footsteps(root: &Path) -> FootstepConfig {
    let index_of = |name: &str| SURFACES.iter().position(|row| row.name == name);
    FootstepConfig {
        surfaces: SURFACES
            .iter()
            .map(|row| {
                let file = root.join("foley").join(row.recording);
                FootstepSurface {
                    name: row.name.to_owned(),
                    file: file.is_file().then_some(file),
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
        footsteps: footsteps(&root),
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
    fn footstep_table_resolves_shipped_recordings_and_falls_back_for_the_rest() {
        let config = footsteps(&source_audio_root());
        for surface in &config.surfaces {
            let expected = matches!(surface.name.as_str(), "generic" | "snow" | "gravel");
            assert_eq!(surface.file.is_some(), expected, "{}", surface.name);
            if let Some(file) = &surface.file {
                assert!(file.is_file());
            }
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
    fn snow_recording_slices_into_its_twelve_steps() {
        let config = footsteps(&source_audio_root());
        let snow = config.surfaces.iter().find(|s| s.name == "snow").unwrap();
        let steps = spall_client::footsteps::step_count(snow.file.as_deref().unwrap()).unwrap();
        assert_eq!(steps, 12);
    }

    #[test]
    fn every_approved_runtime_asset_decodes_without_an_audio_device() {
        let root = source_audio_root();
        let mut checked = 0;
        for category in ["biomes", "weather", "foley"] {
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
        assert_eq!(checked, 23, "all 23 user-approved recordings are packaged");
    }

    #[test]
    fn short_alpine_call_decodes_across_its_loop_boundary() {
        let path = source_audio_root()
            .join("biomes")
            .join("alpine-chiffchaff.ogg");
        spall_client::audio::validate_loop_boundary(&path).unwrap();
    }
}
