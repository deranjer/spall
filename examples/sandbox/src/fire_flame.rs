//! The look of a burning fire pit (ENG-267): a few emissive voxel cubes that
//! flicker in height and colour, embers that rise and fade, and a light smoke
//! wisp. Pure client-side drawing: [`flame_voxels`] is the whole effect for one
//! fire at one instant, in item-local metres, and always emits at most
//! [`MAX_FLAME_CUBES`] cubes.

use spall_client::window::ItemVoxel;
use spall_core::MaterialId;

use crate::game::materials::{LAMP, STONE};

/// Flame colours hot to cool, then two smoke greys, as `(material, sRGB)`;
/// the view appends them to its item tints.
pub const FLAME_PALETTE: [(MaterialId, [u8; 3]); 7] = [
    (LAMP, [255, 244, 190]), // white-yellow core
    (LAMP, [255, 205, 80]),  // yellow
    (LAMP, [255, 135, 30]),  // orange
    (LAMP, [225, 60, 15]),   // red tips
    (LAMP, [255, 95, 25]),   // ember
    (STONE, [70, 68, 66]),   // smoke
    (STONE, [105, 103, 100]),
];

const CORE: usize = 0;
const YELLOW: usize = 1;
const ORANGE: usize = 2;
const RED: usize = 3;
const EMBER: usize = 4;
const SMOKE: usize = 5;

const EMBERS: usize = 3;
const WISPS: usize = 3;
/// 1 core + 3 tongues each with a tip, plus embers and smoke wisps.
pub const MAX_FLAME_CUBES: usize = 1 + 3 * 2 + EMBERS + WISPS;
/// Height the flame starts at: just above the sticks.
const BASE_Y: f32 = 0.08;

/// Fuel (seconds) at and above which the flame is full size; a fire with less
/// burns a smaller flame, never below 60% of full.
const FULL_FLAME_FUEL_S: f32 = 600.0;

/// A slow wobble in `0..=1`: two sines of unrelated rates.
fn wobble(time_s: f32, rate: f32, phase: f32) -> f32 {
    0.5 + 0.25 * ((time_s * rate + phase).sin() + (time_s * rate * 2.3 + phase * 1.7).sin())
}

/// A stable pseudo-random phase (radians) for a fire from its id.
pub fn seed_phase(seed: u64) -> f32 {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40;
    mixed as f32 / (1u64 << 24) as f32 * std::f32::consts::TAU
}

