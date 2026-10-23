//! Bounded, deterministic weather presentation derived from the authoritative
//! weather snapshot and an immutable resident voxel view. No weather state is
//! persisted here; particles are camera-local cosmetics.

use glam::Vec3;
use spall_core::GlobalCell;
use spall_protocol::{WeatherKind, WeatherState, Wind, wind_at};
use spall_render::{Camera, WeatherLighting, WeatherParticle};
use spall_voxel::{Sample, Volume};

/// Hard cap keeps effect cost independent of world size and weather intensity.
pub const MAX_WEATHER_PARTICLES: usize = spall_render::MAX_WEATHER_PARTICLES;
/// Particles per unit of precipitation (0..1). Heavy snow (0.85) is about 7k and the
/// blizzard (0.95) 1.5x that; rain keeps its older, sparser streaks.
const RAIN_PER_PRECIP: f32 = 1_920.0;
const SNOW_PER_PRECIP: f32 = 8_200.0;
const BLIZZARD_PER_PRECIP: f32 = 11_050.0;
/// Snow falls in a tight 24 m box around the camera; rain keeps the wide one.
const SNOW_HALF_BOX_M: f32 = 12.0;
const RAIN_HALF_BOX_M: f32 = 20.0;
const GRID_SIDE: usize = 5;
const GRID_RADIUS_M: f32 = 20.0;
/// Blizzard fog ends this far from the camera at full whiteout.
const WHITEOUT_FOG_END_M: f32 = 22.0;
const WHITEOUT_VISIBILITY: f32 = 0.15;
const SKY_SCAN_M: f32 = 8.0;

/// Borrowed output for this frame. `exposure` is also the shelter signal audio
/// should consume; unknown occupancy is deliberately counted as sheltered.
#[derive(Debug, Clone, Copy)]
pub struct WeatherFrame<'a> {
    pub particles: &'a [WeatherParticle],
    pub lighting: WeatherLighting,
}

/// Local interpolation state. Reset this on world/replica reset; a teleport is
/// detected automatically and clears stale camera-local particles.
#[derive(Debug)]
pub struct WeatherPresentation {
    particles: Vec<WeatherParticle>,
    last_camera: Option<Vec3>,
    last_epoch: Option<u64>,
    phase_seconds: f64,
    /// The snapshot clock the wind schedule is anchored to, and local seconds since
    /// that snapshot arrived. Unlike `phase_seconds` they survive a teleport.
    clock_anchor_ms: Option<u64>,
    clock_since_s: f64,
    /// Latest wind strength (0..1) from the shared schedule, for audio.
    wind_strength: f32,
    /// Camera eye is below the water surface; `submerge` eases toward it.
    submerged: bool,
    submerge: f32,
    /// Integrated wind drift of the flake lattice, metres (x, z).
    drift: [f64; 2],
    exposure_cache_key: Option<(i64, i64, i64, u64)>,
    exposure_grid: [bool; GRID_SIDE * GRID_SIDE],
    precipitation: f32,
    wind: f32,
    exposure: f32,
    enabled: bool,
    reduced_flashes: bool,
}

impl Default for WeatherPresentation {
    fn default() -> Self {
        Self {
            particles: Vec::new(),
            last_camera: None,
            last_epoch: None,
            phase_seconds: 0.0,
            clock_anchor_ms: None,
            clock_since_s: 0.0,
            wind_strength: 0.0,
            submerged: false,
            submerge: 0.0,
            drift: [0.0; 2],
            exposure_cache_key: None,
            exposure_grid: [false; GRID_SIDE * GRID_SIDE],
            precipitation: 0.0,
            wind: 0.0,
            exposure: 0.0,
            enabled: true,
            reduced_flashes: true,
        }
    }
}

