//! Terrain-to-body and body-to-body transfer.
//!
//! Given one [`ComponentMembership`] that a commit disconnected, this builds the
//! child body: a new [`Volume`] holding exactly the member cells **in the
//! parent's own cell frame**, a pose that reproduces the parent's transform (so
//! world-space geometry is unchanged at the split instant), inherited mass from
//! the fine voxel grid, and inherited velocity per `docs/architecture.md`:
//!
//! > Child linear velocity inherits parent velocity plus angular velocity cross
//! > the centre offset; angular velocity inherits the parent value before
//! > impulses. Apply declared explosion impulses once on the server.
//!
//! Geometry is **not** re-centred onto the child's centre of mass: keeping the
//! child's cells and transform identical to the parent's is the exact
//! preservation the architecture requires, and the physics adapter carries the
//! off-origin centre of mass itself.

use glam::{DQuat, DVec3};
use spall_core::{BrickCoord, CellSizeCode, EntityId, GlobalCell, MaterialId, Revision, VolumeId};
use spall_physics::{BodyMassProperties, OccupancyGrid, analytic_mass_properties};
use spall_structure::ComponentMembership;
use spall_voxel::{Brick, EditPlan, Sample, Volume};

use crate::body::BodyPose;
use crate::collider::{ColliderInfeasible, ColliderPlan, plan_collider};
use crate::intent::ExplosionImpulse;