/// The cubes of a burning fire. `palette_base` is the index of
/// [`FLAME_PALETTE`]'s first entry in the view's item tints. Nothing for fuel
/// that is not above zero.
pub fn flame_voxels(fuel_s: f32, time_s: f32, phase: f32, palette_base: u16) -> Vec<ItemVoxel> {
    if fuel_s.is_nan() || fuel_s <= 0.0 {
        return Vec::new();
    }
    let scale = 0.6 + 0.4 * (fuel_s / FULL_FLAME_FUEL_S).min(1.0);
    let tint = |colour: usize| Some(palette_base + colour as u16);
    let lamp = u32::from(LAMP.0);
    let rock = u32::from(STONE.0);
    let mut out = Vec::with_capacity(MAX_FLAME_CUBES);

    // The core: a wide, short, white-yellow block that breathes.
    let core_h = 0.16 * scale * (0.8 + 0.4 * wobble(time_s, 9.0, phase));
    out.push(ItemVoxel {
        center: [0.0, BASE_Y + core_h * 0.5, 0.0],
        size: [0.2 * scale, core_h, 0.2 * scale],
        material: lamp,
        tint: tint(CORE),
    });
    // Three tongues round it, each with a smaller tip that flickers
    // independently and cools to red.
    for i in 0..3 {
        let a = phase + i as f32 * std::f32::consts::TAU / 3.0;
        let w = wobble(time_s, 7.0 + i as f32 * 1.7, a);
        let h = (0.14 + 0.16 * w) * scale;
        let (dx, dz) = (0.055 * a.cos() * scale, 0.055 * a.sin() * scale);
        let width = 0.1 * scale;
        out.push(ItemVoxel {
            center: [dx, BASE_Y + core_h * 0.4 + h * 0.5, dz],
            size: [width, h, width],
            material: lamp,
            tint: tint(if w > 0.5 { YELLOW } else { ORANGE }),
        });
        let tip_w = wobble(time_s, 13.0 + i as f32, a + 1.0);
        let tip_h = (0.05 + 0.07 * tip_w) * scale;
        let tip = width * 0.55;
        out.push(ItemVoxel {
            center: [
                dx + 0.012 * (time_s * 5.0 + a).sin(),
                BASE_Y + core_h * 0.4 + h + tip_h * 0.5,
                dz,
            ],
            size: [tip, tip_h, tip],
            material: lamp,
            tint: tint(if tip_w > 0.45 { ORANGE } else { RED }),
        });
    }
    // Embers: small bright cubes that rise on a drifting path, then shrink
    // to nothing and start again.
    for i in 0..EMBERS {
        let period = 1.5 + 0.45 * i as f32;
        let life = ((time_s / period) + phase * 0.37 + i as f32 * 0.33).fract();
        let a = phase + i as f32 * 2.1;
        let size = 0.035 * (1.0 - life) * scale;
        out.push(ItemVoxel {
            center: [
                0.05 * a.cos() + 0.12 * life * (a + life * 3.0).sin(),
                0.3 + 0.9 * life,
                0.05 * a.sin() + 0.12 * life * (a * 1.3 + life * 2.0).cos(),
            ],
            size: [size; 3],
            material: lamp,
            tint: tint(EMBER),
        });
    }
    // Smoke: grey cubes that start above the flame, grow and drift as they
    // rise, and thin out (shrink) near the top of their climb.
    for i in 0..WISPS {
        let period = 3.2 + 0.6 * i as f32;
        let life = ((time_s / period) + phase * 0.21 + i as f32 / WISPS as f32).fract();
        let a = phase * 1.9 + i as f32 * 2.3;
        let size = (0.05 + 0.1 * life) * (1.0 - life * life) * scale;
        out.push(ItemVoxel {
            center: [
                0.2 * life * a.cos() + 0.04 * (time_s * 2.0 + a).sin(),
                0.45 + 1.3 * life,
                0.2 * life * a.sin(),
            ],
            size: [size; 3],
            material: rock,
            tint: Some(palette_base + (SMOKE + i % 2) as u16),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unlit fire (no fuel) draws nothing; a lit one stays within the cube
    /// budget at any time, fuel, or phase.
    #[test]
    fn an_unlit_fire_emits_nothing_and_a_lit_one_is_bounded() {
        assert!(flame_voxels(0.0, 3.0, 1.0, 10).is_empty());
        assert!(flame_voxels(f32::NAN, 3.0, 1.0, 10).is_empty());
        for fuel in [1.0, 180.0, 1800.0, 1.0e9] {
            for step in 0..200 {
                let cubes = flame_voxels(fuel, step as f32 * 0.37, 0.4 * step as f32, 10);
                assert!(!cubes.is_empty() && cubes.len() <= MAX_FLAME_CUBES);
                for cube in &cubes {
                    assert!(cube.size.iter().all(|s| s.is_finite() && *s >= 0.0));
                    assert!(cube.center.iter().all(|c| c.is_finite()));
                    assert!(cube.center[1] > 0.0 && cube.center[1] < 2.0);
                    assert!(cube.tint.is_some_and(|t| (10..17).contains(&t)));
                }
            }
        }
    }

    /// The flame actually moves, and its colour changes over time.
    #[test]
    fn the_flame_flickers_in_height_and_colour() {
        let frames: Vec<_> = (0..40)
            .map(|i| flame_voxels(600.0, i as f32 * 0.1, 0.5, 0))
            .collect();
        let heights: Vec<f32> = frames.iter().map(|f| f[1].size[1]).collect();
        assert!(heights.iter().any(|h| (h - heights[0]).abs() > 0.02));
        let colours: std::collections::HashSet<_> = frames.iter().map(|f| f[1].tint).collect();
        assert!(colours.len() > 1);
    }
}
