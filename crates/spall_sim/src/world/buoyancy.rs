//! Water forces on detached bodies (ENG-168 follow-up): buoyancy and drag.
//!
//! Each tick, before the physics step, every dynamic body is sampled at the
//! centres of its solid cells. A sample below the water surface in its column
//! counts as displacing its cell's volume; the displaced volume gives an upward
//! impulse `rho * g * V * dt` at the centre of the submerged volume (so a raft
//! with its load off-centre tilts), and the submerged fraction damps the body's
//! linear and angular velocity. Water is not pushed by bodies: this is a
//! one-way coupling, as the design notes allow for this rewrite.
//!
//! A body only displaces water with its *solid* cells. A sealed hull has to be
//! solid to float; a thin open shell displaces little and sinks, which is the
//! honest consequence of not simulating water flooding into it.

use std::collections::HashMap;
use std::sync::Arc;

use glam::DVec3;
use spall_physics::OccupancyGrid;

use super::SimWorld;

/// Density of the water, kg/m^3.
pub const WATER_DENSITY_KG_M3: f64 = 1000.0;
/// Gravity used for the buoyant force, m/s^2.
const GRAVITY_M_S2: f64 = 9.81;
/// Velocity damping per second at full submersion.
const LINEAR_DRAG_PER_S: f64 = 1.2;
const ANGULAR_DRAG_PER_S: f64 = 2.0;
/// Bodies with more solid cells than this are sampled on a stride.
const MAX_SAMPLES: usize = 2048;

/// A body's solid cells as sample points, in its local cell coordinates.
struct Samples {
    /// Cell centres, local cell units.
    centres: Vec<[f32; 3]>,
    /// Volume each sample stands for, m^3 (more than one cell when strided).
    volume_m3: f64,
    /// Total solid volume of the body, m^3.
    total_m3: f64,
}

/// Sample points per body, rebuilt only when the body's volume changes.
#[derive(Default)]
pub struct BuoyancyCache {
    entries: HashMap<u64, (u64, Arc<Samples>)>,
}

/// What one buoyancy pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BuoyancyReport {
    /// Bodies with any part in water this tick.
    pub bodies_in_water: usize,
    /// Total displaced volume, m^3.
    pub displaced_m3: f64,
}

fn build_samples(body: &super::Body) -> Option<Samples> {
    let grid = OccupancyGrid::from_volume(&body.volume).ok().flatten()?;
    let cell_m = body.volume.cell_size().metres();
    let solid = grid.solid_count() as usize;
    if solid == 0 {
        return None;
    }
    let stride = solid.div_ceil(MAX_SAMPLES).max(1);
    let o = grid.origin();
    let mut centres = Vec::with_capacity(solid / stride + 1);
    let mut n = 0usize;
    grid.for_each_solid(|x, y, z, _| {
        if n.is_multiple_of(stride) {
            centres.push([
                (o.x + i64::from(x)) as f32 + 0.5,
                (o.y + i64::from(y)) as f32 + 0.5,
                (o.z + i64::from(z)) as f32 + 0.5,
            ]);
        }
        n += 1;
    });
    let cell_m3 = cell_m.powi(3);
    Some(Samples {
        volume_m3: cell_m3 * stride as f64,
        total_m3: cell_m3 * solid as f64,
        centres,
    })
}

impl SimWorld {
    /// Applies buoyancy and drag for one tick of `dt_s`. `surface` answers
    /// "height (m) of the water surface in the span holding this point, if any".
    pub fn apply_buoyancy(
        &mut self,
        surface: &dyn Fn(f64, f64, f64) -> Option<f64>,
        dt_s: f64,
        cache: &mut BuoyancyCache,
    ) -> BuoyancyReport {
        let mut report = BuoyancyReport::default();
        let live: std::collections::HashSet<u64> = self.bodies.keys().copied().collect();
        cache.entries.retain(|id, _| live.contains(id));
        let keys: Vec<u64> = self.bodies.keys().copied().collect();
        for id in keys {
            let body = &self.bodies[&id];
            if body.dormant {
                continue;
            }
            let stamp = body.volume.state_stamp();
            let samples = match cache.entries.get(&id) {
                Some((s, samples)) if *s == stamp => Arc::clone(samples),
                _ => {
                    let Some(built) = build_samples(body) else {
                        continue;
                    };
                    let built = Arc::new(built);
                    cache.entries.insert(id, (stamp, Arc::clone(&built)));
                    built
                }
            };
            let cell_size = body.volume.cell_size();
            let cell_m = cell_size.metres();
            let pose = body.pose;
            let (mut sub, mut centroid) = (0.0f64, DVec3::ZERO);
            for c in &samples.centres {
                let p = pose.local_cell_to_world_m(
                    DVec3::new(f64::from(c[0]), f64::from(c[1]), f64::from(c[2])),
                    cell_size,
                );
                let Some(top) = surface(p.x, p.y, p.z) else {
                    continue;
                };
                // A sample is a cell: it is submerged in proportion to how far
                // the surface is above its centre, over one cell of height.
                let fraction = ((top - p.y) / cell_m + 0.5).clamp(0.0, 1.0);
                if fraction > 0.0 {
                    sub += fraction * samples.volume_m3;
                    centroid += p * (fraction * samples.volume_m3);
                }
            }
            if sub <= 0.0 {
                continue;
            }
            let point = centroid / sub;
            let phys = body.phys;
            let submerged_fraction = (sub / samples.total_m3).clamp(0.0, 1.0);
            report.bodies_in_water += 1;
            report.displaced_m3 += sub;
            let Ok(point) = self.physics_translation(point.to_array()) else {
                continue;
            };
            // Drag first: it reads the solver's velocity, and the impulse below
            // changes it.
            let st = self.physics.body_state(phys);
            let lin = (-LINEAR_DRAG_PER_S * submerged_fraction * dt_s).exp() as f32;
            let ang = (-ANGULAR_DRAG_PER_S * submerged_fraction * dt_s).exp() as f32;
            self.physics.set_body_velocity(
                phys,
                st.linvel_m_s.map(|v| v * lin),
                st.angvel_rad_s.map(|v| v * ang),
            );
            let impulse = (WATER_DENSITY_KG_M3 * GRAVITY_M_S2 * sub * dt_s) as f32;
            self.physics
                .apply_impulse_at_point(phys, [0.0, impulse, 0.0], point);
        }
        report
    }
}
