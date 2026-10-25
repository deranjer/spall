//! Deep snow and blizzards slow a walking character (ENG-252).
//!
//! The kernel ([`crate::step_character_in_water`]) takes one `ground_scale`
//! multiplier on walk and run speed. Server and client predictor derive it the
//! same way: [`ground_cell_below`] finds the terrain cell under the feet, each
//! side reads the authoritative snow depth on that cell's top face from its
//! own copy of the snow state, and [`snow_speed_scale`] turns depth and
//! weather into the multiplier.

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{Sample, Volume};

use crate::character::tuning;

/// The solid terrain cell a foot at `feet_m` stands on and its material: the
/// cell just below the sole, or the one under that where the capsule floats a
/// skin above.
pub fn ground_cell_below(volume: &Volume, feet_m: [f64; 3]) -> Option<(GlobalCell, MaterialId)> {
    if !feet_m.iter().all(|v| v.is_finite()) {
        return None;
    }
    let cell_m = volume.cell_size().metres();
    [0.05, 0.3].into_iter().find_map(|drop| {
        let cell = GlobalCell::new(
            (feet_m[0] / cell_m).floor() as i64,
            ((feet_m[1] - drop) / cell_m).floor() as i64,
            (feet_m[2] / cell_m).floor() as i64,
        );
        match volume.sample(cell) {
            Ok(Sample::Filled(material)) => Some((cell, material)),
            _ => None,
        }
    })
}

/// Speed multiplier for snow `depth_q16` (Q16 metres, the replicated unit)
/// deep under the feet, in a blizzard or not. Depth is bucketed to
/// 1/16 m, so the small lag between the server's snow state and a client's
/// replicated copy rarely changes the result. Bare ground is `1.0`. Snow that
/// has settled on the ground is the proxy for being outdoors: a blizzard
/// slows a character only where snow lies under the feet.
pub fn snow_speed_scale(depth_q16: u16, blizzard: bool) -> f32 {
    if depth_q16 == 0 {
        return 1.0;
    }
    let steps = (depth_q16 >> tuning::SNOW_DEPTH_BUCKET_SHIFT) as f32;
    let full = (32_768_u16 >> tuning::SNOW_DEPTH_BUCKET_SHIFT) as f32;
    let depth_t = (steps / full).clamp(0.0, 1.0);
    let mut scale = 1.0 - tuning::SNOW_DEEP_SLOWDOWN * depth_t;
    if blizzard {
        scale *= 1.0 - tuning::BLIZZARD_SLOWDOWN;
    }
    scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_falls_with_depth_and_blizzard() {
        assert_eq!(snow_speed_scale(0, true), 1.0);
        let thin = snow_speed_scale(2_000, false);
        let deep = snow_speed_scale(32_768, false);
        assert_eq!(thin, 1.0); // below the first 1/16 m bucket
        assert!((deep - (1.0 - tuning::SNOW_DEEP_SLOWDOWN)).abs() < 1e-6);
        assert!(snow_speed_scale(16_384, false) < 1.0);
        assert!(snow_speed_scale(16_384, false) > deep);
        let storm = snow_speed_scale(32_768, true);
        assert!(storm < deep);
        assert!(storm > 0.5, "stays walkable: {storm}");
        // Depth beyond the replicated maximum cannot push below the floor.
        assert_eq!(snow_speed_scale(u16::MAX, false), deep);
    }
}