impl WeatherPresentation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle local weather effects. Disabled effects return neutral lighting
    /// and no particles, without changing authoritative or audio weather state.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.particles.clear();
            self.precipitation = 0.0;
            self.wind = 0.0;
        }
    }

    /// Whether the camera eye is under water. Weather effects fade out while it
    /// is, and come back as it surfaces.
    pub fn set_submerged(&mut self, submerged: bool) {
        self.submerged = submerged;
    }

    /// 0 above water to 1 fully submerged (eased); audio muffles with it.
    pub fn submerge(&self) -> f32 {
        self.submerge
    }

    /// Wind strength 0..1 of the shared schedule at the last update.
    pub fn wind_strength(&self) -> f32 {
        self.wind_strength
    }

    pub fn set_reduced_flashes(&mut self, reduced: bool) {
        self.reduced_flashes = reduced;
    }

    pub fn reset(&mut self) {
        self.particles.clear();
        self.last_camera = None;
        self.last_epoch = None;
        self.phase_seconds = 0.0;
        self.clock_anchor_ms = None;
        self.clock_since_s = 0.0;
        self.wind_strength = 0.0;
        self.submerge = 0.0;
        self.drift = [0.0; 2];
        self.exposure_cache_key = None;
        self.exposure_grid = [false; GRID_SIDE * GRID_SIDE];
        self.precipitation = 0.0;
        self.wind = 0.0;
        self.exposure = 0.0;
    }

    /// Advance smooth presentation and rebuild at most [`MAX_WEATHER_PARTICLES`]
    /// deterministic ribbons. Exposure uses only a 5x5 set of sky columns and
    /// at most 8 m of vertical committed-voxel samples per column.
    pub fn update<'a>(
        &'a mut self,
        state: &WeatherState,
        camera: &Camera,
        dt_seconds: f32,
        volume: &Volume,
    ) -> WeatherFrame<'a> {
        let position = camera.position;
        let had_camera = self.last_camera.is_some();
        let epoch_changed = self
            .last_epoch
            .is_some_and(|epoch| epoch != state.world_epoch);
        let teleported = self
            .last_camera
            .is_some_and(|last| last.distance(position) > GRID_RADIUS_M * 2.0);
        if teleported || epoch_changed {
            self.particles.clear();
            self.phase_seconds = 0.0;
            self.drift = [0.0; 2];
        }
        self.last_camera = Some(position);
        self.last_epoch = Some(state.world_epoch);
        self.phase_seconds += f64::from(dt_seconds.clamp(0.0, 0.1));
        if self.clock_anchor_ms != Some(state.clock_ms) {
            self.clock_anchor_ms = Some(state.clock_ms);
            self.clock_since_s = 0.0;
        } else {
            self.clock_since_s += f64::from(dt_seconds.clamp(0.0, 0.1));
        }
        let ease = 1.0 - (-dt_seconds.clamp(0.0, 0.1) * 4.0).exp();
        let submerge_target = if self.submerged { 1.0 } else { 0.0 };
        self.submerge += (submerge_target - self.submerge) * ease;
        let fade = 1.0 - self.submerge;
        let cell_m = volume.cell_size().metres() as f32;
        let cache_key = (
            (position.x / cell_m).floor() as i64,
            (position.y / cell_m).floor() as i64,
            (position.z / cell_m).floor() as i64,
            volume.state_stamp(),
        );
        if self.exposure_cache_key != Some(cache_key) {
            self.exposure_grid = sky_exposure_grid(position, volume);
            self.exposure_cache_key = Some(cache_key);
        }
        let exposed_grid = self.exposure_grid;
        let raw_exposure = if exposed_grid[(GRID_SIDE / 2) * GRID_SIDE + GRID_SIDE / 2] {
            1.0
        } else {
            0.0
        };
        if had_camera && !teleported && !epoch_changed {
            let blend = 1.0 - (-dt_seconds.clamp(0.0, 0.1) * 5.0).exp();
            self.exposure += (raw_exposure - self.exposure) * blend;
        } else {
            self.exposure = raw_exposure;
        }
        let exposure = self.exposure;
        if !self.enabled || state.validate().is_err() {
            self.particles.clear();
            self.precipitation = approach(self.precipitation, 0.0, dt_seconds);
            self.wind = approach(self.wind, 0.0, dt_seconds);
            return WeatherFrame {
                particles: &self.particles,
                lighting: WeatherLighting {
                    exposure,
                    precipitation: 0.0,
                    wind: 0.0,
                    visibility: 1.0,
                    storm_dimming: 0.0,
                    lightning_flash: 0.0,
                },
            };
        }

        let raw_precip = if matches!(
            state.kind,
            WeatherKind::Rain | WeatherKind::Storm | WeatherKind::Snow | WeatherKind::Blizzard
        ) {
            f32::from(state.precipitation_q16) / f32::from(u16::MAX)
        } else {
            0.0
        };
        let raw_wind = f32::from(state.gust_q16) / f32::from(u16::MAX);
        let smoothing = (1.0 - (-dt_seconds.clamp(0.0, 0.1) * 2.5).exp()).clamp(0.0, 1.0);
        if teleported {
            self.precipitation = raw_precip;
            self.wind = raw_wind;
        } else {
            self.precipitation += (raw_precip - self.precipitation) * smoothing;
            self.wind += (raw_wind - self.wind) * smoothing;
        }

        let snow = matches!(state.kind, WeatherKind::Snow | WeatherKind::Blizzard);
        let blizzard = state.kind == WeatherKind::Blizzard;
        let storm = state.kind == WeatherKind::Storm;
        // One shared schedule of the weather seed and clock: every client (and the
        // server) sees the same heading and strength at the same moment.
        let wind_now = wind_at(
            state.weather_seed,
            state.clock_ms as f64 / 1000.0 + self.clock_since_s,
        );
        self.wind_strength = wind_now.strength;
        let pulse = wind_now.strength;
        let desired = if fade < 0.02 {
            0
        } else if self.precipitation > 0.005 {
            let per_precip = match state.kind {
                WeatherKind::Blizzard => BLIZZARD_PER_PRECIP,
                WeatherKind::Snow => SNOW_PER_PRECIP,
                _ => RAIN_PER_PRECIP,
            };
            (self.precipitation * per_precip).ceil() as usize
        } else if state.kind == WeatherKind::HighWind && self.wind > 0.08 {
            (self.wind * 180.0).ceil() as usize
        } else {
            0
        }
        .min(MAX_WEATHER_PARTICLES);
        let wind_direction = Vec3::new(wind_now.direction[0], 0.0, wind_now.direction[1]);
        let is_wind_cue = state.kind == WeatherKind::HighWind && self.precipitation <= 0.005;
        let velocity = precipitation_velocity(state.kind, wind_direction, &wind_now);
        let dt = f64::from(dt_seconds.clamp(0.0, 0.1));
        self.drift[0] += f64::from(velocity.x) * dt;
        self.drift[1] += f64::from(velocity.z) * dt;
        let fall_speed = -velocity.y;
        let half = if snow {
            SNOW_HALF_BOX_M
        } else {
            RAIN_HALF_BOX_M
        };
        let span = f64::from(half * 2.0);
        // Vertical window of the falling column: snow stays dense around the camera.
        let (fall_height, below) = if snow { (12.0f32, 2.5f32) } else { (24.0, 4.0) };
        let direction = velocity.normalize_or_zero().to_array();
        let cam = [
            f64::from(position.x),
            f64::from(position.y),
            f64::from(position.z),
        ];
        self.particles.clear();
        self.particles
            .reserve(desired.saturating_sub(self.particles.capacity()));
        for index in 0..desired {
            let h = hash(state.weather_seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let h2 = hash(h);
            let h3 = hash(h2);
            // Flakes live on a world-anchored periodic lattice wrapped around the camera,
            // so walking through them looks like walking, not like carrying a snow globe.
            let mut wx = f64::from(field(h, 0)) * span + self.drift[0];
            let mut wz = f64::from(field(h, 16)) * span + self.drift[1];
            let (speed, size) = if snow && !blizzard {
                // 1.0-1.5 m/s fall, 8-12 cm flakes, a gentle sway.
                let speed = 1.0 + 0.5 * field(h, 48);
                let t = self.phase_seconds;
                let amp = f64::from(0.15 + 0.2 * field(h2, 0));
                let w = f64::from(0.7 + 0.9 * field(h2, 16));
                wx += amp * (t * w + f64::from(field(h2, 32)) * std::f64::consts::TAU).sin();
                wz += amp * (t * w * 0.8 + f64::from(field(h2, 48)) * std::f64::consts::TAU).cos();
                (speed, 0.08 + 0.04 * field(h3, 0))
            } else {
                if blizzard {
                    // Turbulence: each streak swirls on its own slow circle.
                    let t = self.phase_seconds;
                    let amp =
                        f64::from(0.3 + 0.6 * field(h2, 0)) * f64::from(0.4 + wind_now.strength);
                    let w = f64::from(0.8 + 1.6 * field(h2, 16));
                    wx += amp * (t * w + f64::from(field(h2, 32)) * std::f64::consts::TAU).sin();
                    wz += amp
                        * (t * w * 0.9 + f64::from(field(h2, 48)) * std::f64::consts::TAU).cos();
                }
                (fall_speed, 0.0)
            };
            let side_x = (wx - cam[0]).rem_euclid(span) as f32 - half;
            let side_z = (wz - cam[2]).rem_euclid(span) as f32 - half;
            let y = if fall_speed > 0.0 {
                let column =
                    f64::from(field(h, 32) * fall_height) - self.phase_seconds * f64::from(speed);
                position.y + (column - cam[1]).rem_euclid(f64::from(fall_height)) as f32 - below
            } else {
                position.y + (field(h, 32) * 16.0 - 8.0)
            };
            let xz = Vec3::new(position.x + side_x, 0.0, position.z + side_z);
            if !grid_open_at(position, xz, &exposed_grid) {
                continue;
            }
            let centre = Vec3::new(position.x + side_x, y, position.z + side_z);
            // Flakes right at the lens would fill the screen: fade them out inside ~2 m.
            let near = if snow {
                let d = centre.distance(position);
                ((d - 0.6) / 1.6).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let colour = if is_wind_cue {
                [0.72, 0.78, 0.81, 0.24 * fade]
            } else if blizzard {
                [0.92, 0.96, 1.0, 0.7 * near * fade]
            } else if snow {
                [0.92, 0.96, 1.0, 0.85 * near * fade]
            } else {
                [0.58, 0.72, 0.9, if storm { 0.64 } else { 0.48 } * fade]
            };
            let (length_m, width_m) = if is_wind_cue {
                (0.8, 0.07)
            } else if blizzard {
                (0.2 + velocity.x.hypot(velocity.z) * 0.02, 0.04)
            } else if snow {
                (size, size)
            } else {
                (0.9 + self.wind * 0.45, 0.018)
            };
            self.particles.push(WeatherParticle {
                centre: centre.to_array(),
                direction,
                length_m,
                width_m,
                colour_rgba: colour,
            });
        }

        let lightning_age_ticks = state.effective_tick.saturating_sub(state.lightning_tick);
        const FLASH_TICKS: u64 = 11; // about 0.18 s at 60 Hz
        let flash = if storm && state.lightning_id != 0 && lightning_age_ticks < FLASH_TICKS {
            (1.0 - lightning_age_ticks as f32 / FLASH_TICKS as f32) * 0.08 * exposure * fade
        } else {
            0.0
        };
        let lighting = WeatherLighting {
            exposure,
            precipitation: self.precipitation * fade,
            wind: self.wind * fade,
            visibility: 1.0
                - (1.0 - snow_visibility(blizzard, self.precipitation, pulse, exposure)) * fade,
            storm_dimming: if storm {
                self.precipitation * 0.22 * exposure * fade
            } else {
                0.0
            },
            lightning_flash: if self.reduced_flashes { 0.0 } else { flash },
        };
        WeatherFrame {
            particles: &self.particles,
            lighting,
        }
    }
}

