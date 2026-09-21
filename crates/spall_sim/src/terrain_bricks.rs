//! Per-brick terrain colliders (T23 / G4 prototype, opt-in, default OFF).
//!
//! **Problem.** The terrain collider is one whole-world compound. Every terrain edit replaces
//! it, and Rapier wakes everything resting on a replaced collider — so a dig far from a
//! sleeping pile still wakes the pile (measured: ~99.96% of the workload's sleeping-body wakes
//! coincide with a terrain-dig commit; removing digs cuts wakes ~70%). The whole-terrain
//! extraction/plan is also O(terrain) per commit.
//!
//! **Prototype.** [`SimWorld::enable_terrain_brick_colliders`] splits the *physics* terrain into
//! one fixed collider per solid 32-cell brick (8 m at the quarter-metre cell). The
//! authoritative terrain volume, its hashes, journal and replication are untouched: this only
//! changes how the solver sees it. A terrain commit rebuilds only the colliders of the bricks it
//! actually changed, so only bodies touching *those* bricks are woken.
//!
//! Guarantees kept:
//! * **Authoritative geometry.** Each brick collider is built from that brick's exact occupancy
//!   (`OccupancyGrid::from_region`); together they cover exactly the volume's solid cells
//!   ([`SimWorld::validate_terrain_brick_colliders`] checks coverage and revisions).
//! * **Collision across brick boundaries.** A body straddling two bricks collides with both
//!   colliders; the merged-cuboid faces meet flush, exactly as adjacent boxes inside the single
//!   compound do (tested against the single-collider result).
//! * **Revision validation.** Each collider records the brick revision it was built from and is
//!   checked against the volume.
//! * **Tick-boundary publication.** Plans are built during staging, before publish; publish is
//!   infallible and happens at the same point in the commit as the single-collider swap.
//!
//! Not supported by the prototype (documented, not hidden): residency eviction of terrain
//! bricks (an evicted brick makes the commit fail as `Unresident`), and wiring to the server
//! CLI. Compaction is a separate decision.

use std::collections::BTreeMap;

use spall_core::BrickCoord;
use spall_physics::{BodyKind as PhysBodyKind, BodySpec, OccupancyGrid};
use spall_voxel::Volume;

use crate::collider::{ColliderPlan, plan_collider};
use crate::world::{SimWorld, WorldError};

/// One brick's collider state.
#[derive(Debug, Clone)]
pub(crate) struct BrickCollider {
    pub phys: spall_physics::BodyId,
    /// The brick revision this collider was built from.
    pub built_revision: spall_core::Revision,
    /// `false` after the brick was emptied (the fixed body stays so a refill can reuse it).
    pub has_collider: bool,
}

/// All per-brick terrain colliders of a world.
#[derive(Debug, Default, Clone)]
pub(crate) struct TerrainBricks {
    pub bricks: BTreeMap<(i64, i64, i64), BrickCollider>,
}

/// A staged (pre-publish) collider change for one terrain brick.
#[derive(Debug, Clone)]
pub struct BrickPlan {
    pub coord: BrickCoord,
    pub revision: spall_core::Revision,
    /// `None`: the brick has no solid cell any more.
    pub plan: Option<ColliderPlan>,
}

fn key(c: BrickCoord) -> (i64, i64, i64) {
    (c.x, c.y, c.z)
}

/// Exact occupancy of one brick, or `None` if it holds no solid cell.
fn brick_grid(volume: &Volume, coord: BrickCoord) -> Result<Option<OccupancyGrid>, WorldError> {
    let min = spall_core::GlobalCell::new(coord.x * 32, coord.y * 32, coord.z * 32);
    let max = spall_core::GlobalCell::new(min.x + 31, min.y + 31, min.z + 31);
    let grid = OccupancyGrid::from_region(volume, min, max)?;
    Ok((grid.solid_count() > 0).then_some(grid))
}

/// Builds the collider plans for `bricks` from `volume` (the candidate volume during staging).
pub fn plan_terrain_bricks(
    volume: &Volume,
    bricks: &[BrickCoord],
) -> Result<Vec<BrickPlan>, WorldError> {
    let mut out = Vec::with_capacity(bricks.len());
    for &coord in bricks {
        let revision = volume
            .brick_revision(coord)
            .ok()
            .flatten()
            .unwrap_or(spall_core::Revision(0));
        let plan = match brick_grid(volume, coord)? {
            Some(grid) => Some(plan_collider(&grid)?),
            None => None,
        };
        out.push(BrickPlan {
            coord,
            revision,
            plan,
        });
    }
    Ok(out)
}

impl SimWorld {
    /// Whether the physics terrain is split into per-brick colliders.
    pub fn terrain_brick_colliders_enabled(&self) -> bool {
        self.terrain_bricks.is_some()
    }

