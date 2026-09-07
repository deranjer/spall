//! Voxel ray queries: a 3D DDA (Amanatides & Woo) grid traversal over a
//! [`Volume`], plus a world-space entry point for transformed volumes.
//!
//! These queries are for previews, tool aiming, and CPU picking. Authoritative
//! hit validation stays server-side against committed geometry — the results
//! here are not required to be bit-identical across machines.
//!
//! ## Defined behaviour
//!
//! * **Direction** need not be normalised. A zero or non-finite direction is a
//!   [`RayError`]; the reported `t` is always distance along the *normalised*
//!   heading.
//! * **Zero components** are fine: an axis with `dir == 0` simply never steps.
//! * **Start inside a solid cell** returns a hit at `t = 0` with `face = None`
//!   and `placement_cell == cell`.
//! * **Exact boundaries**: the origin coordinate `k.0` belongs to cell `k`
//!   (floor). When two or three axis crossings coincide (a ray through a cell
//!   edge or corner) the step order is a fixed priority **X, then Y, then Z**.
//! * **Finite range**: traversal stops at [`RayConfig::max_distance`] (cells for
//!   [`cast_ray`], metres for [`cast_ray_world`]) and at a hard
//!   [`RayConfig::max_steps`] iteration cap; either yields [`RayOutcome::Miss`].
//! * **Unloaded data**: entering a resident-but-unavailable brick yields
//!   [`RayOutcome::Unknown`], never a miss — an unloaded brick is not empty.
//!   Cells outside a bounded volume's `BrickBounds` *are* treated as empty for
//!   traversal, so a ray may cross open space into the volume.

use glam::DVec3;

use spall_core::{GlobalCell, MaterialId};

use crate::transform::RigidXform;
use crate::volume::{AccessError, Residency, Sample, Volume};

/// The face of a struck cell that a ray entered through. Its [`Face::normal`]
/// points back toward the ray origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    PosX,
    NegX,
    PosY,
    NegY,
    PosZ,
    NegZ,
}

impl Face {
    /// Outward unit normal as an integer triple.
    pub const fn normal(self) -> [i64; 3] {
        match self {
            Face::PosX => [1, 0, 0],
            Face::NegX => [-1, 0, 0],
            Face::PosY => [0, 1, 0],
            Face::NegY => [0, -1, 0],
            Face::PosZ => [0, 0, 1],
            Face::NegZ => [0, 0, -1],
        }
    }

    /// Outward normal as a floating-point vector (in the volume's local space).
    pub fn normal_vec(self) -> DVec3 {
        let [x, y, z] = self.normal();
        DVec3::new(x as f64, y as f64, z as f64)
    }

    /// The face reached by stepping `step` (`+1` / `-1`) along `axis` (`0..3`).
    /// Stepping `+X` enters a cell through its `-X` face.
    const fn entered(axis: usize, step: i64) -> Face {
        match (axis, step) {
            (0, s) if s > 0 => Face::NegX,
            (0, _) => Face::PosX,
            (1, s) if s > 0 => Face::NegY,
            (1, _) => Face::PosY,
            (2, s) if s > 0 => Face::NegZ,
            _ => Face::PosZ,
        }
    }
}

/// A ray in some coordinate space (cell space for [`cast_ray`], world metres for
/// [`cast_ray_world`]).
#[derive(Debug, Clone, Copy)]
pub struct Ray {
    pub origin: DVec3,
    /// Heading; need not be unit length. Zero / non-finite is rejected.
    pub dir: DVec3,
}

impl Ray {
    pub fn new(origin: DVec3, dir: DVec3) -> Self {
        Self { origin, dir }
    }
}

/// Traversal bounds. `max_distance` is in the ray's own units (cells for
/// [`cast_ray`]); `max_steps` is a hard cap on DDA iterations so a grazing ray
/// near a boundary cannot spin.
#[derive(Debug, Clone, Copy)]
pub struct RayConfig {
    pub max_distance: f64,
    pub max_steps: u32,
}

impl RayConfig {
    /// A config limited to `max_distance` units with a step cap derived from it.
    /// A normalised ray crosses at most `|dx| + |dy| + |dz| <= sqrt(3) < 2`
    /// grid planes per unit length, so `2 * distance + 8` always suffices.
    pub fn new(max_distance: f64) -> Self {
        let steps = (max_distance.max(0.0) * 2.0).ceil() as u64 + 8;
        Self {
            max_distance,
            max_steps: steps.min(u64::from(u32::MAX)) as u32,
        }
    }
}

