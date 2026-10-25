//! Accepted edit intents.
//!
//! Every player / bot / console action is turned into an [`EditIntent`] with a
//! unique [`RequestId`] *before* it reaches this crate. The brush is already
//! quantised into the target volume's local integer cell space, and for a moving
//! body that space is the body-local frame sampled at accept time
//! (`docs/architecture.md`). The server still owns whether the intent commits.

use spall_core::{BRUSH_UNIT, BrushPoint, GlobalCell, MaterialId, SphereBrush};
use spall_protocol::RequestId;

/// What an intent does to the target volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Remove material inside the brush (set to air).
    Cut,
    /// Add `material` inside the brush.
    Place(MaterialId),
    /// Turn a voxel door around its authored hinge.
    Door(spall_voxel::doors::DoorSpec),
    /// Rewrite the cells of a box that hold given materials (server-authored:
    /// winter ice forming, cracking and breaking, ENG-268).
    Replace(BoxReplace),
}

impl EditKind {
    /// The material an [`spall_voxel::EditPlan`] writes for this kind.
    pub fn write_material(self) -> MaterialId {
        match self {
            EditKind::Cut => MaterialId::AIR,
            EditKind::Place(m) => m,
            EditKind::Door(spec) => spec.panel,
            EditKind::Replace(replace) => replace.to,
        }
    }
}

/// Every cell of an axis-aligned box whose material is one of `from` becomes
/// `to` ([`MaterialId::AIR`] in `from` matches resident air). The plan is read
/// from the volume at staging, so it touches exactly the cells that match then;
/// a non-resident cell never matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxReplace {
    /// Lowest corner of the box (terrain cells).
    pub min: GlobalCell,
    /// Cells along x, y and z; each at least 1.
    pub size: [u8; 3],
    pub from: [MaterialId; 2],
    pub to: MaterialId,
}

impl BoxReplace {
    /// The cells of the box that match `from` in `volume`, `+X` fastest.
    pub fn cells(&self, volume: &spall_voxel::Volume) -> Vec<GlobalCell> {
        let mut cells = Vec::new();
        for dz in 0..i64::from(self.size[2]) {
            for dy in 0..i64::from(self.size[1]) {
                for dx in 0..i64::from(self.size[0]) {
                    let cell = GlobalCell::new(self.min.x + dx, self.min.y + dy, self.min.z + dz);
                    let material = match volume.sample(cell) {
                        Ok(spall_voxel::Sample::Empty { .. }) => MaterialId::AIR,
                        Ok(spall_voxel::Sample::Filled(m)) => m,
                        _ => continue,
                    };
                    if self.from.contains(&material) && material != self.to {
                        cells.push(cell);
                    }
                }
            }
        }
        cells
    }

    /// The edit plan for `volume` (see [`Self::cells`]).
    pub fn plan(&self, volume: &spall_voxel::Volume) -> spall_voxel::EditPlan {
        let mut plan = spall_voxel::EditPlan::new(volume.id());
        for cell in self.cells(volume) {
            plan.set(cell, self.to);
        }
        plan
    }

    /// A sphere enclosing the box, for the pipeline's contention region and
    /// residency bounds.
    pub fn brush(&self) -> SphereBrush {
        let half = self.size.map(|s| i64::from(s.max(1)) * BRUSH_UNIT / 2);
        let centre = BrushPoint::from_units(
            self.min.x * BRUSH_UNIT + half[0],
            self.min.y * BRUSH_UNIT + half[1],
            self.min.z * BRUSH_UNIT + half[2],
        );
        let radius = ((half[0].pow(2) + half[1].pow(2) + half[2].pow(2)) as f64)
            .sqrt()
            .ceil() as i64;
        SphereBrush::new(centre, radius).expect("a u8-sized box fits the brush limit")
    }
}

/// Which volume an intent edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    /// The world terrain grid.
    Terrain,
    /// A detached body, addressed by its stable entity id.
    Body(spall_core::EntityId),
}

/// A one-shot outward impulse applied to the components a commit detaches. It is
/// applied exactly once, at the single successful commit — a recompute / retry
/// never re-applies it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExplosionImpulse {
    /// Impulse magnitude, newton-seconds, shared across detached children by
    /// mass.
    pub magnitude_ns: f64,
    /// World-space direction (need not be normalised; a zero vector is ignored).
    pub direction: [f64; 3],
}

/// An accepted edit, ready to stage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditIntent {
    pub request_id: RequestId,
    /// The acting entity (a player / tool / console). Informational for the
    /// journal; authority is the server's.
    pub actor: spall_core::EntityId,
    pub target: EditTarget,
    pub kind: EditKind,
    /// Brush in the target volume's local fixed-point cell units.
    pub brush: SphereBrush,
    /// Optional one-time detachment impulse.
    pub explosion: Option<ExplosionImpulse>,
}

impl EditIntent {
    pub fn cut(
        request_id: RequestId,
        actor: spall_core::EntityId,
        target: EditTarget,
        brush: SphereBrush,
    ) -> Self {
        Self {
            request_id,
            actor,
            target,
            kind: EditKind::Cut,
            brush,
            explosion: None,
        }
    }

    #[must_use]
    pub fn with_explosion(mut self, explosion: ExplosionImpulse) -> Self {
        self.explosion = Some(explosion);
        self
    }
}

/// Why an intent could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("intent queue is full ({limit} pending)")]
    QueueFull { limit: usize },
    #[error("request id {0:?} was already accepted")]
    DuplicateRequest(RequestId),
    #[error("intent targets body {0} which does not exist")]
    UnknownBody(spall_core::EntityId),
}
