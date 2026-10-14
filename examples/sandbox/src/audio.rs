//! Game-owned ambience catalogue. Audio playback is an optional client service;
//! these biome-to-file choices stay out of the Spall engine crates.

use std::path::{Path, PathBuf};

use spall_client::audio::BiomeAudioConfig;
use spall_worldgen::Biome;

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

/// Finds assets beside the executable first (portable/editor launches), then
/// falls back to the sandbox source tree (Cargo developer runs from any CWD).
pub fn config() -> BiomeAudioConfig {
    let root = audio_root();
    BiomeAudioConfig {
        tracks: TRACK_NAMES
            .iter()
            .map(|name| (!name.is_empty()).then(|| root.join("biomes").join(name)))
            .collect(),
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
        assert_eq!(checked, 19, "all 19 user-approved recordings are packaged");
    }

    #[test]
    fn short_alpine_call_decodes_across_its_loop_boundary() {
        let path = source_audio_root()
            .join("biomes")
            .join("alpine-chiffchaff.ogg");
        spall_client::audio::validate_loop_boundary(&path).unwrap();
    }
}