/// World-space velocity shared by particle drift and the visible ribbon axis.
/// Normal precipitation remains distinctly downward; blizzards are driven
/// nearly flat along the wind in gusts and fall almost straight in lulls, and
/// wind-only cues stay horizontal.
fn precipitation_velocity(kind: WeatherKind, direction: Vec3, wind: &Wind) -> Vec3 {
    let drift = wind.speed_mps(kind);
    let fall = match kind {
        WeatherKind::Rain | WeatherKind::Storm => 13.0,
        WeatherKind::Snow => 1.25,
        WeatherKind::Blizzard => 1.0,
        WeatherKind::Clear | WeatherKind::HighWind => 0.0,
    };
    Vec3::new(direction.x * drift, -fall, direction.z * drift)
}

/// Camera visibility: heavy snow thins the air somewhat, a blizzard thins in lulls (about
/// 0.6) and closes to `WHITEOUT_VISIBILITY` in strong gusts (`pulse` is the wind strength). Sheltered cameras see clear air.
fn snow_visibility(blizzard: bool, precipitation: f32, pulse: f32, exposure: f32) -> f32 {
    let loss = if blizzard {
        let severity = (precipitation / 0.95).clamp(0.0, 1.0);
        (1.0 - WHITEOUT_VISIBILITY) * severity * (0.5 + 0.5 * pulse)
    } else {
        // Light and medium (0.5) snow stay clear; heavy (0.85) loses 0.3.
        0.3 * ((precipitation - 0.5) / 0.35).clamp(0.0, 1.0)
    };
    1.0 - loss * exposure
}

