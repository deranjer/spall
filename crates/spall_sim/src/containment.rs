//! Containment diagnostics (T23 / G4): two *different* questions about a body that
//! is somewhere it should not be.
//!
//! * **Collision correctness** — does the body's transformed occupied geometry
//!   overlap intact terrain? A resting body's cells lie on or above the surface; a
//!   body embedded in the ground is a solver failure whatever else it later does.
//!   This is judged from the *cells* (or, for a cheap pre-filter, the transformed
//!   collider bounds), never from the body origin: a child body's origin can sit far
//!   from its geometry.
//! * **Out-of-world lifecycle** — is the body's geometry entirely outside the bounded
//!   world? README declares that such debris is persisted in a dormant external-body
//!   set; nothing implements that yet, so a body crossing the limit keeps simulating
//!   (and free-falling) as an ordinary dynamic body. Crossing over an ordinary wall
//!   is physically legitimate; only leaving the bounds makes a body external.
//!
//! Read-only: nothing here changes authoritative state, hashes or journals.

use glam::DVec3;
use spall_core::{CELLS_PER_BRICK, GlobalCell, LocalCell};
use spall_voxel::Sample;

use crate::body::Body;
use crate::world::SimWorld;

/// A body flagged by [`containment_census`].
#[derive(Debug, Clone)]
pub struct ContainmentRow {
    pub entity: u64,
    pub aabb_min_m: [f64; 3],
    pub aabb_max_m: [f64; 3],
    pub velocity_m_s: [f64; 3],
    pub sleeping: bool,
    pub dormant: bool,
    pub solid_cells: u64,
    /// Deepest embedding of any of the body's cell centres in solid terrain (m);
    /// `0` for a row that is only outside the world.
    pub penetration_depth_m: f64,
    /// Body cells whose centre lies inside solid terrain.
    pub overlapped_cells: u32,
}

/// Result of one scan.
#[derive(Debug, Clone, Default)]
pub struct ContainmentCensus {
    /// World box, metres (the terrain volume's brick bounds).
    pub world_min_m: [f64; 3],
    pub world_max_m: [f64; 3],
    pub bodies: u64,
    /// Bodies whose collider bounds touched solid terrain and were checked cell by cell.
    pub cell_checked: u64,
    /// Candidates skipped because they have more solid cells than the cap.
    pub skipped_large: u64,
    /// Bodies whose collider bounds contain a solid terrain cell (prefilter hits).
    pub prefilter_passed: u64,
    /// Bodies not examined because their bounds were too large to scan.
    pub skipped_aabb_too_large: u64,
    /// Bodies not examined because this scan's sample budget ran out.
    pub skipped_budget: u64,
    /// Terrain samples the prefilter spent this scan.
    pub prefilter_samples: u64,
    /// Where the next scan should start so a budgeted census rotates through every body.
    pub next_offset: usize,
    /// Bodies whose cells are embedded in terrain by at least `min_depth_m`.
    pub deep_penetrations: Vec<ContainmentRow>,
    /// Bodies whose geometry lies entirely outside the world box.
    pub external: Vec<ContainmentRow>,
}

fn transform(body: &Body, local_m: DVec3) -> DVec3 {
    body.pose.rotation * local_m + DVec3::from_array(body.pose.translation_m)
}

/// World-space bounds of the body's collider region (8 transformed corners).
fn collider_bounds(body: &Body) -> ([f64; 3], [f64; 3]) {
    let m = body.cell_size().metres();
    let (lo, hi) = body.collider_region;
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for corner in 0..8 {
        let p = DVec3::new(
            if corner & 1 == 0 {
                lo.x as f64
            } else {
                hi.x as f64 + 1.0
            } * m,
            if corner & 2 == 0 {
                lo.y as f64
            } else {
                hi.y as f64 + 1.0
            } * m,
            if corner & 4 == 0 {
                lo.z as f64
            } else {
                hi.z as f64 + 1.0
            } * m,
        );
        let w = transform(body, p);
        for (a, v) in [w.x, w.y, w.z].into_iter().enumerate() {
            min[a] = min[a].min(v);
            max[a] = max[a].max(v);
        }
    }
    (min, max)
}

fn terrain_solid_at(world: &SimWorld, p: DVec3) -> bool {
    let m = world.terrain().cell_size().metres();
    let cell = GlobalCell::new(
        (p.x / m).floor() as i64,
        (p.y / m).floor() as i64,
        (p.z / m).floor() as i64,
    );
    matches!(world.terrain().volume.sample(cell), Ok(Sample::Filled(_)))
}

/// Limits for one census scan.
#[derive(Debug, Clone, Copy)]
pub struct CensusOptions {
    /// Per-body cap on the cell walk (bodies with more solid cells that touch terrain are counted
    /// in `skipped_large`, not checked).
    pub max_cells: u64,
    /// Embedding depth (m) that counts as a deep penetration.
    pub min_depth_m: f64,
    /// Per-body cap on prefilter terrain samples (bigger bounds are `skipped_aabb_too_large`).
    pub max_aabb_samples: u64,
    /// Total prefilter samples per scan; bodies past the budget are `skipped_budget`.
    pub sample_budget: u64,
    /// Body index to start from (use the previous scan's `next_offset`).
    pub start_offset: usize,
}

