//! Per-frame footsteps: counts each walking player's stride, finds the surface
//! under the foot in the replica, and hands the step to the footstep audio.

use super::*;
use crate::footsteps::{FootstepEvent, heard_step};

/// Furthest another player's step is looked up (metres); beyond it is silent.
const HEARD_RANGE_M: f64 = 32.0;

/// The solid cell a foot at `feet_m` stands on and its material: the cell just
/// below the sole, or the one under that where the capsule floats a skin above.
fn ground_below(volume: &Volume, feet_m: [f64; 3]) -> Option<(GlobalCell, MaterialId)> {
    let cell_m = f64::from(CELL_M);
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

/// Depth (metres) of the thin snow layer on the face of `ground`, 0 if none.
fn snow_depth_m(replica: &ReplicaWorld, ground: GlobalCell) -> f32 {
    let tile = i64::from(spall_protocol::snow::SNOW_TILE_CELLS);
    let Ok(top_y) = i32::try_from(ground.y) else {
        return 0.0;
    };
    let Some(frame) = replica.snow_frame(
        ground.x.div_euclid(tile) as i32,
        ground.z.div_euclid(tile) as i32,
        top_y,
    ) else {
        return 0.0;
    };
    let face = (ground.z.rem_euclid(tile) * tile + ground.x.rem_euclid(tile)) as usize;
    frame
        .surfaces
        .iter()
        .find(|surface| (surface.exposed_faces[face / 64] >> (face % 64)) & 1 == 1)
        .map_or(0.0, |surface| f32::from(surface.depth_q16) / 65_536.0)
}

struct Walker {
    id: u64,
    feet_m: [f64; 3],
    velocity_m_s: [f32; 3],
    /// `None`: not reported, so the terrain below decides.
    grounded: Option<bool>,
    swimming: bool,
}

impl InteractiveApp {
    pub(super) fn update_footsteps(&mut self, now: Instant) {
        let dt = self.footstep_at.map_or(0.0, |at| {
            now.saturating_duration_since(at).as_secs_f32().min(0.1)
        });
        self.footstep_at = Some(now);
        let Some(audio) = &self.footsteps else {
            return;
        };
        audio.set_gain(self.footstep_volume);
        let water = self
            .session
            .column_surface
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .query();
        let mut walkers = Vec::new();
        let mut listener = None;
        if self.fly_eye.is_none()
            && let Some(view) = self
                .session
                .view
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            let feet_m = view.predicted.position_m;
            listener = Some(feet_m);
            walkers.push(Walker {
                id: 0,
                feet_m,
                velocity_m_s: view.predicted.velocity_m_s,
                grounded: Some(view.predicted.grounded),
                swimming: player_swims(&water, feet_m),
            });
        }
        walkers.extend(self.last_remote_players.iter().map(|player| Walker {
            id: player.entity,
            feet_m: player.feet_m,
            velocity_m_s: player.velocity_m_s,
            grounded: None,
            swimming: player.in_water,
        }));
        self.footstep_trackers
            .retain(|id, _| walkers.iter().any(|walker| walker.id == *id));
        // A frame that finds the replica busy skips its steps; strides carry on.
        let Some(replica) = self.session.replica.get().and_then(|r| r.try_lock().ok()) else {
            return;
        };
        let Some(volume) = replica.terrain_volume() else {
            return;
        };
        let config = &self.biome_audio.footsteps;
        let (right_x, right_z) = (f64::from(self.yaw.cos()), f64::from(self.yaw.sin()));
        for walker in &walkers {
            let [vx, vy, vz] = walker.velocity_m_s;
            let offset = listener.map(|l| [walker.feet_m[0] - l[0], walker.feet_m[2] - l[2]]);
            // Another player is heard only when near enough to matter.
            if walker.id != 0 && offset.is_none_or(|o| o[0].hypot(o[1]) > HEARD_RANGE_M) {
                self.footstep_trackers.remove(&walker.id);
                continue;
            }
            let ground = ground_below(volume, walker.feet_m);
            let grounded = !walker.swimming
                && walker
                    .grounded
                    .unwrap_or_else(|| ground.is_some() && vy.abs() < 2.0);
            let tracker = self.footstep_trackers.entry(walker.id).or_default();
            let Some(footfall) = tracker.advance(config, dt, vx.hypot(vz), grounded) else {
                continue;
            };
            let snow = ground.map_or(0.0, |(cell, _)| snow_depth_m(&replica, cell));
            let in_water = water
                .sample_at_m(walker.feet_m[0], walker.feet_m[1] + 0.05, walker.feet_m[2])
                .is_some();
            let surface =
                config.select_surface(ground.map(|(_, material)| material.0), snow, in_water);
            let (gain, pan) = match offset {
                Some(o) if walker.id != 0 => {
                    let distance = o[0].hypot(o[1]) as f32;
                    let right = (o[0] * right_x + o[1] * right_z) as f32;
                    let Some(heard) = heard_step(distance, right) else {
                        continue;
                    };
                    heard
                }
                _ => (1.0, 0.0),
            };
            if self.footstep_log {
                tracing::info!(
                    player = walker.id,
                    surface = config
                        .surfaces
                        .get(surface)
                        .map_or("?", |s| s.name.as_str()),
                    material = ground.map(|(_, material)| material.0),
                    snow_depth_m = snow,
                    running = footfall.running,
                    "footfall"
                );
            }
            if !audio.play(FootstepEvent {
                surface,
                gain,
                pan,
                running: footfall.running,
            }) {
                tracing::warn!("footstep worker stopped; continuing without footsteps");
                self.footsteps = None;
                return;
            }
        }
    }
}