/// First solid cell a ray meets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayHit {
    /// The solid cell struck.
    pub cell: GlobalCell,
    pub material: MaterialId,
    /// The face `cell` was entered through, or `None` if the ray began inside a
    /// solid cell.
    pub face: Option<Face>,
    /// The empty cell against `face` — `cell + face.normal()` — where a
    /// placement tool would add material. Equal to `cell` for a start-inside
    /// hit.
    pub placement_cell: GlobalCell,
    /// Distance from the origin along the normalised heading to the face
    /// crossing; `0.0` for a start-inside hit. Metres for [`cast_ray_world`],
    /// cells for [`cast_ray`].
    pub t: f64,
    /// DDA iterations performed.
    pub steps: u32,
}

/// Why a traversal stopped without a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    /// Passed [`RayConfig::max_distance`].
    OutOfRange,
    /// Hit [`RayConfig::max_steps`].
    StepLimit,
}

/// Outcome of a ray query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RayOutcome {
    Hit(RayHit),
    Miss {
        steps: u32,
        reason: MissReason,
    },
    /// Traversal reached a brick that is not available. The caller must not
    /// treat this as empty space.
    Unknown {
        cell: GlobalCell,
        residency: Residency,
        steps: u32,
    },
}

/// Why a ray could not be cast at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RayError {
    #[error("ray direction is zero or too short to define a heading")]
    ZeroDirection,
    #[error("ray origin or direction has a non-finite component")]
    NonFinite,
    #[error("ray max_distance must be finite and greater than zero")]
    BadRange,
    #[error("ray origin lies outside the representable integer-cell range")]
    OriginOutOfRange,
}

const CELL_RANGE_LIMIT: f64 = 4.611_686_018_427_388e18; // 2^62

/// Cast a ray through `volume` in the volume's **local cell space**. Use this
/// for terrain (identity transform) or when the caller has already mapped the
/// ray into local space.
pub fn cast_ray(volume: &Volume, ray: Ray, cfg: RayConfig) -> Result<RayOutcome, RayError> {
    traverse(volume, ray, cfg)
}

/// Cast a **world-space** ray (origin and heading in metres) against a volume
/// placed by `xform`. `max_distance_m` bounds the ray in metres. The returned
/// [`RayHit::t`] is in metres and [`RayHit::face`] is in the volume's local
/// space (rotate with [`RigidXform::local_dir_to_world`] for a world normal).
pub fn cast_ray_world(
    volume: &Volume,
    xform: &RigidXform,
    ray: Ray,
    max_distance_m: f64,
) -> Result<RayOutcome, RayError> {
    if !(max_distance_m.is_finite() && max_distance_m > 0.0) {
        return Err(RayError::BadRange);
    }
    let cell_m = xform.cell_size_m();
    let local = Ray {
        origin: xform.world_to_local_cell(ray.origin),
        dir: xform.world_dir_to_local(ray.dir),
    };
    let cfg = RayConfig::new(max_distance_m / cell_m);
    Ok(match traverse(volume, local, cfg)? {
        RayOutcome::Hit(mut hit) => {
            hit.t *= cell_m;
            RayOutcome::Hit(hit)
        }
        other => other,
    })
}