/// Unbudgeted scan (tests and one-off diagnostics).
pub fn containment_census(world: &SimWorld, max_cells: u64, min_depth_m: f64) -> ContainmentCensus {
    containment_census_with(
        world,
        CensusOptions {
            max_cells,
            min_depth_m,
            max_aabb_samples: 1_000_000,
            sample_budget: u64::MAX,
            start_offset: 0,
        },
    )
}

/// Scans every non-terrain body, examining as many as the budget allows and reporting exactly how
/// many were not, so a clean result is never mistaken for full coverage.
pub fn containment_census_with(world: &SimWorld, opts: CensusOptions) -> ContainmentCensus {
    let (max_cells, min_depth_m) = (opts.max_cells, opts.min_depth_m);
    let terrain = world.terrain();
    let tm = terrain.cell_size().metres();
    let mut census = ContainmentCensus::default();
    if let Some(b) = terrain.volume.bounds() {
        let edge = 32.0 * tm;
        census.world_min_m = [
            b.min.x as f64 * edge,
            b.min.y as f64 * edge,
            b.min.z as f64 * edge,
        ];
        census.world_max_m = [
            (b.max.x + 1) as f64 * edge,
            (b.max.y + 1) as f64 * edge,
            (b.max.z + 1) as f64 * edge,
        ];
    } else {
        census.world_min_m = [f64::MIN; 3];
        census.world_max_m = [f64::MAX; 3];
    }
    let bodies: Vec<&Body> = world.bodies().filter(|b| b.entity.is_some()).collect();
    let n = bodies.len();
    census.next_offset = opts.start_offset % n.max(1);
    for step in 0..n {
        let idx = (opts.start_offset + step) % n;
        let body = bodies[idx];
        census.bodies += 1;
        let (min, max) = collider_bounds(body);
        let row = |depth: f64, overlapped: u32, cells: u64| ContainmentRow {
            entity: body.entity.map_or(0, |e| e.get()),
            aabb_min_m: min,
            aabb_max_m: max,
            velocity_m_s: body.linvel_m_s,
            sleeping: body.sleeping,
            dormant: body.dormant,
            solid_cells: cells,
            penetration_depth_m: depth,
            overlapped_cells: overlapped,
        };
        let outside =
            (0..3).any(|a| max[a] < census.world_min_m[a] || min[a] > census.world_max_m[a]);
        if outside {
            census.external.push(row(0.0, 0, 0));
            continue;
        }
        // Pre-filter: does the body's world-space collider bounds contain any solid terrain? The
        // bounds are scanned at terrain-cell spacing, not just at their corners: a long or thin body
        // that straddles a wall or the ground with both ends in air has no corner in solid
        // terrain. Bounds too large to scan, and bodies beyond this scan's budget, are counted in
        // the census -- never silently treated as clear.
        let dims: [u64; 3] = std::array::from_fn(|a| ((max[a] - min[a]) / tm).ceil() as u64 + 1);
        let samples = dims[0].saturating_mul(dims[1]).saturating_mul(dims[2]);
        if samples > opts.max_aabb_samples {
            census.skipped_aabb_too_large += 1;
            continue;
        }
        if census.prefilter_samples.saturating_add(samples) > opts.sample_budget {
            census.skipped_budget += 1;
            if census.skipped_budget == 1 {
                census.next_offset = idx;
            }
            continue;
        }
        census.prefilter_samples += samples;
        let mut touches_terrain = false;
        'aabb: for i in 0..dims[0] {
            let x = (min[0] + i as f64 * tm).min(max[0]);
            for j in 0..dims[1] {
                let y = (min[1] + j as f64 * tm).min(max[1]);
                for k in 0..dims[2] {
                    let z = (min[2] + k as f64 * tm).min(max[2]);
                    if terrain_solid_at(world, DVec3::new(x, y, z)) {
                        touches_terrain = true;
                        break 'aabb;
                    }
                }
            }
        }
        if !touches_terrain {
            continue;
        }
        census.prefilter_passed += 1;
        census.cell_checked += 1;
        let cm = body.cell_size().metres();
        let mut cells = 0u64;
        let mut overlapped = 0u32;
        let mut depth = 0.0f64;
        let mut too_large = false;
        'bricks: for coord in body.volume.resident_brick_coords() {
            let Some(snap) = body.volume.snapshot_brick(coord).ok().flatten() else {
                continue;
            };
            for index in 0..CELLS_PER_BRICK as u16 {
                let local = LocalCell::from_linear_index(index).expect("index < 32768");
                if snap.get(local).is_air() {
                    continue;
                }
                cells += 1;
                if cells > max_cells {
                    too_large = true;
                    break 'bricks;
                }
                let c = DVec3::new(
                    (coord.x * 32 + local.x() as i64) as f64 + 0.5,
                    (coord.y * 32 + local.y() as i64) as f64 + 0.5,
                    (coord.z * 32 + local.z() as i64) as f64 + 0.5,
                ) * cm;
                let w = transform(body, c);
                if terrain_solid_at(world, w) {
                    overlapped += 1;
                    // Distance up to the terrain surface, in half-cell steps.
                    let mut d = 0.0;
                    let mut q = w;
                    while d < 4.0 && terrain_solid_at(world, q) {
                        d += tm * 0.5;
                        q.y += tm * 0.5;
                    }
                    depth = depth.max(d);
                }
            }
        }
        if too_large {
            census.skipped_large += 1;
            continue;
        }
        if overlapped > 0 && depth >= min_depth_m {
            census.deep_penetrations.push(row(depth, overlapped, cells));
        }
    }
    census
}