/// Distance fog for the current visibility: `(start_m, end_m, whiteout)`. Clear air keeps
/// the normal range; thinning visibility pulls the end in geometrically toward
/// [`WHITEOUT_FOG_END_M`], starts the fade almost at the camera and whitens the colour.
pub fn weather_fog(visibility: f32, normal_start_m: f32, normal_end_m: f32) -> (f32, f32, f32) {
    let t = ((1.0 - visibility) / (1.0 - WHITEOUT_VISIBILITY)).clamp(0.0, 1.0);
    if t <= 0.0 || normal_end_m <= WHITEOUT_FOG_END_M {
        return (normal_start_m, normal_end_m, 0.0);
    }
    let end = normal_end_m * (WHITEOUT_FOG_END_M / normal_end_m).powf(t);
    let ratio = normal_start_m / normal_end_m;
    let start = end * (ratio + (0.05 - ratio) * t);
    (start, end, t)
}

fn sky_exposure_grid(camera: Vec3, volume: &Volume) -> [bool; GRID_SIDE * GRID_SIDE] {
    std::array::from_fn(|index| column_open(camera, volume, index % GRID_SIDE, index / GRID_SIDE))
}

fn grid_open_at(camera: Vec3, world: Vec3, grid: &[bool; GRID_SIDE * GRID_SIDE]) -> bool {
    let x = (((world.x - camera.x + GRID_RADIUS_M) / (2.0 * GRID_RADIUS_M))
        * (GRID_SIDE - 1) as f32)
        .round()
        .clamp(0.0, (GRID_SIDE - 1) as f32) as usize;
    let z = (((world.z - camera.z + GRID_RADIUS_M) / (2.0 * GRID_RADIUS_M))
        * (GRID_SIDE - 1) as f32)
        .round()
        .clamp(0.0, (GRID_SIDE - 1) as f32) as usize;
    grid[x + z * GRID_SIDE]
}

