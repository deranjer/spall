//! Bounded, deterministic weather presentation derived from the authoritative
//! weather snapshot and an immutable resident voxel view. No weather state is
//! persisted here; particles are camera-local cosmetics.

use glam::Vec3;
use spall_core::GlobalCell;
use spall_protocol::{WeatherKind, WeatherState};
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
    /// Drives the wind veer and gusts; unlike `phase_seconds` it survives a teleport.
    wind_clock: f64,
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
            wind_clock: 0.0,
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

    pub fn set_reduced_flashes(&mut self, reduced: bool) {
        self.reduced_flashes = reduced;
    }

    pub fn reset(&mut self) {
        self.particles.clear();
        self.last_camera = None;
        self.last_epoch = None;
        self.phase_seconds = 0.0;
        self.wind_clock = 0.0;
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
        self.wind_clock += f64::from(dt_seconds.clamp(0.0, 0.1));
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
        // Veering and gusting are presentation only, seeded from the weather seed so
        // every client draws the same pattern; snow tiers get gentler versions.
        let wind_sample = wind_sample(
            state.weather_seed,
            self.wind_clock,
            if blizzard { 1.0 } else { 0.3 },
        );
        let pulse = wind_sample.surge;
        let desired = if self.precipitation > 0.005 {
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
        let mut wind_direction = Vec3::new(
            f32::from(state.wind_x_q15) / 32767.0,
            0.0,
            f32::from(state.wind_z_q15) / 32767.0,
        )
        .normalize_or_zero();
        if snow {
            wind_direction = glam::Quat::from_rotation_y(wind_sample.yaw_rad) * wind_direction;
        }
        let is_wind_cue = state.kind == WeatherKind::HighWind && self.precipitation <= 0.005;
        let gust = self.wind;
        let velocity = precipitation_velocity(state.kind, gust, wind_direction, wind_sample);
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
                    let t = self.wind_clock;
                    let amp =
                        f64::from(0.3 + 0.6 * field(h2, 0)) * f64::from(0.5 + wind_sample.surge);
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
                [0.72, 0.78, 0.81, 0.24]
            } else if blizzard {
                [0.92, 0.96, 1.0, 0.7 * near]
            } else if snow {
                [0.92, 0.96, 1.0, 0.85 * near]
            } else {
                [0.58, 0.72, 0.9, if storm { 0.64 } else { 0.48 }]
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
            (1.0 - lightning_age_ticks as f32 / FLASH_TICKS as f32) * 0.08 * exposure
        } else {
            0.0
        };
        let lighting = WeatherLighting {
            exposure,
            precipitation: self.precipitation,
            wind: self.wind,
            visibility: snow_visibility(blizzard, self.precipitation, pulse, exposure),
            storm_dimming: if storm {
                self.precipitation * 0.22 * exposure
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

/// One instant of the presentation-side wind variation.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WindSample {
    /// Veer of the wind direction about +Y, radians.
    yaw_rad: f32,
    /// Blizzard driving speed, 6-16 m/s.
    speed_mps: f32,
    /// 0..1 gust strength; also drives the whiteout pulse.
    surge: f32,
}

const MAX_VEER_RAD: f32 = 35.0 * std::f32::consts::PI / 180.0;

/// Wind veer and gusts as a smooth, bounded function of seed and time. The
/// direction wanders up to `35 deg * veer_scale` over tens of seconds, gusts
/// surge every few seconds, and the speed rides them between 6 and 16 m/s.
fn wind_sample(seed: u64, t: f64, veer_scale: f32) -> WindSample {
    let tau = std::f64::consts::TAU;
    let phase = |salt: u64| (hash(seed ^ salt) >> 11) as f64 / (1u64 << 53) as f64 * tau;
    let veer =
        0.6 * (t * tau / 47.0 + phase(0xA1)).sin() + 0.4 * (t * tau / 23.0 + phase(0xA2)).sin();
    let surge = 0.5
        + 0.5 * (t * tau / 5.3 + phase(0xA3) + 1.1 * (t * tau / 13.7 + phase(0xA4)).sin()).sin();
    let surge = (surge * surge) as f32;
    WindSample {
        yaw_rad: veer as f32 * MAX_VEER_RAD * veer_scale.clamp(0.0, 1.0),
        speed_mps: 6.0 + 10.0 * surge,
        surge,
    }
}

/// World-space velocity shared by particle drift and the visible ribbon axis.
/// Normal precipitation remains distinctly downward; blizzards are driven
/// nearly flat along the (veering, gusting) wind, and wind-only cues stay
/// horizontal.
fn precipitation_velocity(kind: WeatherKind, gust: f32, wind: Vec3, sample: WindSample) -> Vec3 {
    let gust = gust.clamp(0.0, 1.0);
    if kind == WeatherKind::HighWind {
        return wind * (2.0 + gust * 6.0);
    }
    let (fall, drift) = match kind {
        WeatherKind::Rain => (13.0, gust * 1.8),
        WeatherKind::Storm => (13.0, gust * 2.8),
        WeatherKind::Snow => (1.25, (0.25 + gust * 0.5) * (0.6 + 0.8 * sample.surge)),
        // Driving snow: 6-16 m/s along the wind with only a slow fall.
        WeatherKind::Blizzard => (1.0, sample.speed_mps),
        WeatherKind::Clear | WeatherKind::HighWind => (0.0, 0.0),
    };
    Vec3::new(wind.x * drift, -fall, wind.z * drift)
}

/// Camera visibility: heavy snow thins the air somewhat, a blizzard pulses between about
/// 0.30 and `WHITEOUT_VISIBILITY` with the gusts. Sheltered cameras see clear air.
fn snow_visibility(blizzard: bool, precipitation: f32, pulse: f32, exposure: f32) -> f32 {
    let loss = if blizzard {
        let severity = (precipitation / 0.95).clamp(0.0, 1.0);
        (1.0 - WHITEOUT_VISIBILITY) * severity * (0.82 + 0.18 * pulse)
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
        let sample = WindSample {
            yaw_rad: 0.0,
            speed_mps: 11.0,
            surge: 0.5,
        };
        let rain = precipitation_velocity(WeatherKind::Rain, 0.9, cardinal, sample);
        let snow = precipitation_velocity(WeatherKind::Snow, 0.2, diagonal, sample);
        let blizzard = precipitation_velocity(WeatherKind::Blizzard, 0.9, diagonal, sample);
        let slope = |velocity: Vec3| velocity.x.hypot(velocity.z) / -velocity.y;

        assert!(rain.y < 0.0 && rain.x > 0.0 && rain.z == 0.0);
        assert!(
            rain.y / rain.length() < -0.99,
            "normal rain must read as falling: {rain:?}"
        );
        assert!(
            slope(rain) < 0.13,
            "normal rain has only modest slant: {rain:?}"
        );
        assert!(snow.y < 0.0 && snow.x > 0.0 && snow.z > 0.0);
        assert!(
            snow.y / snow.length() < -0.9,
            "normal snow stays downward: {snow:?}"
        );
        assert!(
            slope(blizzard) > 5.0,
            "blizzard is driven near-flat: {blizzard:?}"
        );
        let speed = blizzard.x.hypot(blizzard.z);
        assert!((6.0..=16.0).contains(&speed), "driving speed {speed}");

        let wind_only = precipitation_velocity(WeatherKind::HighWind, 0.9, diagonal, sample);
        assert_eq!(wind_only.y, 0.0);
        assert!(wind_only.x > 0.0 && wind_only.z > 0.0);
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
        assert!(gust < calm && gust <= WHITEOUT_VISIBILITY + 0.01 && calm < 0.35);
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
    fn wind_veers_and_gusts_within_bounds_deterministically_and_smoothly() {
        let seed = 0x5EED;
        let (mut min_speed, mut max_speed) = (f32::MAX, f32::MIN);
        let (mut min_yaw, mut max_yaw) = (f32::MAX, f32::MIN);
        let mut previous = wind_sample(seed, 0.0, 1.0);
        for step in 1..=12_000 {
            let t = f64::from(step) * 0.05;
            let sample = wind_sample(seed, t, 1.0);
            assert_eq!(sample, wind_sample(seed, t, 1.0));
            assert!(
                (6.0..=16.0).contains(&sample.speed_mps),
                "{}",
                sample.speed_mps
            );
            assert!(sample.yaw_rad.abs() <= MAX_VEER_RAD + 1e-6);
            assert!((0.0..=1.0).contains(&sample.surge));
            assert!(
                (sample.speed_mps - previous.speed_mps).abs() < 2.5,
                "gusts are smooth"
            );
            assert!(
                (sample.yaw_rad - previous.yaw_rad).abs() < 0.03,
                "veer is smooth"
            );
            previous = sample;
            min_speed = min_speed.min(sample.speed_mps);
            max_speed = max_speed.max(sample.speed_mps);
            min_yaw = min_yaw.min(sample.yaw_rad);
            max_yaw = max_yaw.max(sample.yaw_rad);
        }
        assert!(
            max_speed - min_speed > 7.0,
            "gusts surge: {min_speed}..{max_speed}"
        );
        assert!(
            max_yaw - min_yaw > 0.8,
            "direction veers: {min_yaw}..{max_yaw}"
        );
        assert_ne!(wind_sample(seed, 9.0, 1.0), wind_sample(seed + 1, 9.0, 1.0));
        // Snow tiers swing less.
        let gentle = (0..2_000)
            .map(|i| wind_sample(seed, f64::from(i) * 0.1, 0.3).yaw_rad.abs())
            .fold(0.0, f32::max);
        assert!(gentle <= MAX_VEER_RAD * 0.3 + 1e-6);
    }
}
