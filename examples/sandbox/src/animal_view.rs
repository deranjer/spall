//! Client-side drawing of replicated animals: samples the authored SPVX clip
//! for each replicated record at its presented pose and emits lit cubes.

use std::time::Instant;

use glam::{Quat, Vec3};
use spall_client::replica_creatures::ReplicaCreatures;
use spall_client::window::CreatureView;
use spall_core::MaterialId;
use spall_render::CubeInstance;

use crate::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use crate::animals::{AnimalError, LOCOMOTION_IDLE, locomotion_clip, species};

#[derive(Debug)]
pub struct AnimalView {
    /// Species key, visual, and whether it is drawn in its palette colours.
    visuals: Vec<(&'static str, AnimalVisual, bool)>,
    /// Distinct `(material, tint)` pairs the tinted species paint with.
    tints: Vec<(MaterialId, [u8; 3])>,
    started: Instant,
}

impl AnimalView {
    pub fn new() -> Result<Self, AnimalError> {
        let mapping = material_mapping(&crate::game::manifest());
        let mut tints = Vec::new();
        let mut visuals = Vec::new();
        for s in species()? {
            let key = s.key;
            if s.tinted {
                for run in s.asset.runs.iter().filter(|run| run.tint_slot != 0) {
                    let Some(&material) =
                        mapping.get(&s.asset.material_keys[usize::from(run.material_slot) - 1])
                    else {
                        return Err(AnimalError::Asset(key, "unmapped material".into()));
                    };
                    let pair = (material, s.asset.tints_srgb[usize::from(run.tint_slot) - 1]);
                    if !tints.contains(&pair) {
                        tints.push(pair);
                    }
                }
            }
            let visual = AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)
                .map_err(|e| AnimalError::Asset(key, e.to_string()))?;
            visuals.push((key, visual, s.tinted));
        }
        Ok(Self {
            visuals,
            tints,
            started: Instant::now(),
        })
    }
}

impl CreatureView for AnimalView {
    fn tints(&self) -> Vec<(MaterialId, [u8; 3])> {
        self.tints.clone()
    }

    fn instances(
        &self,
        creatures: &ReplicaCreatures,
        _tick: f64,
        tint_base: u32,
        now: Instant,
    ) -> Vec<CubeInstance> {
        let mut out = Vec::new();
        for record in creatures.records() {
            let Some((species_key, visual, tinted)) =
                self.visuals.iter().find(|(key, ..)| *key == record.species)
            else {
                continue;
            };
            let presented = creatures.presentation(record, now);
            let clip = locomotion_clip(species_key, presented.locomotion_tag).unwrap_or("idle");
            let duration = f64::from(visual.clip_duration(clip).unwrap_or(1).max(1));
            // Resting animals idle on a local clock, offset per creature so a herd
            // does not breathe in step.
            let phase = if presented.locomotion_tag == LOCOMOTION_IDLE {
                now.saturating_duration_since(self.started).as_secs_f64() * 60.0
                    + (record.creature_id % 17) as f64 * 11.0
            } else {
                presented.clip_phase_tick
            };
            let Ok(cells) = visual.sample(clip, phase.rem_euclid(duration)) else {
                continue;
            };
            let [x, y, z, w] = presented.rotation_xyzw;
            let root_q = Quat::from_xyzw(x, y, z, w).normalize();
            let root_t = Vec3::new(
                presented.translation_m[0] as f32,
                presented.translation_m[1] as f32,
                presented.translation_m[2] as f32,
            );
            out.extend(cells.iter().map(|cell| {
                let material = cell
                    .tint_srgb
                    .filter(|_| *tinted)
                    .and_then(|tint| self.tints.iter().position(|t| *t == (cell.material, tint)))
                    .map_or(u32::from(cell.material.0), |i| tint_base + i as u32);
                CubeInstance::new(
                    (root_t + root_q * cell.center).to_array(),
                    material,
                    cell.size.to_array(),
                    (root_q * cell.rotation).to_array(),
                )
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only species marked `tinted` (wolves) are drawn in palette colours;
    /// the rest keep their materials' own colours.
    #[test]
    fn only_tinted_species_get_palette_materials() {
        let view = AnimalView::new().unwrap();
        let wolf = species()
            .unwrap()
            .into_iter()
            .find(|s| s.key == "wolf")
            .unwrap();
        assert!(wolf.tinted);
        assert!(!view.tints.is_empty());
        for (_, tint) in &view.tints {
            assert!(
                wolf.asset.tints_srgb.contains(tint),
                "{tint:?} is not a wolf colour"
            );
        }
    }
}