/// Why [`plan_child`] could not produce an installable child body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanChildError {
    /// The child volume's occupancy could not be extracted.
    #[error(transparent)]
    Occupancy(#[from] spall_physics::ExtractError),
    /// The child is too fragmented / large for an exact active collider.
    #[error(transparent)]
    Collider(#[from] ColliderInfeasible),
}

/// The parent's kinematic state at the split instant, in world space.
#[derive(Debug, Clone, Copy)]
pub struct ParentState {
    pub pose: BodyPose,
    pub com_world_m: [f64; 3],
    pub linvel_m_s: [f64; 3],
    pub angvel_rad_s: [f64; 3],
    pub cell_size: CellSizeCode,
}

/// A fully-planned child body, ready to install.
#[derive(Debug, Clone)]
pub struct ChildBody {
    pub entity: EntityId,
    pub volume_id: VolumeId,
    pub volume: Volume,
    pub pose: BodyPose,
    pub linvel_m_s: [f64; 3],
    pub angvel_rad_s: [f64; 3],
    pub mass_kg: f64,
    pub com_world_m: [f64; 3],
    /// Exact mass / COM / inertia from the child's fine material grid, in the
    /// body-local frame the collider is built in. Installed into the physics
    /// body verbatim so the solver never derives mass from the (possibly
    /// coarsened) collision shape.
    pub mass_properties: BodyMassProperties,
    pub collider_plan: ColliderPlan,
    pub collider_grid_origin: GlobalCell,
    pub collider_region: (GlobalCell, GlobalCell),
    pub cell_count: u64,
}

/// Builds the child [`Volume`] for `membership` by copying the member cells'
/// materials out of `parent` (which must still hold them — call this before
/// removing them). The volume is bounded to the member bricks and every brick in
/// that box is made resident so occupancy extraction never hits an absent brick.
pub fn build_child_volume(
    parent: &Volume,
    membership: &ComponentMembership,
    child_id: VolumeId,
) -> Volume {
    let cells: Vec<GlobalCell> = membership.cells().collect();
    debug_assert!(!cells.is_empty(), "a split component has at least one cell");

    let mut min = BrickCoord::new(i64::MAX, i64::MAX, i64::MAX);
    let mut max = BrickCoord::new(i64::MIN, i64::MIN, i64::MIN);
    for cell in &cells {
        let b = cell.split().0;
        min = BrickCoord::new(min.x.min(b.x), min.y.min(b.y), min.z.min(b.z));
        max = BrickCoord::new(max.x.max(b.x), max.y.max(b.y), max.z.max(b.z));
    }

    let bounds = spall_voxel::BrickBounds::new(min, max).expect("min <= max by construction");
    let mut child = Volume::bounded(child_id, parent.cell_size(), bounds);
    for bz in min.z..=max.z {
        for by in min.y..=max.y {
            for bx in min.x..=max.x {
                child
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .expect("brick is within the bounds we just set");
            }
        }
    }

    let mut plan = EditPlan::new(child_id);
    for cell in &cells {
        let material = match parent.sample(*cell) {
            Ok(Sample::Filled(m)) => m,
            other => panic!("split member cell {cell:?} is not solid in the parent: {other:?}"),
        };
        plan.set(*cell, material);
    }
    child.apply_edit(&plan).expect("child fill is in bounds");
    child
}

/// Plans a child body from `membership`. Velocity is the inherited rigid-body
/// value only; a one-time [`ExplosionImpulse`] is added afterwards by
/// [`apply_explosion`] once every child's mass is known.
pub fn plan_child(
    parent_volume: &Volume,
    membership: &ComponentMembership,
    parent: ParentState,
    child_entity: EntityId,
    child_id: VolumeId,
    density: &impl Fn(MaterialId) -> f64,
    collider_region_pad_cells: i64,
) -> Result<ChildBody, PlanChildError> {
    let volume = build_child_volume(parent_volume, membership, child_id);
    let grid =
        OccupancyGrid::from_volume(&volume)?.expect("a split component always has a solid cell");
    let cell_m = parent.cell_size.metres();

    let mp = analytic_mass_properties(&grid, cell_m, density);
    let mass_kg = mp.mass_kg;
    let mass_properties = mp.to_body_properties();

    // Child COM in the parent's local metre frame: grid origin (in parent-local
    // cells) scaled to metres, plus the analytic COM offset within the grid.
    let origin = grid.origin();
    let child_com_local_m = DVec3::new(
        origin.x as f64 * cell_m + mp.com_m[0],
        origin.y as f64 * cell_m + mp.com_m[1],
        origin.z as f64 * cell_m + mp.com_m[2],
    );
    let q = parent.pose.rotation;
    let child_com_world = q * child_com_local_m + DVec3::from_array(parent.pose.translation_m);

    // v_child = v_parent + omega_parent x (r_child_com - r_parent_com)
    let omega = DVec3::from_array(parent.angvel_rad_s);
    let r = child_com_world - DVec3::from_array(parent.com_world_m);
    let linvel = DVec3::from_array(parent.linvel_m_s) + omega.cross(r);
    let angvel = omega;

    // The child body reproduces the parent's transform exactly.
    let pose = BodyPose::new(q, parent.pose.translation_m);

    let collider_plan = plan_collider(&grid)?;
    let region = padded_region(&grid, collider_region_pad_cells);

    Ok(ChildBody {
        entity: child_entity,
        volume_id: child_id,
        volume,
        pose,
        linvel_m_s: linvel.to_array(),
        angvel_rad_s: angvel.to_array(),
        mass_kg,
        com_world_m: child_com_world.to_array(),
        mass_properties,
        collider_plan,
        collider_grid_origin: origin,
        collider_region: region,
        cell_count: membership.cell_count,
    })
}

/// Adds a single [`ExplosionImpulse`] to every child of one commit, once. The
/// impulse is shared by mass, so each child gets the same delta-v along the
/// (normalised) direction: `delta_v = magnitude_ns / total_child_mass`.
pub fn apply_explosion(children: &mut [ChildBody], explosion: Option<ExplosionImpulse>) {
    let Some(ex) = explosion else { return };
    let dir = DVec3::from_array(ex.direction);
    let total: f64 = children.iter().map(|c| c.mass_kg).sum();
    if dir.length_squared() <= 0.0 || total <= 0.0 {
        return;
    }
    let delta = dir.normalize() * (ex.magnitude_ns / total);
    for child in children {
        let v = DVec3::from_array(child.linvel_m_s) + delta;
        child.linvel_m_s = v.to_array();
    }
}

/// World-space centre of mass of a body from its physics-derived local COM.
pub fn com_world(pose: BodyPose, local_com_m: [f32; 3]) -> [f64; 3] {
    let q: DQuat = pose.rotation;
    let world = q * DVec3::new(
        f64::from(local_com_m[0]),
        f64::from(local_com_m[1]),
        f64::from(local_com_m[2]),
    ) + DVec3::from_array(pose.translation_m);
    world.to_array()
}

fn padded_region(grid: &OccupancyGrid, pad: i64) -> (GlobalCell, GlobalCell) {
    let o = grid.origin();
    let d = grid.dims();
    (
        GlobalCell::new(o.x - pad, o.y - pad, o.z - pad),
        GlobalCell::new(
            o.x + d[0] as i64 - 1 + pad,
            o.y + d[1] as i64 - 1 + pad,
            o.z + d[2] as i64 - 1 + pad,
        ),
    )
}
