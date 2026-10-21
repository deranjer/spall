//! Client-side drawing of player bodies: the `player.spvox` articulated asset,
//! posed on each player capsule, with the clip chosen from how the player moves.
//! Cubes come from the same [`AnimalVisual`] sampler the animals use.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use glam::{Quat, Vec3};
use spall_client::window::{PlayerBody, PlayerView};
use spall_core::MaterialId;
use spall_physics::CharacterParams;
use spall_render::CubeInstance;

use crate::animal_visual::{
    AnimalVisual, AnimalVisualError, MAX_ANIMAL_VISUAL_CELLS, material_mapping,
};

/// Ground speed above which a player walks rather than stands, m/s.
pub const WALK_MIN_SPEED_M_S: f32 = 0.3;
/// Ground speed above which a player runs rather than walks, m/s: midway
/// between the walk and run speeds of the movement kernel.
pub const RUN_MIN_SPEED_M_S: f32 = 5.3;
/// Ground covered by one walk clip cycle (two steps), metres.
pub const WALK_STRIDE_M: f32 = 2.1;
/// Ground covered by one run clip cycle, metres.
pub const RUN_STRIDE_M: f32 = 3.5;
/// The root pelvis pivot sits 13 cells of 6.25 cm above the shoe soles.
pub const PELVIS_ABOVE_SOLE_M: f32 = 13.0 * 0.0625;
/// Fastest the body turns toward its heading, 1/s (exponential approach).
const TURN_RATE_PER_S: f32 = 12.0;
/// Entries not drawn for this long are forgotten.
const FORGET_AFTER_S: f32 = 5.0;

/// The clip a player plays: swimming in water, else by ground speed.
pub fn select_clip(ground_speed_m_s: f32, in_water: bool) -> &'static str {
    if in_water {
        "swim"
    } else if ground_speed_m_s > RUN_MIN_SPEED_M_S {
        "run"
    } else if ground_speed_m_s > WALK_MIN_SPEED_M_S {
        "walk"
    } else {
        "idle"
    }
}

/// Where the root pelvis pivot goes for a capsule whose lowest point is
/// `feet_m`. Upright, the feet stand on the capsule bottom; swimming, the
/// prone body is centred on the capsule instead.
pub fn pelvis_position(feet_m: [f64; 3], in_water: bool) -> [f64; 3] {
    let up = if in_water {
        CharacterParams::DEFAULT.centre_offset_m()
    } else {
        PELVIS_ABOVE_SOLE_M
    };
    [feet_m[0], feet_m[1] + f64::from(up), feet_m[2]]
}

/// Rotation about `+Y` that turns the asset's `+X` facing toward the horizontal
/// direction `(dx, dz)`.
fn heading_yaw(dx: f32, dz: f32) -> f32 {
    (-dz).atan2(dx)
}

#[derive(Debug, Clone, Copy)]
struct Animation {
    /// Body yaw, radians about `+Y`.
    yaw: f32,
    /// Position in the current clip loop, `0..1`; kept across clip switches so
    /// the legs carry on from walk to run.
    cycle: f64,
    at: Instant,
}

#[derive(Debug)]
pub struct PlayerBodyView {
    visual: AnimalVisual,
    /// Distinct `(material, tint)` pairs the asset paints with.
    tints: Vec<(MaterialId, [u8; 3])>,
    states: Mutex<HashMap<u64, Animation>>,
}

impl PlayerBodyView {
    pub fn new() -> Result<Self, AnimalVisualError> {
        let asset = spall_asset::decode(include_bytes!("../assets/characters/player.spvox"))?;
        let mapping = material_mapping(&crate::game::manifest());
        let mut tints = Vec::new();
        for run in asset.runs.iter().filter(|run| run.tint_slot != 0) {
            let material = *mapping
                .get(&asset.material_keys[usize::from(run.material_slot) - 1])
                .ok_or_else(|| AnimalVisualError::Invalid("unmapped player material".into()))?;
            let pair = (material, asset.tints_srgb[usize::from(run.tint_slot) - 1]);
            if !tints.contains(&pair) {
                tints.push(pair);
            }
        }
        let visual = AnimalVisual::new(asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)?;
        Ok(Self {
            visual,
            tints,
            states: Mutex::new(HashMap::new()),
        })
    }

    /// Steps `body`'s animation to `now` and returns the clip, yaw and clip tick.
    fn advance(&self, body: &PlayerBody, now: Instant) -> (&'static str, f32, f64) {
        let [vx, _, vz] = body.velocity_m_s;
        let speed = vx.hypot(vz);
        let clip = select_clip(speed, body.in_water);
        let duration = f64::from(self.visual.clip_duration(clip).unwrap_or(1).max(1));
        let moving = speed > WALK_MIN_SPEED_M_S;
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        states.retain(|_, s| now.saturating_duration_since(s.at).as_secs_f32() < FORGET_AFTER_S);
        let heading = if moving {
            Some(heading_yaw(vx, vz))
        } else {
            body.look_yaw.map(|yaw| heading_yaw(yaw.sin(), -yaw.cos()))
        };
        let state = states.entry(body.entity).or_insert(Animation {
            yaw: heading.unwrap_or(0.0),
            cycle: 0.0,
            at: now,
        });
        let dt = now
            .saturating_duration_since(state.at)
            .as_secs_f32()
            .min(0.25);
        if let Some(target) = heading
            && moving
        {
            let mut delta = (target - state.yaw).rem_euclid(std::f32::consts::TAU);
            if delta > std::f32::consts::PI {
                delta -= std::f32::consts::TAU;
            }
            state.yaw += delta * (1.0 - (-TURN_RATE_PER_S * dt).exp());
        }
        let cycles_per_s = match clip {
            "run" => speed / RUN_STRIDE_M,
            "walk" => speed / WALK_STRIDE_M,
            "swim" => (0.5 + 0.5 * (speed / 3.2).min(1.0)) / (duration as f32 / 60.0),
            _ => 1.0 / (duration as f32 / 60.0),
        };
        state.cycle = (state.cycle + f64::from(cycles_per_s * dt)).rem_euclid(1.0);
        state.at = now;
        (clip, state.yaw, state.cycle * duration)
    }
}

