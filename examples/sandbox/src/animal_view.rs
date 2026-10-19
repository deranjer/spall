//! Client-side drawing of replicated animals: samples the authored SPVX clip
//! for each replicated record and emits lit cubes at the record's root pose.

use glam::{Quat, Vec3};
use spall_client::replica_creatures::ReplicaCreatures;
use spall_client::window::CreatureView;
use spall_protocol::CreatureLifecycleState;
use spall_render::CubeInstance;

use crate::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use crate::animals::{AnimalError, species};

#[derive(Debug)]
pub struct AnimalView {
    visuals: Vec<([u8; 16], AnimalVisual)>,
}

impl AnimalView {
    pub fn new() -> Result<Self, AnimalError> {
        let mapping = material_mapping(&crate::game::manifest());
        let visuals = species()?
            .into_iter()
            .map(|s| {
                let uuid = s.asset.uuid;
                AnimalVisual::new(s.asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)
                    .map(|visual| (uuid, visual))
                    .map_err(|e| AnimalError::Asset(s.key, e.to_string()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { visuals })
    }
}

impl CreatureView for AnimalView {
    fn instances(&self, creatures: &ReplicaCreatures, tick: f64) -> Vec<CubeInstance> {
        let mut out = Vec::new();
        for record in creatures.records() {
            let state = &record.state;
            let Some((_, visual)) = self
                .visuals
                .iter()
                .find(|(uuid, _)| *uuid == state.asset.uuid)
            else {
                continue;
            };
            let phase = match state.lifecycle {
                CreatureLifecycleState::Alive => tick + state.clip_phase_tick as f64,
                _ => state.clip_phase_tick as f64,
            };
            let duration = f64::from(visual.clip_duration(&state.clip_name).unwrap_or(1).max(1));
            let Ok(cells) = visual.sample(&state.clip_name, phase.rem_euclid(duration)) else {
                continue;
            };
            let [x, y, z, w] = state.root_rotation_xyzw;
            let root_q = Quat::from_xyzw(x, y, z, w).normalize();
            let root_t = Vec3::new(
                state.root_translation_m[0] as f32,
                state.root_translation_m[1] as f32,
                state.root_translation_m[2] as f32,
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
