//! Named lighting environments shared by every renderer that draws a Spall
//! scene: the editor's scene viewport (through [`crate::ViewportRenderer`]) and
//! the interactive game window. Both read the same numbers, so a scene looks
//! the same in the editor as it does in the game.
//!
//! This module is pure data and CPU math; it touches no GPU resource.
//!
//! Persisted scenes store [`EnvironmentPreset::key`] (a stable lowercase
//! string), never the Rust enum layout.

use glam::Vec3;

/// The built-in environments. Add a variant only together with its
/// [`EnvironmentPreset::environment`] entry and a stable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EnvironmentPreset {
    /// Neutral grey-on-dark studio lighting; the editor default.
    #[default]
    Studio,
    Daylight,
    Overcast,
    Sunset,
    Night,
}

impl EnvironmentPreset {
    pub const ALL: [Self; 5] = [
        Self::Studio,
        Self::Daylight,
        Self::Overcast,
        Self::Sunset,
        Self::Night,
    ];

    /// Stable persisted identifier.
    pub fn key(self) -> &'static str {
        match self {
            Self::Studio => "studio",
            Self::Daylight => "daylight",
            Self::Overcast => "overcast",
            Self::Sunset => "sunset",
            Self::Night => "night",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Studio => "Studio",
            Self::Daylight => "Daylight",
            Self::Overcast => "Overcast",
            Self::Sunset => "Sunset",
            Self::Night => "Night",
        }
    }

    /// Case-insensitive lookup by [`Self::key`].
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.key().eq_ignore_ascii_case(key.trim()))
    }

    pub fn environment(self) -> Environment {
        // Directions point from the light toward the scene.
        match self {
            Self::Studio => Environment {
                sun_dir: Vec3::new(-0.4, -0.82, -0.4).normalize(),
                sun_color: [1.0, 1.0, 1.0],
                sun_intensity: 4.5,
                sky: [0.30, 0.30, 0.30],
                ground: [0.13, 0.13, 0.13],
                sun_angular_diameter_deg: 2.0,
                exposure: 1.0,
                background: [0x12, 0x15, 0x1a],
                sun_visibility: 1.0,
                moon_dir: Vec3::new(0.0, -1.0, 0.0),
                moon_color: [0.52, 0.62, 0.92],
                moon_intensity: 0.025,
                moon_phase: 0.5,
                moon_visibility: 0.04,
            },
            Self::Daylight => Environment {
                sun_dir: Vec3::new(-0.45, -0.75, -0.35).normalize(),
                sun_color: [1.0, 0.96, 0.88],
                sun_intensity: 5.0,
                sky: [0.30, 0.40, 0.60],
                ground: [0.14, 0.13, 0.11],
                sun_angular_diameter_deg: 1.0,
                exposure: 1.0,
                background: [120, 170, 220],
                sun_visibility: 1.0,
                moon_dir: Vec3::new(0.0, -1.0, 0.0),
                moon_color: [0.52, 0.62, 0.92],
                moon_intensity: 0.025,
                moon_phase: 0.5,
                moon_visibility: 0.16,
            },
            Self::Overcast => Environment {
                sun_dir: Vec3::new(-0.2, -0.95, -0.15).normalize(),
                sun_color: [0.88, 0.93, 1.0],
                sun_intensity: 1.6,
                sky: [0.50, 0.55, 0.62],
                ground: [0.25, 0.26, 0.28],
                sun_angular_diameter_deg: 6.0,
                exposure: 0.9,
                background: [120, 132, 145],
                sun_visibility: 1.0,
                moon_dir: Vec3::new(0.0, -1.0, 0.0),
                moon_color: [0.52, 0.62, 0.92],
                moon_intensity: 0.025,
                moon_phase: 0.5,
                moon_visibility: 0.12,
            },
            Self::Sunset => Environment {
                sun_dir: Vec3::new(-0.9, -0.18, -0.35).normalize(),
                sun_color: [1.0, 0.52, 0.24],
                sun_intensity: 5.0,
                sky: [0.26, 0.20, 0.28],
                ground: [0.14, 0.08, 0.05],
                sun_angular_diameter_deg: 1.2,
                exposure: 1.0,
                background: [180, 92, 60],
                sun_visibility: 1.0,
                moon_dir: Vec3::new(0.0, -1.0, 0.0),
                moon_color: [0.52, 0.62, 0.92],
                moon_intensity: 0.025,
                moon_phase: 0.5,
                moon_visibility: 0.16,
            },
            Self::Night => Environment {
                sun_dir: Vec3::new(0.3, -0.7, 0.4).normalize(),
                sun_color: [0.55, 0.68, 1.0],
                sun_intensity: 0.7,
                sky: [0.04, 0.06, 0.14],
                ground: [0.015, 0.02, 0.04],
                sun_angular_diameter_deg: 0.6,
                exposure: 1.2,
                background: [8, 12, 31],
                sun_visibility: 0.0,
                moon_dir: Vec3::new(0.0, -1.0, 0.0),
                moon_color: [0.52, 0.62, 0.92],
                moon_intensity: 0.08,
                moon_phase: 0.5,
                moon_visibility: 1.0,
            },
        }
    }
}

