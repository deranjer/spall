//! Bounded, deterministic weather presentation derived from the authoritative
//! weather snapshot and an immutable resident voxel view. No weather state is
//! persisted here; particles are camera-local cosmetics.

use glam::Vec3;
use spall_core::GlobalCell;
use spall_protocol::{WeatherKind, WeatherState};
use spall_render::{Camera, WeatherLighting, WeatherParticle};
use spall_voxel::{Sample, Volume};

/// Hard cap keeps effect cost independent of world size and weather intensity.
pub const MAX_WEATHER_PARTICLES: usize = 1_536;
const GRID_SIDE: usize = 5;
const GRID_RADIUS_M: f32 = 20.0;
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
        }
        self.last_camera = Some(position);
        self.last_epoch = Some(state.world_epoch);
        self.phase_seconds += f64::from(dt_seconds.clamp(0.0, 0.1));
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
        let desired = if self.precipitation > 0.005 {
            let scale = if snow { 1.0 } else { 1.25 };
            (self.precipitation * MAX_WEATHER_PARTICLES as f32 * scale).ceil() as usize
        } else if state.kind == WeatherKind::HighWind && self.wind > 0.08 {
            (self.wind * 180.0).ceil() as usize
        } else {
            0
        }
        .min(MAX_WEATHER_PARTICLES);
        let wind_direction = Vec3::new(
            f32::from(state.wind_x_q15) / 32767.0,
            0.0,
            f32::from(state.wind_z_q15) / 32767.0,
        )
        .normalize_or_zero();
        let is_wind_cue = state.kind == WeatherKind::HighWind && self.precipitation <= 0.005;
        let velocity = precipitation_velocity(state.kind, self.wind, wind_direction);
        let fall_speed = -velocity.y;
        self.particles.clear();
        self.particles
            .reserve(desired.saturating_sub(self.particles.capacity()));
        for index in 0..desired {
            let h = hash(state.weather_seed ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let phase = unit(h) * 64.0;
            let y = if fall_speed > 0.0 {
                position.y + 20.0
                    - (self.phase_seconds * f64::from(fall_speed) + f64::from(phase))
                        .rem_euclid(24.0) as f32
            } else {
                position.y + (unit(hash(h ^ 0x0F0F)) * 16.0 - 8.0)
            };
            let side_x = (unit(hash(h ^ 0xA5A5)) * 40.0 - 20.0
                + (self.phase_seconds * f64::from(velocity.x)).rem_euclid(40.0) as f32)
                .rem_euclid(40.0)
                - 20.0;
            let side_z = (unit(hash(h ^ 0x5A5A)) * 40.0 - 20.0
                + (self.phase_seconds * f64::from(velocity.z)).rem_euclid(40.0) as f32)
                .rem_euclid(40.0)
                - 20.0;
            let xz = Vec3::new(position.x + side_x, 0.0, position.z + side_z);
            if !grid_open_at(position, xz, &exposed_grid) {
                continue;
            }
            let centre = Vec3::new(position.x + side_x, y, position.z + side_z);
            let colour = if is_wind_cue {
                [0.72, 0.78, 0.81, 0.24]
            } else if snow {
                [0.88, 0.94, 1.0, if blizzard { 0.78 } else { 0.64 }]
            } else {
                [0.58, 0.72, 0.9, if storm { 0.64 } else { 0.48 }]
            };
            self.particles.push(WeatherParticle {
                centre: centre.to_array(),
                direction: velocity.normalize_or_zero().to_array(),
                length_m: if is_wind_cue {
                    0.8
                } else if blizzard {
                    0.18 + self.wind * 0.08
                } else if snow {
                    0.08
                } else {
                    0.9 + self.wind * 0.45
                },
                width_m: if is_wind_cue {
                    0.07
                } else if blizzard {
                    0.035
                } else if snow {
                    0.07
                } else {
                    0.018
                },
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
            visibility: if blizzard {
                1.0 - self.precipitation * 0.78 * exposure
            } else {
                1.0
            },
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

/// World-space velocity shared by particle drift and the visible ribbon axis.
/// Normal precipitation remains distinctly downward; blizzards carry a
/// stronger crosswind while retaining a fall component, and wind-only cues
/// stay horizontal.
fn precipitation_velocity(kind: WeatherKind, gust: f32, wind: Vec3) -> Vec3 {
    let gust = gust.clamp(0.0, 1.0);
    if kind == WeatherKind::HighWind {
        return wind * (2.0 + gust * 6.0);
    }
    let (fall, drift) = match kind {
        WeatherKind::Rain => (13.0, gust * 1.8),
        WeatherKind::Storm => (13.0, gust * 2.8),
        WeatherKind::Snow => (2.5, gust * 0.45),
        WeatherKind::Blizzard => (2.5, gust * 2.2),
        WeatherKind::Clear | WeatherKind::HighWind => (0.0, 0.0),
    };
    Vec3::new(wind.x * drift, -fall, wind.z * drift)
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

fn unit(value: u64) -> f32 {
    (value >> 40) as f32 / ((1u32 << 24) - 1) as f32
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
        let rain = precipitation_velocity(WeatherKind::Rain, 0.9, cardinal);
        let snow = precipitation_velocity(WeatherKind::Snow, 0.9, diagonal);
        let blizzard = precipitation_velocity(WeatherKind::Blizzard, 0.9, diagonal);
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
            snow.y / snow.length() < -0.98,
            "normal snow stays downward: {snow:?}"
        );
        assert!(slope(blizzard) > slope(snow));
        assert!(
            slope(blizzard) < 0.8,
            "blizzard still falls, not flat: {blizzard:?}"
        );

        let wind_only = precipitation_velocity(WeatherKind::HighWind, 0.9, diagonal);
        assert_eq!(wind_only.y, 0.0);
        assert!(wind_only.x > 0.0 && wind_only.z > 0.0);
    }

    #[test]
    fn clear_weather_does_not_turn_background_gust_into_sideways_precipitation() {
        let v = known_air();
        let mut p = WeatherPresentation::new();
        let clear = state(WeatherKind::Clear, 0);
        let frame = p.update(&clear, &camera(), 0.1, &v);
        assert!(frame.particles.is_empty());
    }
}
