//! Client-side drawing of replicated animals: samples the authored SPVX clip
//! for each replicated record at its presented pose and emits lit cubes.

use std::time::Instant;

use glam::{Quat, Vec3};
use spall_client::replica_creatures::ReplicaCreatures;
use spall_client::window::CreatureView;
use spall_render::CubeInstance;

use crate::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use crate::animals::{AnimalError, LOCOMOTION_IDLE, locomotion_clip, species};

#[derive(Debug)]
pub struct AnimalView {
    visuals: Vec<(&'static str, AnimalVisual)>,
    started: Instant,
}

impl AnimalView {
    pub fn new() -> Result<Self, AnimalError> {
        let mapping = material_mapping(&crate::game::manifest());
        let visuals = species()?
            .into_iter()
            .map(|s| {
                let key = s.key;
                AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)
                    .map(|visual| (key, visual))
                    .map_err(|e| AnimalError::Asset(key, e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            visuals,
            started: Instant::now(),
        })
    }
}

impl CreatureView for AnimalView {
    fn instances(
        &self,
        creatures: &ReplicaCreatures,
        _tick: f64,
        now: Instant,
    ) -> Vec<CubeInstance> {
        let mut out = Vec::new();
        for record in creatures.records() {
            let Some((species_key, visual)) =
                self.visuals.iter().find(|(key, _)| *key == record.species)
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
                CubeInstance::new(
                    (root_t + root_q * cell.center).to_array(),
                    u32::from(cell.material.0),
                    cell.size.to_array(),
                    (root_q * cell.rotation).to_array(),
                )
            }));
        }
        out
    }
}
