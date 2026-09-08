//! An authoritative body: geometry (one [`Volume`]), a rigid pose, kinematic
//! state, and the opaque physics handle for its collider.
//!
//! Terrain is a body too — [`BodyKind::Terrain`], identity pose, a fixed
//! collider — so the edit / split / collider-swap path is literally the same
//! code for "cut the world" and "cut a falling building" (`docs/architecture.md`:
//! "Dynamic bodies use the same edit/split path").

use glam::{DQuat, DVec3};
use spall_core::{CellSizeCode, EntityId, GlobalCell, Pose, QuantizedQuat, VolumeId};
use spall_physics::BodyId as PhysBodyId;
use spall_voxel::{RigidXform, Volume};

/// Whether a body is the immovable world grid or a simulated detached volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// The world terrain grid: identity transform, fixed collider.
    Terrain,
    /// A detached, simulated voxel body.
    Dynamic,
}

/// A rigid pose in world space: `f64` metre translation (authority positions are
/// `f64`) and a normalised orientation.
#[derive(Debug, Clone, Copy)]
pub struct BodyPose {
    pub translation_m: [f64; 3],
    pub rotation: DQuat,
}

impl BodyPose {
    /// The identity pose — the terrain transform.
    pub fn identity() -> Self {
        Self {
            translation_m: [0.0; 3],
            rotation: DQuat::IDENTITY,
        }
    }

    /// A pose from a rotation (renormalised) and a translation.
    pub fn new(rotation: DQuat, translation_m: [f64; 3]) -> Self {
        let rotation = if rotation.is_finite() && rotation.length_squared() > f64::EPSILON {
            rotation.normalize()
        } else {
            DQuat::IDENTITY
        };
        Self {
            translation_m,
            rotation,
        }
    }

    /// The local-cell-space → world-metres transform for a volume of this cell
    /// size at this pose.
    pub fn xform(&self, cell_size: CellSizeCode) -> RigidXform {
        RigidXform::new(
            self.rotation,
            DVec3::from_array(self.translation_m),
            cell_size,
        )
    }

    /// World position of a point given in the body's local cell coordinates.
    pub fn local_cell_to_world_m(&self, local_cell: DVec3, cell_size: CellSizeCode) -> DVec3 {
        self.rotation * (local_cell * cell_size.metres()) + DVec3::from_array(self.translation_m)
    }

    /// The replicated [`Pose`]: `f64` translation, quantised orientation.
    pub fn to_protocol(&self) -> Pose {
        let q = self.rotation;
        let rotation = QuantizedQuat::from_unit(q.x as f32, q.y as f32, q.z as f32, q.w as f32)
            .unwrap_or(QuantizedQuat {
                x: 0,
                y: 0,
                z: 0,
                w: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap().w,
            });
        Pose {
            translation_m: self.translation_m,
            rotation,
        }
    }
}

/// One authoritative body.
#[derive(Debug, Clone)]
pub struct Body {
    /// `None` for terrain (canonical owner is the world grid, not an entity).
    pub entity: Option<EntityId>,
    pub volume_id: VolumeId,
    pub volume: Volume,
    pub kind: BodyKind,
    pub pose: BodyPose,
    /// Linear velocity of the body-centre, m/s.
    pub linvel_m_s: [f64; 3],
    /// Angular velocity, rad/s.
    pub angvel_rad_s: [f64; 3],
    pub sleeping: bool,
    /// Bumped on every collider rebuild; replication and persistence compare it.
    pub collider_revision: u64,
    /// Integer downsample factor the current collider was built at (`1` = exact,
    /// `2` / `4` = coarse-fracture fallback per `docs/collision-decision.md`).
    pub coarsen_k: u32,
    /// Opaque handle into the physics world.
    pub phys: PhysBodyId,
    /// Inclusive global-cell box the current collider covers. Edits inside it
    /// trigger a bounded collider rebuild over exactly this box.
    pub collider_region: (GlobalCell, GlobalCell),
}

impl Body {
    /// The volume's cell size.
    pub fn cell_size(&self) -> CellSizeCode {
        self.volume.cell_size()
    }

    /// `true` if `cell` lies within the collider region box.
    pub fn region_contains(&self, cell: GlobalCell) -> bool {
        let (lo, hi) = self.collider_region;
        (lo.x..=hi.x).contains(&cell.x)
            && (lo.y..=hi.y).contains(&cell.y)
            && (lo.z..=hi.z).contains(&cell.z)
    }
}