    /// Every physics body that carries terrain collision (the single terrain body when the
    /// prototype is off).
    pub fn terrain_physics_bodies(&self) -> Vec<spall_physics::BodyId> {
        match &self.terrain_bricks {
            Some(t) => t.bricks.values().map(|b| b.phys).collect(),
            None => vec![self.terrain().phys],
        }
    }

    /// Whether `id` is (one of) the terrain's physics bodies.
    pub fn is_terrain_physics_body(&self, id: spall_physics::BodyId) -> bool {
        id == self.terrain().phys
            || self
                .terrain_bricks
                .as_ref()
                .is_some_and(|t| t.bricks.values().any(|b| b.phys == id))
    }

    /// Replaces the single whole-world terrain collider by one collider per solid brick.
    /// Idempotent. The authoritative terrain volume is not touched.
    pub fn enable_terrain_brick_colliders(&mut self) -> Result<(), WorldError> {
        if self.terrain_bricks.is_some() {
            return Ok(());
        }
        let coords = self.terrain().volume.resident_brick_coords();
        let plans = plan_terrain_bricks(&self.terrain().volume, &coords)?;
        // The single whole-world collider goes away only once every brick plan exists.
        let single = self.terrain().phys;
        self.physics_mut().remove_collider(single);
        self.terrain_bricks = Some(TerrainBricks::default());
        self.publish_terrain_brick_plans(plans);
        Ok(())
    }

    /// Applies staged plans. Infallible: everything that can fail happened in
    /// [`plan_terrain_bricks`].
    pub(crate) fn publish_terrain_brick_plans(&mut self, plans: Vec<BrickPlan>) {
        let cell_m = self.terrain().cell_size().metres() as f32;
        for p in plans {
            let k = key(p.coord);
            let existing = self
                .terrain_bricks
                .as_ref()
                .and_then(|t| t.bricks.get(&k))
                .cloned();
            match (existing, p.plan) {
                (Some(b), Some(plan)) => {
                    self.physics_mut()
                        .rebuild_collider(b.phys, &plan.grid, plan.representation);
                    if let Some(t) = self.terrain_bricks.as_mut() {
                        t.bricks.insert(
                            k,
                            BrickCollider {
                                phys: b.phys,
                                built_revision: p.revision,
                                has_collider: true,
                            },
                        );
                    }
                }
                (Some(b), None) => {
                    if b.has_collider {
                        self.physics_mut().remove_collider(b.phys);
                    }
                    if let Some(t) = self.terrain_bricks.as_mut() {
                        t.bricks.insert(
                            k,
                            BrickCollider {
                                phys: b.phys,
                                built_revision: p.revision,
                                has_collider: false,
                            },
                        );
                    }
                }
                (None, Some(plan)) => {
                    let phys = self.physics_mut().add_body(BodySpec {
                        kind: PhysBodyKind::Fixed,
                        representation: plan.representation,
                        grid: plan.grid,
                        cell_m,
                        density_kg_m3: 1.0,
                        mass_properties: None,
                        translation_m: [0.0; 3],
                        linvel_m_s: [0.0; 3],
                    });
                    if let Some(t) = self.terrain_bricks.as_mut() {
                        t.bricks.insert(
                            k,
                            BrickCollider {
                                phys,
                                built_revision: p.revision,
                                has_collider: true,
                            },
                        );
                    }
                }
                (None, None) => {}
            }
        }
    }

    /// Checks the per-brick colliders against the authoritative terrain: every solid brick has a
    /// collider built from its *current* revision, no collider is left for an empty brick, and
    /// the solid-cell count covered equals the volume's. `Err` names the first mismatch.
    pub fn validate_terrain_brick_colliders(&self) -> Result<(), String> {
        let Some(t) = &self.terrain_bricks else {
            return Ok(());
        };
        let volume = &self.terrain().volume;
        let mut covered = 0u64;
        for coord in volume.resident_brick_coords() {
            let grid = brick_grid(volume, coord).map_err(|e| e.to_string())?;
            let current = volume
                .brick_revision(coord)
                .ok()
                .flatten()
                .unwrap_or(spall_core::Revision(0));
            match (grid, t.bricks.get(&key(coord))) {
                (Some(g), Some(b)) if b.has_collider => {
                    if b.built_revision != current {
                        return Err(format!(
                            "brick {coord:?}: collider built from revision {:?}, volume is at {:?}",
                            b.built_revision, current
                        ));
                    }
                    covered += g.solid_count();
                }
                (Some(_), _) => {
                    return Err(format!("brick {coord:?} has solid cells but no collider"));
                }
                (None, Some(b)) if b.has_collider => {
                    return Err(format!("brick {coord:?} is empty but still has a collider"));
                }
                (None, _) => {}
            }
        }
        let solid = crate::world::solid_cells(volume);
        if covered != solid {
            return Err(format!(
                "colliders cover {covered} solid cells, the terrain volume has {solid}"
            ));
        }
        Ok(())
    }
}
