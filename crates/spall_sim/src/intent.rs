//! Accepted edit intents.
//!
//! Every player / bot / console action is turned into an [`EditIntent`] with a
//! unique [`RequestId`] *before* it reaches this crate. The brush is already
//! quantised into the target volume's local integer cell space, and for a moving
//! body that space is the body-local frame sampled at accept time
//! (`docs/architecture.md`). The server still owns whether the intent commits.

use spall_core::{MaterialId, SphereBrush};
use spall_protocol::RequestId;

/// What an intent does to the target volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Remove material inside the brush (set to air).
    Cut,
    /// Add `material` inside the brush.
    Place(MaterialId),
}

impl EditKind {
    /// The material an [`spall_voxel::EditPlan`] writes for this kind.
    pub fn write_material(self) -> MaterialId {
        match self {
            EditKind::Cut => MaterialId::AIR,
            EditKind::Place(m) => m,
        }
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