fn traverse(volume: &Volume, ray: Ray, cfg: RayConfig) -> Result<RayOutcome, RayError> {
    if !ray.origin.is_finite() || !ray.dir.is_finite() {
        return Err(RayError::NonFinite);
    }
    if !(cfg.max_distance.is_finite() && cfg.max_distance > 0.0) {
        return Err(RayError::BadRange);
    }
    let len = ray.dir.length();
    if !len.is_finite() || len < 1e-12 {
        return Err(RayError::ZeroDirection);
    }
    let dir = (ray.dir / len).to_array();
    let origin = ray.origin.to_array();

    let mut cell = [
        floor_i64(origin[0]).ok_or(RayError::OriginOutOfRange)?,
        floor_i64(origin[1]).ok_or(RayError::OriginOutOfRange)?,
        floor_i64(origin[2]).ok_or(RayError::OriginOutOfRange)?,
    ];

    // First cell: start-inside solid, or an unavailable brick.
    match sample_cell(volume, cell) {
        CellSample::Unknown(residency) => {
            return Ok(RayOutcome::Unknown {
                cell: to_global(cell),
                residency,
                steps: 0,
            });
        }
        CellSample::Filled(material) => {
            let g = to_global(cell);
            return Ok(RayOutcome::Hit(RayHit {
                cell: g,
                material,
                face: None,
                placement_cell: g,
                t: 0.0,
                steps: 0,
            }));
        }
        CellSample::Empty => {}
    }

    let mut step = [0i64; 3];
    let mut t_max = [f64::INFINITY; 3];
    let mut t_delta = [f64::INFINITY; 3];
    for a in 0..3 {
        if dir[a] > 0.0 {
            step[a] = 1;
            t_delta[a] = 1.0 / dir[a];
            t_max[a] = (cell[a] as f64 + 1.0 - origin[a]) / dir[a];
        } else if dir[a] < 0.0 {
            step[a] = -1;
            t_delta[a] = 1.0 / -dir[a];
            t_max[a] = (origin[a] - cell[a] as f64) / -dir[a];
        }
    }

    let mut steps = 0u32;
    loop {
        // Smallest crossing, ties broken X < Y < Z.
        let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            0
        } else if t_max[1] <= t_max[2] {
            1
        } else {
            2
        };
        let t_cross = t_max[axis];
        // `t_cross` is finite or +inf here; +inf fails this and misses cleanly.
        if t_cross > cfg.max_distance {
            return Ok(RayOutcome::Miss {
                steps,
                reason: MissReason::OutOfRange,
            });
        }
        if steps >= cfg.max_steps {
            return Ok(RayOutcome::Miss {
                steps,
                reason: MissReason::StepLimit,
            });
        }

        let Some(next) = cell[axis].checked_add(step[axis]) else {
            return Ok(RayOutcome::Miss {
                steps,
                reason: MissReason::OutOfRange,
            });
        };
        cell[axis] = next;
        t_max[axis] += t_delta[axis];
        steps += 1;

        let face = Face::entered(axis, step[axis]);
        match sample_cell(volume, cell) {
            CellSample::Unknown(residency) => {
                return Ok(RayOutcome::Unknown {
                    cell: to_global(cell),
                    residency,
                    steps,
                });
            }
            CellSample::Filled(material) => {
                let g = to_global(cell);
                let n = face.normal();
                return Ok(RayOutcome::Hit(RayHit {
                    cell: g,
                    material,
                    face: Some(face),
                    placement_cell: GlobalCell::new(g.x + n[0], g.y + n[1], g.z + n[2]),
                    t: t_cross,
                    steps,
                }));
            }
            CellSample::Empty => {}
        }
    }
}

enum CellSample {
    Unknown(Residency),
    Empty,
    Filled(MaterialId),
}

fn sample_cell(volume: &Volume, cell: [i64; 3]) -> CellSample {
    match volume.sample(to_global(cell)) {
        Ok(Sample::Filled(material)) => CellSample::Filled(material),
        Ok(Sample::Empty { .. }) => CellSample::Empty,
        Ok(Sample::Unknown(residency)) => CellSample::Unknown(residency),
        // Outside a bounded volume there is simply no geometry: traversable.
        Err(AccessError::OutOfBounds { .. }) => CellSample::Empty,
        Err(AccessError::BadCellIndex(_)) => unreachable!("GlobalCell::split always yields 0..32"),
    }
}

fn to_global(cell: [i64; 3]) -> GlobalCell {
    GlobalCell::new(cell[0], cell[1], cell[2])
}