fn column_open(camera: Vec3, volume: &Volume, x: usize, z: usize) -> bool {
    let cell_m = volume.cell_size().metres() as f32;
    let wx = camera.x + (x as f32 / (GRID_SIDE - 1) as f32 * 2.0 - 1.0) * GRID_RADIUS_M;
    let wz = camera.z + (z as f32 / (GRID_SIDE - 1) as f32 * 2.0 - 1.0) * GRID_RADIUS_M;
    let cx = (wx / cell_m).floor() as i64;
    let cz = (wz / cell_m).floor() as i64;
    let start_y = (camera.y / cell_m).floor() as i64;
    let mut cells = (SKY_SCAN_M / cell_m).ceil() as i64;
    if let Some(bounds) = volume.bounds() {
        let Some(top) = bounds.max.y.checked_add(1).and_then(|y| y.checked_mul(32)) else {
            return false;
        };
        if start_y >= top {
            return false;
        }
        cells = cells.min(top - start_y);
    }
    for dy in 0..cells {
        match volume.sample(GlobalCell::new(cx, start_y + dy, cz)) {
            Ok(Sample::Empty { .. }) => {}
            Ok(Sample::Filled(_)) => return false,
            Ok(Sample::Unknown(_)) | Err(_) => return false,
        }
    }
    true
}

fn approach(value: f32, target: f32, dt: f32) -> f32 {
    value + (target - value) * (1.0 - (-dt.clamp(0.0, 0.1) * 2.5).exp())
}

