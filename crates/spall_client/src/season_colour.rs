//! Seasonal colour of greenery, presentation only.
//!
//! The server's materials and vegetation never change with the season; the
//! client re-tints the render colours of the greenery materials (grass, moss,
//! terrain foliage) and of the conifer and moss entries of the vegetation
//! palette. Deciduous trees already drop their leaves in winter and cover
//! plants already pick a per-season palette entry (`spall_ecology`), so those
//! are not touched here.
//!
//! The tint follows the season clock: it holds for most of a season and blends
//! into the next one over the last quarter ([`BLEND_START`]), so it is already
//! the next season's colour when the ecology switches. A season the clock
//! jumps to (F10) is approached at [`EASE_PER_S`] rather than snapped.

use spall_render::Material;

/// What a material is, for seasonal tinting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Greenery {
    Grass,
    Moss,
    /// Broadleaf foliage voxels: yellow in autumn, brown in winter.
    Leaf,
    /// Evergreen needles: darker and bluer in winter, never brown.
    Conifer,
}

impl Greenery {
    /// The kind of a manifest material by name (`grass`, `grass.v03`, `moss`,
    /// `foliage.oak.v01`, ...).
    pub fn of_material(name: &str) -> Option<Self> {
        let stem = name.split('.').next().unwrap_or(name);
        match stem {
            "grass" => Some(Self::Grass),
            "moss" => Some(Self::Moss),
            "foliage" => Some(Self::Leaf),
            _ => None,
        }
    }

    /// The kind of an entry of [`spall_ecology::living::PALETTE`].
    pub fn of_palette(index: usize) -> Option<Self> {
        match index {
            // Pine, sage, teal, lichen, deep green and dark pine: the evergreens.
            2 | 13 | 14 | 21 | 25 | 31 => Some(Self::Conifer),
            20 => Some(Self::Moss),
            _ => None,
        }
    }
}

/// Fraction of a season after which the colours start blending into the next.
pub const BLEND_START: f32 = 0.75;
/// How fast the shown season weights follow their target, weight per second.
pub const EASE_PER_S: f32 = 1.0;
/// Shown weights must differ from the uploaded ones by this much to re-upload.
const UPLOAD_STEP: f32 = 1.0 / 64.0;

/// Weights of spring, summer, autumn and winter colours (sum 1) for a season
/// clock: `start_season` (0 spring .. 3 winter) advanced by `clock_ms` in
/// seasons of `season_ms`.
pub fn season_weights(start_season: u8, clock_ms: u64, season_ms: u64) -> [f32; 4] {
    let season_ms = season_ms.max(1);
    let index = (u64::from(start_season) + clock_ms / season_ms) % 4;
    let fraction = (clock_ms % season_ms) as f32 / season_ms as f32;
    let t = ((fraction - BLEND_START) / (1.0 - BLEND_START)).clamp(0.0, 1.0);
    let t = t * t * (3.0 - 2.0 * t);
    let mut weights = [0.0; 4];
    weights[index as usize] = 1.0 - t;
    weights[(index as usize + 1) % 4] += t;
    weights
}

fn linear(srgb: [u8; 3]) -> [f32; 3] {
    srgb.map(|c| (f32::from(c) / 255.).powf(2.2))
}