fn floor_i64(v: f64) -> Option<i64> {
    let f = v.floor();
    (f.is_finite() && f.abs() <= CELL_RANGE_LIMIT).then_some(f as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::Brick;
    use crate::edit::EditPlan;
    use crate::fixtures;
    use crate::volume::Volume;
    use glam::DQuat;
    use spall_core::{BrickCoord, CellSizeCode, Revision, VolumeId};

    const STONE: MaterialId = MaterialId(1);

    fn down_ray(y: f64) -> Ray {
        Ray::new(DVec3::new(0.5, y, 0.5), DVec3::new(0.0, -1.0, 0.0))
    }

    #[test]
    fn downward_ray_hits_the_terrain_surface_with_the_top_face() {
        let terrain = fixtures::flat_terrain(VolumeId::new(1).unwrap());
        let out = cast_ray(&terrain, down_ray(40.0), RayConfig::new(64.0)).unwrap();
        let RayOutcome::Hit(hit) = out else {
            panic!("expected a hit, got {out:?}");
        };
        assert_eq!(hit.cell, GlobalCell::new(0, 31, 0));
        assert_eq!(hit.face, Some(Face::PosY));
        assert_eq!(hit.placement_cell, GlobalCell::new(0, 32, 0));
        assert!((hit.t - 8.0).abs() < 1e-9, "t = {}", hit.t);
    }

    #[test]
    fn exact_boundary_origin_is_owned_by_the_upper_cell() {
        // origin.y is exactly 32.0 — the plane between the air brick and the
        // stone brick. floor(32.0) == 32, an air cell, so the first step is
        // immediate and the hit is still the stone top face.
        let terrain = fixtures::flat_terrain(VolumeId::new(1).unwrap());
        let out = cast_ray(&terrain, down_ray(32.0), RayConfig::new(8.0)).unwrap();
        let RayOutcome::Hit(hit) = out else {
            panic!("expected a hit, got {out:?}");
        };
        assert_eq!(hit.cell, GlobalCell::new(0, 31, 0));
        assert_eq!(hit.face, Some(Face::PosY));
        assert!((hit.t - 0.0).abs() < 1e-9);
    }

    #[test]
    fn negative_coordinate_ray_hits_a_wall_with_a_signed_cell() {
        let tower = fixtures::hollow_tower(VolumeId::new(7).unwrap());
        // Start in the hollow interior, head -X into the west wall.
        let ray = Ray::new(DVec3::new(-3.5, 0.5, 0.5), DVec3::new(-1.0, 0.0, 0.0));
        let out = cast_ray(&tower, ray, RayConfig::new(16.0)).unwrap();
        let RayOutcome::Hit(hit) = out else {
            panic!("expected a hit, got {out:?}");
        };
        assert_eq!(hit.cell, GlobalCell::new(-5, 0, 0));
        assert_eq!(hit.material, STONE);
        assert_eq!(hit.face, Some(Face::PosX));
        assert_eq!(hit.face.unwrap().normal(), [1, 0, 0]);
        assert_eq!(hit.face.unwrap().normal_vec(), DVec3::new(1.0, 0.0, 0.0));
        assert_eq!(hit.placement_cell, GlobalCell::new(-4, 0, 0));
        assert!((hit.t - 0.5).abs() < 1e-9, "t = {}", hit.t);
    }

    #[test]
    fn diagonal_corner_crossing_steps_x_before_y() {
        // Two solid cells: (2,1,0) lies on the X-first staircase from (0,0,.5)
        // along (1,1,0); (2,2,0) lies on the Y-first staircase. The tie-break
        // must pick (2,1,0).
        let mut v = Volume::new(VolumeId::new(3).unwrap(), CellSizeCode::Quarter);
        let mut plan = EditPlan::new(v.id());
        plan.set(GlobalCell::new(2, 1, 0), STONE);
        plan.set(GlobalCell::new(2, 2, 0), STONE);
        v.apply_edit(&plan).unwrap();

        let ray = Ray::new(DVec3::new(0.0, 0.0, 0.5), DVec3::new(1.0, 1.0, 0.0));
        let RayOutcome::Hit(hit) = cast_ray(&v, ray, RayConfig::new(10.0)).unwrap() else {
            panic!("expected a hit");
        };
        assert_eq!(hit.cell, GlobalCell::new(2, 1, 0));
        assert_eq!(hit.face, Some(Face::NegX));
        assert_eq!(hit.placement_cell, GlobalCell::new(1, 1, 0));
        assert_eq!(hit.steps, 3);
    }

    #[test]
    fn start_inside_solid_reports_zero_distance_and_no_face() {
        let mut v = Volume::new(VolumeId::new(3).unwrap(), CellSizeCode::Quarter);
        v.insert_brick(BrickCoord::new(0, 0, 0), Brick::uniform(STONE, Revision(1)))
            .unwrap();
        let ray = Ray::new(DVec3::new(4.5, 4.5, 4.5), DVec3::new(1.0, 0.2, -0.3));
        let RayOutcome::Hit(hit) = cast_ray(&v, ray, RayConfig::new(10.0)).unwrap() else {
            panic!("expected a hit");
        };
        assert_eq!(hit.cell, GlobalCell::new(4, 4, 4));
        assert_eq!(hit.face, None);
        assert_eq!(hit.placement_cell, hit.cell);
        assert_eq!(hit.t, 0.0);
        assert_eq!(hit.steps, 0);
    }

    #[test]
    fn finite_range_stops_short_of_a_far_surface() {
        let terrain = fixtures::flat_terrain(VolumeId::new(1).unwrap());
        // Surface is 8 cells below the origin; cap the ray at 4.
        let near = cast_ray(&terrain, down_ray(40.0), RayConfig::new(4.0)).unwrap();
        assert!(
            matches!(
                near,
                RayOutcome::Miss {
                    reason: MissReason::OutOfRange,
                    ..
                }
            ),
            "got {near:?}"
        );
        // And reaches it when allowed the distance.
        assert!(matches!(
            cast_ray(&terrain, down_ray(40.0), RayConfig::new(9.0)).unwrap(),
            RayOutcome::Hit(_)
        ));
    }

    #[test]
    fn step_limit_is_a_miss_not_a_hang() {
        let terrain = fixtures::flat_terrain(VolumeId::new(1).unwrap());
        let cfg = RayConfig {
            max_distance: 1000.0,
            max_steps: 2,
        };
        assert!(matches!(
            cast_ray(&terrain, down_ray(40.0), cfg).unwrap(),
            RayOutcome::Miss {
                reason: MissReason::StepLimit,
                steps: 2,
            }
        ));
    }

    #[test]
    fn unloaded_brick_is_unknown_not_a_miss() {
        let v = Volume::new(VolumeId::new(9).unwrap(), CellSizeCode::Quarter);
        let ray = Ray::new(DVec3::new(0.5, 0.5, 0.5), DVec3::new(1.0, 0.0, 0.0));
        assert!(matches!(
            cast_ray(&v, ray, RayConfig::new(10.0)).unwrap(),
            RayOutcome::Unknown {
                residency: Residency::Absent,
                steps: 0,
                ..
            }
        ));
    }

    #[test]
    fn empty_loaded_volume_misses() {
        let mut v = Volume::new(VolumeId::new(9).unwrap(), CellSizeCode::Quarter);
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
        let ray = Ray::new(DVec3::new(0.5, 0.5, 0.5), DVec3::new(0.0, 0.0, 1.0));
        assert!(matches!(
            cast_ray(&v, ray, RayConfig::new(20.0)).unwrap(),
            RayOutcome::Miss {
                reason: MissReason::OutOfRange,
                ..
            }
        ));
    }

    #[test]
    fn zero_and_non_finite_directions_are_rejected() {
        let terrain = fixtures::flat_terrain(VolumeId::new(1).unwrap());
        assert_eq!(
            cast_ray(
                &terrain,
                Ray::new(DVec3::new(0.5, 40.0, 0.5), DVec3::ZERO),
                RayConfig::new(4.0)
            ),
            Err(RayError::ZeroDirection)
        );
        assert_eq!(
            cast_ray(
                &terrain,
                Ray::new(DVec3::new(0.5, 40.0, 0.5), DVec3::new(f64::NAN, 1.0, 0.0)),
                RayConfig::new(4.0)
            ),
            Err(RayError::NonFinite)
        );
    }

    #[test]
    fn world_ray_and_local_ray_agree_on_a_rotated_translated_volume() {
        let tower = fixtures::hollow_tower(VolumeId::new(7).unwrap());
        let local = Ray::new(DVec3::new(-3.5, 0.5, 0.5), DVec3::new(-1.0, 0.0, 0.0));
        let local_hit = match cast_ray(&tower, local, RayConfig::new(16.0)).unwrap() {
            RayOutcome::Hit(h) => h,
            other => panic!("local: {other:?}"),
        };

        let rot = DQuat::from_axis_angle(DVec3::Y, std::f64::consts::FRAC_PI_2);
        let xform = RigidXform::new(rot, DVec3::new(100.0, -5.0, 20.0), CellSizeCode::Quarter);
        let world = Ray::new(
            xform.local_cell_to_world_m(DVec3::new(-3.5, 0.5, 0.5)),
            xform.local_dir_to_world(DVec3::new(-1.0, 0.0, 0.0)),
        );
        let world_hit = match cast_ray_world(&tower, &xform, world, 8.0).unwrap() {
            RayOutcome::Hit(h) => h,
            other => panic!("world: {other:?}"),
        };

        assert_eq!(world_hit.cell, local_hit.cell);
        assert_eq!(world_hit.face, local_hit.face);
        // local t is in cells; world t is in metres.
        assert!(
            (world_hit.t - local_hit.t * 0.25).abs() < 1e-6,
            "world t {} vs local t {} cells",
            world_hit.t,
            local_hit.t
        );
    }
}