fn hash(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A 16-bit field of a hash as 0..1.
fn field(value: u64, shift: u32) -> f32 {
    ((value >> shift) & 0xFFFF) as f32 / 65_535.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, VolumeId};
    use spall_protocol::{WeatherClock, WeatherClockSource};
    use spall_voxel::{Brick, EditPlan};

    fn state(kind: WeatherKind, precipitation: u16) -> WeatherState {
        let mut s = WeatherState::initial(
            99,
            1,
            WeatherClock {
                time_ms: 50,
                source: WeatherClockSource::ServerFallback,
                start_season: 0,
                season_ms: 10_000,
            },
        );
        s.kind = kind;
        s.precipitation_q16 = precipitation;
        s.gust_q16 = 50_000;
        s.wind_x_q15 = 20_000;
        s.transition_id = 0;
        s.transition_start_ms = 0;
        s
    }

    fn known_air() -> Volume {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        for z in -2..=2 {
            for y in 0..=1 {
                for x in -2..=2 {
                    v.insert_brick(spall_core::BrickCoord::new(x, y, z), Brick::empty())
                        .unwrap();
                }
            }
        }
        v
    }

    fn camera() -> Camera {
        Camera::looking_along(Vec3::new(0.0, 1.0, 0.0), Vec3::NEG_Z, 1.0, 1.0)
    }

    #[test]
    fn particle_population_is_deterministic_bounded_and_weather_specific() {
        let v = known_air();
        let mut a = WeatherPresentation::new();
        let mut b = WeatherPresentation::new();
        let state = state(WeatherKind::Blizzard, u16::MAX);
        let aa = a
            .update(&state, &camera(), 1.0 / 60.0, &v)
            .particles
            .to_vec();
        let bb = b
            .update(&state, &camera(), 1.0 / 60.0, &v)
            .particles
            .to_vec();
        assert_eq!(aa, bb);
        assert!(!aa.is_empty());
        assert!(aa.len() <= MAX_WEATHER_PARTICLES);
        assert!(
            aa.iter()
                .all(|p| p.length_m.is_finite() && p.length_m > 0.0)
        );
    }

    #[test]
    fn precipitation_populates_all_horizontal_quadrants_instead_of_one_diagonal() {
        let volume = known_air();
        let mut presentation = WeatherPresentation::new();
        let weather = state(WeatherKind::Blizzard, u16::MAX);
        for _ in 0..60 {
            presentation.update(&weather, &camera(), 1.0 / 60.0, &volume);
        }
        let frame = presentation.update(&weather, &camera(), 1.0 / 60.0, &volume);
        let mut quadrants = [0usize; 4];
        for particle in frame.particles {
            let quadrant =
                usize::from(particle.centre[0] >= 0.0) + 2 * usize::from(particle.centre[2] >= 0.0);
            quadrants[quadrant] += 1;
        }
        assert!(quadrants.iter().all(|&count| count > 30), "{quadrants:?}");
    }

    #[test]
    fn unknown_and_roofed_columns_are_sheltered_but_open_columns_remain_exposed() {
        let empty = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        let grid = sky_exposure_grid(Vec3::ZERO, &empty);
        assert_eq!(grid.iter().filter(|&&open| open).count(), 0);
        let mut presentation = WeatherPresentation::new();
        assert_eq!(
            presentation
                .update(&state(WeatherKind::Rain, 30_000), &camera(), 0.02, &empty)
                .lighting
                .exposure,
            0.0,
            "unknown volume samples are sheltered"
        );
        presentation.reset();
        let mut v = known_air();
        assert_eq!(
            presentation
                .update(&state(WeatherKind::Rain, 30_000), &camera(), 0.02, &v)
                .lighting
                .exposure,
            1.0,
            "resident air with no roof is exposed"
        );
        presentation.reset();
        let cell = GlobalCell::new(0, 8, 0);
        let mut plan = EditPlan::new(v.id());
        plan.set(cell, spall_core::MaterialId(1));
        v.apply_edit(&plan).unwrap();
        assert!(!column_open(Vec3::new(0.0, 1.0, 0.0), &v, 2, 2));
        assert!(column_open(Vec3::new(0.0, 1.0, 0.0), &v, 1, 2));
        assert_eq!(
            presentation
                .update(&state(WeatherKind::Rain, 30_000), &camera(), 0.02, &v)
                .lighting
                .exposure,
            0.0,
            "the camera's roofed sample controls audio shelter and fog"
        );
    }

    #[test]
    fn wind_without_precipitation_has_a_visible_bounded_cue_and_toggle_clears_it() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let frame = p.update(&state(WeatherKind::HighWind, 0), &camera(), 0.1, &v);
        assert!(!frame.particles.is_empty());
        assert!(frame.lighting.wind > 0.0);
        p.set_enabled(false);
        let frame = p.update(&state(WeatherKind::Storm, u16::MAX), &camera(), 0.1, &v);
        assert!(frame.particles.is_empty());
        assert_eq!(frame.lighting.precipitation, 0.0);
    }

    #[test]
    fn reduced_flashes_suppresses_lightning_and_teleport_restarts_camera_local_effects() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let mut s = state(WeatherKind::Storm, 20_000);
        s.lightning_id = 1;
        s.lightning_tick = 1;
        let first_particles = p.update(&s, &camera(), 0.1, &v).particles.to_vec();
        let first = p.update(&s, &camera(), 0.1, &v);
        assert_eq!(first.lighting.lightning_flash, 0.0);
        p.set_reduced_flashes(false);
        let second = p.update(&s, &camera(), 0.1, &v);
        assert!(second.lighting.lightning_flash <= 0.08);
        let distant = Camera::looking_along(Vec3::splat(200.0), Vec3::NEG_Z, 1.0, 1.0);
        assert!(p.update(&s, &distant, 0.1, &v).particles.is_empty());
        assert!(
            first_particles
                .iter()
                .all(|particle| particle.centre[0].abs() < 25.0)
        );
    }

    #[test]
    fn fixed_authoritative_snapshot_still_animates_and_clear_wind_direction_is_not_a_gust() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let mut snow = state(WeatherKind::Snow, 40_000);
        snow.gust_q16 = 0;
        let first = p.update(&snow, &camera(), 0.0, &v).particles.to_vec();
        let second = p.update(&snow, &camera(), 0.1, &v).particles.to_vec();
        assert_ne!(
            first, second,
            "visual motion advances between server snapshots"
        );

        let mut clear = state(WeatherKind::Clear, 0);
        clear.gust_q16 = 0;
        clear.wind_x_q15 = i16::MAX;
        let mut frame = p.update(&clear, &camera(), 0.1, &v);
        for _ in 0..40 {
            frame = p.update(&clear, &camera(), 0.1, &v);
        }
        assert_eq!(frame.lighting.wind, 0.0);
        assert!(frame.particles.is_empty());
    }

    #[test]
    fn precipitation_velocity_matches_downward_fall_and_wind_drift() {
        let cardinal = Vec3::X;
        let diagonal = Vec3::new(1.0, 0.0, 1.0).normalize();
        let gust = Wind {
            direction: [1.0, 0.0],
            strength: 0.9,
        };
        let lull = Wind {
            strength: 0.0,
            ..gust
        };
        let rain = precipitation_velocity(WeatherKind::Rain, cardinal, &gust);
        let snow = precipitation_velocity(WeatherKind::Snow, diagonal, &gust);
        let blizzard = precipitation_velocity(WeatherKind::Blizzard, diagonal, &gust);
        let slope = |velocity: Vec3| velocity.x.hypot(velocity.z) / -velocity.y;

        assert!(rain.y < 0.0 && rain.x > 0.0 && rain.z == 0.0);
        assert!(
            rain.y / rain.length() < -0.95,
            "normal rain must read as falling: {rain:?}"
        );
        assert!(snow.y < 0.0 && snow.x > 0.0 && snow.z > 0.0);
        assert!(slope(snow) < 2.5, "snow gusts stay moderate: {snow:?}");
        assert!(
            slope(blizzard) > 5.0,
            "a blizzard gust is driven near-flat: {blizzard:?}"
        );
        let speed = blizzard.x.hypot(blizzard.z);
        assert!((10.0..=18.0).contains(&speed), "driving speed {speed}");
        assert!(
            slope(precipitation_velocity(
                WeatherKind::Blizzard,
                diagonal,
                &lull
            )) < 0.05,
            "a lull falls straight down"
        );

        let wind_only = precipitation_velocity(WeatherKind::HighWind, diagonal, &lull);
        assert_eq!(wind_only.y, 0.0);
        assert!(
            wind_only.x > 0.0 && wind_only.z > 0.0,
            "streaks keep an axis"
        );
    }

    fn count(kind: WeatherKind, precipitation: u16, wind: u16) -> usize {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let mut s = state(kind, precipitation);
        s.gust_q16 = wind;
        // Let the smoothed precipitation settle.
        for _ in 0..120 {
            p.update(&s, &camera(), 1.0 / 60.0, &v);
        }
        p.update(&s, &camera(), 1.0 / 60.0, &v).particles.len()
    }

    #[test]
    fn particle_count_scales_with_snow_tier_and_stays_under_the_cap() {
        use spall_protocol::SnowTier;
        let [light, medium, heavy] =
            SnowTier::ALL.map(|t| count(WeatherKind::Snow, t.precipitation_q16(), 13_107));
        assert!(light < medium && medium < heavy, "{light} {medium} {heavy}");
        let blizzard = count(WeatherKind::Blizzard, 62_259, 55_000);
        assert!(
            blizzard > heavy * 13 / 10,
            "blizzard {blizzard} vs heavy {heavy}"
        );
        assert!(blizzard <= MAX_WEATHER_PARTICLES);
        let saturated = count(WeatherKind::Blizzard, u16::MAX, 55_000);
        assert!(saturated <= MAX_WEATHER_PARTICLES);
    }

    #[test]
    fn blizzard_whiteout_pulses_with_gusts_and_heavy_snow_only_thins_the_air() {
        let calm = snow_visibility(true, 0.95, 0.0, 1.0);
        let gust = snow_visibility(true, 0.95, 1.0, 1.0);
        assert!(gust < calm && gust <= WHITEOUT_VISIBILITY + 0.01 && calm > 0.5);
        assert_eq!(
            snow_visibility(true, 0.95, 1.0, 0.0),
            1.0,
            "sheltered is clear"
        );
        assert_eq!(snow_visibility(false, 0.5, 0.5, 1.0), 1.0);
        let heavy = snow_visibility(false, 0.85, 0.5, 1.0);
        assert!((0.6..0.8).contains(&heavy), "{heavy}");
        let (start, end, white) = weather_fog(gust, 120.0, 190.0);
        assert!(
            end < 30.0 && start < 3.0 && white > 0.95,
            "{start} {end} {white}"
        );
        assert_eq!(weather_fog(1.0, 120.0, 190.0), (120.0, 190.0, 0.0));
    }

    #[test]
    fn clear_weather_does_not_turn_background_gust_into_sideways_precipitation() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let clear = state(WeatherKind::Clear, 0);
        let frame = p.update(&clear, &camera(), 0.1, &v);
        assert!(frame.particles.is_empty());
    }

    #[test]
    fn submerged_camera_drops_weather_effects_and_restores_them_on_surfacing() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let mut s = state(WeatherKind::Storm, u16::MAX);
        s.lightning_id = 1;
        s.lightning_tick = 1;
        p.set_reduced_flashes(false);
        for _ in 0..120 {
            p.update(&s, &camera(), 1.0 / 60.0, &v);
        }
        let dry = p.update(&s, &camera(), 1.0 / 60.0, &v);
        assert!(!dry.particles.is_empty() && dry.lighting.storm_dimming > 0.0);

        p.set_submerged(true);
        for _ in 0..120 {
            p.update(&s, &camera(), 1.0 / 60.0, &v);
        }
        let wet = p.update(&s, &camera(), 1.0 / 60.0, &v);
        assert!(wet.particles.is_empty());
        assert!(wet.lighting.storm_dimming < 1e-3);
        assert!(wet.lighting.lightning_flash < 1e-3);
        assert!(p.submerge() > 0.99);

        let mut b = state(WeatherKind::Blizzard, u16::MAX);
        b.weather_seed = s.weather_seed;
        let mut q = WeatherPresentation::new();
        q.set_submerged(true);
        for _ in 0..120 {
            q.update(&b, &camera(), 1.0 / 60.0, &v);
        }
        assert!(q.update(&b, &camera(), 1.0 / 60.0, &v).lighting.visibility > 0.99);

        p.set_submerged(false);
        for _ in 0..120 {
            p.update(&s, &camera(), 1.0 / 60.0, &v);
        }
        assert!(!p.update(&s, &camera(), 1.0 / 60.0, &v).particles.is_empty());
    }
}