fn luminance(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// `base` pulled `amount` of the way to `tone`, with `tone` scaled by how much
/// lighter or darker `base` is than the reference green so that the variants
/// of one material keep their spread.
fn pull(base: [f32; 3], tone: [u8; 3], amount: f32, reference: [f32; 3]) -> [f32; 3] {
    let scale = (luminance(base) / luminance(reference).max(1e-4)).clamp(0.75, 1.3);
    let tone = linear(tone);
    std::array::from_fn(|i| base[i] + (tone[i] * scale - base[i]) * amount)
}

/// The linear colour `kind` has in a season (0 spring .. 3 winter), from the
/// material's authored colour `base`.
pub fn seasonal_colour(kind: Greenery, base: [f32; 3], season: usize) -> [f32; 3] {
    const GRASS: [u8; 3] = [76, 126, 51];
    const MOSS: [u8; 3] = [64, 92, 43];
    const LEAF: [u8; 3] = [90, 130, 56];
    match (kind, season) {
        (Greenery::Grass, 2) => pull(base, [128, 120, 56], 0.65, linear(GRASS)),
        (Greenery::Grass, 3) => pull(base, [136, 122, 78], 0.9, linear(GRASS)),
        (Greenery::Moss, 2) => pull(base, [82, 90, 46], 0.3, linear(MOSS)),
        (Greenery::Moss, 3) => pull(base, [54, 60, 48], 0.85, linear(MOSS)),
        (Greenery::Leaf, 2) => pull(base, [176, 122, 40], 0.75, linear(LEAF)),
        (Greenery::Leaf, 3) => pull(base, [98, 76, 52], 0.9, linear(LEAF)),
        (Greenery::Conifer, 3) => {
            let tint = [0.62, 0.74, 0.86];
            std::array::from_fn(|i| base[i] * tint[i])
        }
        _ => base,
    }
}

/// The colour for season `weights`: the weighted mix of the four seasons.
pub fn blended_colour(kind: Greenery, base: [f32; 3], weights: [f32; 4]) -> [f32; 3] {
    let mut out = [0.0; 3];
    for (season, weight) in weights.into_iter().enumerate() {
        if weight > 0.0 {
            let colour = seasonal_colour(kind, base, season);
            for (o, c) in out.iter_mut().zip(colour) {
                *o += c * weight;
            }
        }
    }
    out
}

/// The render material table with its greenery re-tinted for the season.
pub struct SeasonalMaterials {
    base: Vec<Material>,
    kinds: Vec<Option<Greenery>>,
    shown: [f32; 4],
    uploaded: Option<[f32; 4]>,
}

impl SeasonalMaterials {
    /// `kinds[i]` is the kind of `base[i]` (missing entries are untouched).
    pub fn new(base: Vec<Material>, kinds: Vec<Option<Greenery>>) -> Self {
        Self {
            base,
            kinds,
            shown: [0.0, 1.0, 0.0, 0.0],
            uploaded: None,
        }
    }

    /// Moves the shown season toward `target` by `dt` seconds (the first call
    /// takes it at once) and returns the new table if it changed enough to
    /// upload. `None` when no greenery material exists or nothing changed.
    pub fn update(&mut self, target: [f32; 4], dt: f32) -> Option<Vec<Material>> {
        if self.uploaded.is_none() {
            self.shown = target;
        } else {
            let step = EASE_PER_S * dt.max(0.0);
            for (shown, target) in self.shown.iter_mut().zip(target) {
                *shown += (target - *shown).clamp(-step, step);
            }
        }
        let changed = self.uploaded.is_none_or(|uploaded| {
            uploaded
                .iter()
                .zip(self.shown)
                .any(|(a, b)| (a - b).abs() >= UPLOAD_STEP)
        }) || (self.uploaded != Some(self.shown) && self.shown == target);
        if !changed || !self.kinds.iter().any(Option::is_some) {
            return None;
        }
        self.uploaded = Some(self.shown);
        Some(
            self.base
                .iter()
                .zip(&self.kinds)
                .map(|(material, kind)| match kind {
                    Some(kind) => Material {
                        base_color: blended_colour(*kind, material.base_color, self.shown),
                        ..*material
                    },
                    None => *material,
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRASS: [f32; 3] = [0.07, 0.19, 0.03];

    #[test]
    fn weights_hold_then_blend_into_the_next_season() {
        let season = 1000;
        assert_eq!(season_weights(1, 0, season), [0., 1., 0., 0.]);
        assert_eq!(season_weights(1, 700, season), [0., 1., 0., 0.]);
        let mid = season_weights(1, 875, season);
        assert!(mid[1] > 0.0 && mid[2] > 0.0 && (mid.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        // Winter blends into the next spring and the clock wraps.
        assert_eq!(season_weights(3, 0, season), [0., 0., 0., 1.]);
        assert_eq!(season_weights(3, 1000, season), [1., 0., 0., 0.]);
        assert_eq!(season_weights(2, 3 * season + 10, season), [0., 1., 0., 0.]);
    }

    #[test]
    fn colours_are_deterministic_and_bounded() {
        for kind in [
            Greenery::Grass,
            Greenery::Moss,
            Greenery::Leaf,
            Greenery::Conifer,
        ] {
            for season in 0..4 {
                let a = seasonal_colour(kind, GRASS, season);
                assert_eq!(a, seasonal_colour(kind, GRASS, season));
                assert!(a.iter().all(|c| (0.0..=1.0).contains(c)), "{kind:?} {a:?}");
            }
        }
        // Spring and summer are the authored colour.
        assert_eq!(seasonal_colour(Greenery::Grass, GRASS, 0), GRASS);
        assert_eq!(seasonal_colour(Greenery::Grass, GRASS, 1), GRASS);
        // Dormant winter grass is less green than summer grass.
        let winter = seasonal_colour(Greenery::Grass, GRASS, 3);
        assert!(winter[1] / winter[0] < GRASS[1] / GRASS[0] * 0.6);
        // Brighter variants stay brighter in winter.
        let light = seasonal_colour(Greenery::Grass, [0.09, 0.24, 0.04], 3);
        assert!(luminance(light) > luminance(winter));
        // Conifers stay green, only darker.
        let pine = seasonal_colour(Greenery::Conifer, [0.02, 0.08, 0.04], 3);
        assert!(pine[1] > pine[0] && pine[1] < 0.08);
    }

    #[test]
    fn only_greenery_materials_change() {
        assert_eq!(Greenery::of_material("grass.v04"), Some(Greenery::Grass));
        assert_eq!(Greenery::of_material("moss"), Some(Greenery::Moss));
        assert_eq!(
            Greenery::of_material("foliage.oak.v00"),
            Some(Greenery::Leaf)
        );
        assert_eq!(Greenery::of_material("stone"), None);
        assert_eq!(Greenery::of_material("dirt.v00"), None);
        let stone = Material::new([0.4, 0.4, 0.4], 0.9, 0.0);
        let grass = Material::new(GRASS, 0.9, 0.0);
        let mut table =
            SeasonalMaterials::new(vec![stone, grass], vec![None, Some(Greenery::Grass)]);
        let winter = table.update([0., 0., 0., 1.], 0.0).unwrap();
        assert_eq!(winter[0], stone);
        assert_ne!(winter[1].base_color, GRASS);
        assert_eq!(winter[1].roughness, grass.roughness);
    }

    #[test]
    fn a_season_change_uploads_once_per_step_and_settles() {
        let grass = Material::new(GRASS, 0.9, 0.0);
        let mut table = SeasonalMaterials::new(vec![grass], vec![Some(Greenery::Grass)]);
        let summer = [0., 1., 0., 0.];
        assert!(table.update(summer, 0.016).is_some(), "first frame uploads");
        assert!(
            table.update(summer, 0.016).is_none(),
            "unchanged: no upload"
        );
        // F10 to winter: eased over about a second, ends exactly at winter.
        let winter = [0., 0., 0., 1.];
        let mut uploads = 0;
        for _ in 0..90 {
            uploads += usize::from(table.update(winter, 1.0 / 60.0).is_some());
        }
        assert!(uploads > 1, "eased, not snapped");
        assert!(table.update(winter, 1.0 / 60.0).is_none(), "settled");
        let settled = table.uploaded.unwrap();
        assert_eq!(settled, winter);
    }
}
