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

/// Boxes one [`BoxReplace`] can carry: a freeze batches a region's boxes into
/// one commit, since every commit has a fixed cost.
pub const REPLACE_BOXES: usize = 8;
/// Largest extent (cells, per axis) of the bounds of a [`BoxReplace`]'s boxes.
const REPLACE_SPAN_CELLS: i64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellBox {
    min: GlobalCell,
    size: [u8; 3],
}

/// A box stored as an offset from the replace's origin (bounds stay within
/// [`REPLACE_SPAN_CELLS`], so offsets fit `i8`), keeping intents small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedBox {
    offset: [i8; 3],
    size: [u8; 3],
}

/// Every cell of up to [`REPLACE_BOXES`] axis-aligned boxes whose material is
/// one of `from` becomes `to` ([`MaterialId::AIR`] in `from` matches resident
/// air). The plan is read from the volume at staging, so it touches exactly the
/// cells that match then; a non-resident cell never matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxReplace {
    origin: GlobalCell,
    boxes: [PackedBox; REPLACE_BOXES],
    count: u8,
    pub from: [MaterialId; 2],
    pub to: MaterialId,
}

impl BoxReplace {
    /// One box from `min`, `size` cells along x, y and z (each at least 1).
    pub fn new(min: GlobalCell, size: [u8; 3], from: [MaterialId; 2], to: MaterialId) -> Self {
        let first = PackedBox {
            offset: [0; 3],
            size: size.map(|s| s.max(1)),
        };
        Self {
            origin: min,
            boxes: [first; REPLACE_BOXES],
            count: 1,
            from,
            to,
        }
    }

    /// Adds a box with the same materials, or returns false when the replace
    /// is full or the box is too far from the others.
    pub fn try_add(&mut self, min: GlobalCell, size: [u8; 3]) -> bool {
        if usize::from(self.count) == REPLACE_BOXES {
            return false;
        }
        let added = CellBox {
            min,
            size: size.map(|s| s.max(1)),
        };
        let (lo, hi) = bounds(self.boxes().iter().chain(std::iter::once(&added)));
        if (0..3).any(|i| hi[i] - lo[i] > REPLACE_SPAN_CELLS) {
            return false;
        }
        let offset = [
            min.x - self.origin.x,
            min.y - self.origin.y,
            min.z - self.origin.z,
        ];
        self.boxes[usize::from(self.count)] = PackedBox {
            offset: offset.map(|o| i8::try_from(o).expect("bounds keep offsets small")),
            size: added.size,
        };
        self.count += 1;
        true
    }

    /// Adds every box of `other` (same materials), or returns false and
    /// leaves `self` unchanged when they do not all fit.
    pub fn try_merge(&mut self, other: &BoxReplace) -> bool {
        if self.from != other.from || self.to != other.to {
            return false;
        }
        let saved = *self;
        for b in &other.boxes() {
            if !self.try_add(b.min, b.size) {
                *self = saved;
                return false;
            }
        }
        true
    }

    fn boxes(&self) -> Vec<CellBox> {
        self.boxes[..usize::from(self.count)]
            .iter()
            .map(|b| CellBox {
                min: GlobalCell::new(
                    self.origin.x + i64::from(b.offset[0]),
                    self.origin.y + i64::from(b.offset[1]),
                    self.origin.z + i64::from(b.offset[2]),
                ),
                size: b.size,
            })
            .collect()
    }

    /// The cells of the boxes that match `from` in `volume`, in `(z, y, x)`
    /// order.
    pub fn cells(&self, volume: &spall_voxel::Volume) -> Vec<GlobalCell> {
        let mut cells = Vec::new();
        for b in &self.boxes() {
            for dz in 0..i64::from(b.size[2]) {
                for dy in 0..i64::from(b.size[1]) {
                    for dx in 0..i64::from(b.size[0]) {
                        let cell = GlobalCell::new(b.min.x + dx, b.min.y + dy, b.min.z + dz);
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
        }
        cells.sort_unstable_by_key(|c| (c.z, c.y, c.x));
        cells.dedup();
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

    /// A sphere enclosing the boxes, for the pipeline's contention region and
    /// residency bounds.
    pub fn brush(&self) -> SphereBrush {
        let (lo, hi) = bounds(self.boxes().iter());
        let half = [0, 1, 2].map(|i| (hi[i] - lo[i]) * BRUSH_UNIT / 2);
        let centre = BrushPoint::from_units(
            lo[0] * BRUSH_UNIT + half[0],
            lo[1] * BRUSH_UNIT + half[1],
            lo[2] * BRUSH_UNIT + half[2],
        );
        let radius = ((half[0].pow(2) + half[1].pow(2) + half[2].pow(2)) as f64)
            .sqrt()
            .ceil() as i64;
        SphereBrush::new(centre, radius).expect("replace bounds fit the brush limit")
    }
}

/// Lowest cell and one past the highest cell of `boxes`.
fn bounds<'a>(boxes: impl Iterator<Item = &'a CellBox>) -> ([i64; 3], [i64; 3]) {
    let mut lo = [i64::MAX; 3];
    let mut hi = [i64::MIN; 3];
    for b in boxes {
        let min = [b.min.x, b.min.y, b.min.z];
        for i in 0..3 {
            lo[i] = lo[i].min(min[i]);
            hi[i] = hi[i].max(min[i] + i64::from(b.size[i]));
        }
    }
    (lo, hi)
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