impl PlayerView for PlayerBodyView {
    fn tints(&self) -> Vec<(MaterialId, [u8; 3])> {
        self.tints.clone()
    }

    fn instances(&self, players: &[PlayerBody], tint_base: u32, now: Instant) -> Vec<CubeInstance> {
        let mut out = Vec::new();
        for body in players {
            let (clip, yaw, tick) = self.advance(body, now);
            let Ok(cells) = self.visual.sample(clip, tick) else {
                continue;
            };
            let rotation = Quat::from_rotation_y(yaw);
            let [px, py, pz] = pelvis_position(body.feet_m, body.in_water);
            let origin = Vec3::new(px as f32, py as f32, pz as f32);
            out.extend(cells.iter().map(|cell| {
                let material = cell
                    .tint_srgb
                    .and_then(|tint| self.tints.iter().position(|t| *t == (cell.material, tint)))
                    .map_or(u32::from(cell.material.0), |i| tint_base + i as u32);
                CubeInstance::new(
                    (origin + rotation * cell.center).to_array(),
                    material,
                    cell.size.to_array(),
                    (rotation * cell.rotation).to_array(),
                )
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_follows_water_then_ground_speed() {
        assert_eq!(select_clip(0.0, false), "idle");
        assert_eq!(select_clip(0.1, false), "idle");
        assert_eq!(select_clip(4.0, false), "walk");
        assert_eq!(select_clip(6.5, false), "run");
        // The kernel's own walk and run speeds land on walk and run.
        use spall_physics::character::tuning::{RUN_SPEED_M_S, WALK_SPEED_M_S};
        assert_eq!(select_clip(WALK_SPEED_M_S, false), "walk");
        assert_eq!(select_clip(RUN_SPEED_M_S, false), "run");
        // Water wins over speed, standing or not.
        assert_eq!(select_clip(0.0, true), "swim");
        assert_eq!(select_clip(6.5, true), "swim");
    }

    #[test]
    fn standing_feet_rest_on_the_capsule_bottom_and_swimmers_centre_on_it() {
        let view = PlayerBodyView::new().unwrap();
        let feet = [10.0, 20.0, -5.0];
        // Lowest sole of the idle pose, relative to the pelvis pivot.
        let sole = view
            .visual
            .sample("idle", 0.0)
            .unwrap()
            .iter()
            .map(|c| c.center.y - c.size.y * 0.5)
            .fold(f32::INFINITY, f32::min);
        let pelvis = pelvis_position(feet, false);
        assert!(
            (pelvis[1] + f64::from(sole) - feet[1]).abs() < 1e-3,
            "soles at {} vs feet {}",
            pelvis[1] + f64::from(sole),
            feet[1]
        );
        assert_eq!(pelvis[0], feet[0]);
        assert_eq!(pelvis[2], feet[2]);
        let swim = pelvis_position(feet, true);
        let centre = f64::from(CharacterParams::DEFAULT.centre_offset_m());
        assert!((swim[1] - (feet[1] + centre)).abs() < 1e-9);
    }

    #[test]
    fn body_faces_its_heading_and_tints_resolve_to_table_entries() {
        let view = PlayerBodyView::new().unwrap();
        let now = Instant::now();
        let draw = |velocity: [f32; 3]| {
            view.instances(
                &[PlayerBody {
                    entity: 7,
                    feet_m: [0.0; 3],
                    velocity_m_s: velocity,
                    in_water: false,
                    look_yaw: None,
                }],
                1000,
                now,
            )
        };
        // Nose of the head: the cell farthest along the facing direction.
        let forward = |cubes: &[CubeInstance], dir: Vec3| {
            cubes
                .iter()
                .map(|c| Vec3::from_array(c.offset).dot(dir))
                .fold(f32::NEG_INFINITY, f32::max)
        };
        let along_x = draw([4.5, 0.0, 0.0]);
        assert!(forward(&along_x, Vec3::X) > forward(&along_x, -Vec3::X));
        view.states.lock().unwrap().clear();
        let along_z = draw([0.0, 0.0, 4.5]);
        assert!(forward(&along_z, Vec3::Z) > forward(&along_z, -Vec3::Z));
        let tints = view.tints().len() as u32;
        assert!(tints > 0);
        assert!(along_x.iter().all(|c| c.material < 1000 + tints));
        assert!(along_x.iter().any(|c| c.material >= 1000));
    }
}