/// Everything a renderer needs to light a scene and clear the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Environment {
    /// Unit vector from the sun toward the scene.
    pub sun_dir: Vec3,
    /// Linear sun tint.
    pub sun_color: [f32; 3],
    /// Scales the sun contribution (radiance before the Lambert `1/pi`).
    pub sun_intensity: f32,
    /// Linear ambient for surfaces facing up.
    pub sky: [f32; 3],
    /// Linear ambient for surfaces facing down.
    pub ground: [f32; 3],
    /// Angular diameter of the light source in degrees; sets how quickly
    /// shadow penumbrae widen with occluder distance (PCSS). Physical sunlight
    /// is ~0.53; presets use artistic values, larger under overcast skies.
    pub sun_angular_diameter_deg: f32,
    /// Linear exposure applied before tone mapping.
    pub exposure: f32,
    /// Displayed (sRGB-encoded) background colour after tone mapping.
    pub background: [u8; 3],
    /// Sun-disc visibility independently of direct-light intensity.
    pub sun_visibility: f32,
    /// Unit vector from the moon toward the scene.
    pub moon_dir: Vec3,
    /// Linear moon tint.
    pub moon_color: [f32; 3],
    /// Direct moonlight strength; low by design, with readable ambient kept
    /// separately in `sky` and `ground`.
    pub moon_intensity: f32,
    /// Lunar age in the range [0, 1): new at 0, full at 0.5.
    pub moon_phase: f32,
    /// Visible disc brightness after daylight washout and horizon fade.
    pub moon_visibility: f32,
}

impl Default for Environment {
    fn default() -> Self {
        EnvironmentPreset::Studio.environment()
    }
}

impl Environment {
    /// Linear HDR clear colour that lands on [`Self::background`] once the
    /// tone map (fitted ACES at [`Self::exposure`]) has run.
    pub fn hdr_clear(&self) -> [f64; 4] {
        let [r, g, b] = self
            .background
            .map(|c| pre_tone_map(srgb_to_linear(c), self.exposure));
        [r, g, b, 1.0]
    }

    /// [`Self::background`] as a linear colour, for a target that is written
    /// without a tone-map pass (an sRGB surface encodes it on store).
    pub fn background_linear(&self) -> [f64; 4] {
        let [r, g, b] = self.background.map(|c| f64::from(srgb_to_linear(c)));
        [r, g, b, 1.0]
    }
}

/// Inverse of the tone map's fitted ACES curve at `exposure`.
fn pre_tone_map(display_linear: f32, exposure: f32) -> f64 {
    let aces = |x: f32| {
        let (a, b, c, d, e) = (2.51_f32, 0.03, 2.43, 0.59, 0.14);
        ((x * (a * x + b)) / (x * (c * x + d) + e)).clamp(0.0, 1.0)
    };
    let target = display_linear.clamp(0.0, 0.999);
    let (mut lo, mut hi) = (0.0_f32, 64.0_f32);
    for _ in 0..40 {
        let mid = (lo + hi) * 0.5;
        if aces(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    f64::from(lo * 0.5 + hi * 0.5) / f64::from(exposure)
}

fn srgb_to_linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_are_unique() {
        for preset in EnvironmentPreset::ALL {
            assert_eq!(EnvironmentPreset::from_key(preset.key()), Some(preset));
        }
        assert_eq!(
            EnvironmentPreset::from_key(" Sunset "),
            Some(EnvironmentPreset::Sunset)
        );
        assert_eq!(EnvironmentPreset::from_key("mars"), None);
    }

    #[test]
    fn studio_is_the_default_and_matches_the_original_fixed_lighting() {
        let env = Environment::default();
        assert_eq!(env, EnvironmentPreset::Studio.environment());
        assert_eq!(env.sun_intensity, 4.5);
        assert_eq!(env.sky, [0.30; 3]);
        assert_eq!(env.ground, [0.13; 3]);
    }

    #[test]
    fn environments_actually_differ_in_lighting() {
        let all = EnvironmentPreset::ALL.map(EnvironmentPreset::environment);
        for (i, a) in all.iter().enumerate() {
            assert!((a.sun_dir.length() - 1.0).abs() < 1e-5);
            for b in &all[i + 1..] {
                assert_ne!(a.sun_dir, b.sun_dir);
                assert_ne!(a.sun_color, b.sun_color);
                assert_ne!(a.sky, b.sky);
            }
        }
    }

    #[test]
    fn the_clear_colour_survives_tone_mapping() {
        // Forward ACES of the pre-tone-map value reproduces the target.
        let exposure = 0.85;
        let x = (pre_tone_map(0.25, exposure) * f64::from(exposure)) as f32;
        let (a, b, c, d, e) = (2.51_f32, 0.03, 2.43, 0.59, 0.14);
        let out = (x * (a * x + b)) / (x * (c * x + d) + e);
        assert!((out - 0.25).abs() < 1e-3, "{out}");
    }
}
