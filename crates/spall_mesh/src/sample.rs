//! Cell occupancy sampling for meshing, including the one-cell halo across brick
//! seams.

use spall_core::{BrickCoord, CELLS_PER_BRICK, GlobalCell, LocalCell, MaterialId};
use spall_voxel::{AccessError, Residency, Sample, Volume};

/// Why a volume's meshing work set could not be planned.
///
/// Meshing enumerates the cells of every resident brick. That work is bounded by
/// the resident data, but two failure modes are still reported explicitly rather
/// than papered over: a brick so far out that its cell coordinates leave `i64`,
/// and a resident set larger than the caller's work budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MeshError {
    /// A resident brick's `32^3` cell extent overflows `i64`.
    #[error("brick ({},{},{}) maps to cell coordinates that overflow i64", coord.x, coord.y, coord.z)]
    CoordinateOverflow { coord: BrickCoord },
    /// The resident set needs more cell visits than the configured budget.
    #[error("meshing would visit {cell_visits} cells, exceeding the budget of {budget}")]
    WorkBudgetExceeded { cell_visits: u128, budget: u128 },
}

/// What a meshing query found at one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// A resident solid cell.
    Solid(MaterialId),
    /// Resident air, or open space outside a bounded volume — a face against
    /// this neighbour is exposed and fully resolved.
    Open,
    /// The brick is not available. A face against this neighbour is still
    /// emitted (a wall that might be occluded is safer than a hole), but the
    /// brick is recorded as a halo dependency so the mesh is rebuilt when it
    /// arrives.
    Unknown(Residency),
}

impl Occupancy {
    #[inline]
    pub fn is_solid(self) -> bool {
        matches!(self, Occupancy::Solid(_))
    }

    /// True when a solid cell's face toward a neighbour in this state is
    /// exposed and must be emitted.
    #[inline]
    pub fn exposes_face(self) -> bool {
        !self.is_solid()
    }
}

/// A read-only occupancy view over one [`Volume`]. Out-of-bounds cells read as
/// [`Occupancy::Open`], matching the ray traversal in `spall_voxel`.
#[derive(Debug, Clone, Copy)]
pub struct VolumeSampler<'a> {
    volume: &'a Volume,
}

impl<'a> VolumeSampler<'a> {
    pub fn new(volume: &'a Volume) -> Self {
        Self { volume }
    }

    pub fn volume(&self) -> &'a Volume {
        self.volume
    }

    /// Occupancy of one global cell.
    #[inline]
    pub fn at(&self, cell: [i64; 3]) -> Occupancy {
        match self
            .volume
            .sample(GlobalCell::new(cell[0], cell[1], cell[2]))
        {
            Ok(Sample::Filled(material)) => Occupancy::Solid(material),
            Ok(Sample::Empty { .. }) => Occupancy::Open,
            Ok(Sample::Unknown(residency)) => Occupancy::Unknown(residency),
            Err(AccessError::OutOfBounds { .. }) => Occupancy::Open,
            Err(AccessError::BadCellIndex(_) | AccessError::RevisionExhausted { .. }) => {
                unreachable!("sampling a GlobalCell never allocates or mis-indexes")
            }
        }
    }

    /// True when `cell` is a resident solid cell.
    #[inline]
    pub fn is_solid(&self, cell: [i64; 3]) -> bool {
        self.at(cell).is_solid()
    }
}

/// Inclusive integer cell bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellBox {
    pub min: [i64; 3],
    pub max: [i64; 3],
}

impl CellBox {
    /// The `32^3` inclusive cell box of a single brick, or `None` when the
    /// brick sits so far out that its cell coordinates overflow `i64`. Uses the
    /// same checked coordinate math as [`GlobalCell::from_parts`].
    pub fn of_brick(coord: BrickCoord) -> Option<Self> {
        let lo = GlobalCell::from_parts(coord, local(0, 0, 0)).ok()?;
        let hi = GlobalCell::from_parts(coord, local(31, 31, 31)).ok()?;
        Some(Self {
            min: [lo.x, lo.y, lo.z],
            max: [hi.x, hi.y, hi.z],
        })
    }

    /// The tight cell box covering every resident brick of `volume`
    /// (`32^3` cells per brick), or `None` if no brick is resident or a brick's
    /// cell extent overflows `i64`.
    ///
    /// This is the *bounding hull* of the resident set. Enumerating it visits
    /// every cell between far-apart bricks, so it must not be handed to a face
    /// emitter for sparse input — use [`ResidentCells`] for that. It remains
    /// useful for dense test volumes and for reporting how large the hull is.
    pub fn of_resident(volume: &Volume) -> Option<Self> {
        let mut coords = volume.resident_brick_coords().into_iter();
        let mut acc = Self::of_brick(coords.next()?)?;
        for c in coords {
            let b = Self::of_brick(c)?;
            for a in 0..3 {
                acc.min[a] = acc.min[a].min(b.min[a]);
                acc.max[a] = acc.max[a].max(b.max[a]);
            }
        }
        Some(acc)
    }

    /// Number of cells in this inclusive box, in `u128` so a wide hull box
    /// cannot overflow the count.
    pub fn cell_count(&self) -> u128 {
        let span =
            |a: usize| (i128::from(self.max[a]) - i128::from(self.min[a]) + 1).max(0) as u128;
        span(0) * span(1) * span(2)
    }

    /// Iterate the contained cells in canonical `(z, y, x)` order.
    pub fn cells(&self) -> impl Iterator<Item = [i64; 3]> + '_ {
        (self.min[2]..=self.max[2]).flat_map(move |z| {
            (self.min[1]..=self.max[1])
                .flat_map(move |y| (self.min[0]..=self.max[0]).map(move |x| [x, y, z]))
        })
    }
}

#[inline]
fn local(x: u8, y: u8, z: u8) -> LocalCell {
    LocalCell::new(x, y, z).expect("0..=31 is in range")
}

/// The cells a volume mesh must visit: one `32^3` box per resident brick, in
/// canonical brick order.
///
/// Unlike [`CellBox::of_resident`] this never spans the empty space *between*
/// bricks, so the meshing cost is exactly `resident_brick_count * 32768` cell
/// visits regardless of how far apart the bricks sit. Seams are still resolved
/// exactly: the face emitter samples one cell past each brick boundary directly,
/// which reaches into an adjacent resident brick or reports it as a missing
/// halo dependency.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResidentCells {
    boxes: Vec<CellBox>,
}

impl ResidentCells {
    /// Default ceiling on cell visits: `1 << 30` cells, i.e. 32768 resident
    /// bricks. Well past any G1 world; a larger resident set is an admission
    /// problem, not something to enumerate silently.
    pub const DEFAULT_CELL_VISIT_BUDGET: u128 = 1 << 30;

    /// Plan the meshing work set for every resident brick of `volume`.
    ///
    /// Fails with [`MeshError::WorkBudgetExceeded`] before enumerating anything
    /// if the resident set needs more than `cell_visit_budget` cell visits, and
    /// with [`MeshError::CoordinateOverflow`] if a resident brick's cell extent
    /// leaves the `i64` range.
    pub fn plan(volume: &Volume, cell_visit_budget: u128) -> Result<Self, MeshError> {
        let coords = volume.resident_brick_coords();

        let cell_visits = (coords.len() as u128)
            .checked_mul(CELLS_PER_BRICK as u128)
            .filter(|visits| *visits <= cell_visit_budget)
            .ok_or(MeshError::WorkBudgetExceeded {
                cell_visits: (coords.len() as u128).saturating_mul(CELLS_PER_BRICK as u128),
                budget: cell_visit_budget,
            })?;
        debug_assert!(cell_visits <= cell_visit_budget);

        let mut boxes = Vec::with_capacity(coords.len());
        for coord in coords {
            boxes.push(CellBox::of_brick(coord).ok_or(MeshError::CoordinateOverflow { coord })?);
        }
        Ok(Self { boxes })
    }

    /// `true` when no brick is resident.
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// Number of resident bricks in the set.
    pub fn brick_count(&self) -> usize {
        self.boxes.len()
    }

    /// Exact number of cell visits enumerating this set costs
    /// (`brick_count * 32768`).
    pub fn cell_visits(&self) -> u128 {
        self.boxes.iter().map(CellBox::cell_count).sum()
    }

    /// The per-brick cell boxes, in canonical brick order.
    pub fn boxes(&self) -> &[CellBox] {
        &self.boxes
    }

    /// Iterate every cell of every resident brick: canonical `(z, y, x)` brick
    /// order, then canonical cell order within each brick.
    pub fn cells(&self) -> impl Iterator<Item = [i64; 3]> + '_ {
        self.boxes.iter().flat_map(CellBox::cells)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, Revision, VolumeId};
    use spall_voxel::Brick;

    fn vid() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    #[test]
    fn out_of_bounds_reads_as_open_absent_reads_as_unknown() {
        let bounds =
            spall_voxel::BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(0, 0, 0))
                .unwrap();
        let v = Volume::bounded(vid(), CellSizeCode::Quarter, bounds);
        let s = VolumeSampler::new(&v);
        assert_eq!(s.at([100, 0, 0]), Occupancy::Open);
        assert_eq!(s.at([0, 0, 0]), Occupancy::Unknown(Residency::Absent));
    }

    #[test]
    fn resident_cells_classify_solid_and_open() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(1), Revision(1)),
        )
        .unwrap();
        v.insert_brick(
            BrickCoord::new(0, 1, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
        let s = VolumeSampler::new(&v);
        assert_eq!(s.at([5, 5, 5]), Occupancy::Solid(MaterialId(1)));
        assert_eq!(s.at([5, 40, 5]), Occupancy::Open);
    }

    #[test]
    fn resident_cell_box_covers_every_brick() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId(1), Revision(1)),
        )
        .unwrap();
        v.insert_brick(
            BrickCoord::new(-1, 2, 0),
            Brick::uniform(MaterialId(1), Revision(1)),
        )
        .unwrap();
        let b = CellBox::of_resident(&v).unwrap();
        assert_eq!(b.min, [-32, 0, 0]);
        assert_eq!(b.max, [31, 95, 31]);
        assert_eq!(b.cells().count(), 64 * 96 * 32);
    }

    fn solid_at(v: &mut Volume, coord: BrickCoord) {
        v.insert_brick(coord, Brick::uniform(MaterialId(1), Revision(1)))
            .unwrap();
    }

    #[test]
    fn resident_cells_cost_scales_with_bricks_not_separation() {
        let mut near = Volume::new(vid(), CellSizeCode::Quarter);
        solid_at(&mut near, BrickCoord::new(0, 0, 0));
        solid_at(&mut near, BrickCoord::new(1, 0, 0));

        let mut far = Volume::new(vid(), CellSizeCode::Quarter);
        solid_at(&mut far, BrickCoord::new(0, 0, 0));
        solid_at(&mut far, BrickCoord::new(1_000_000, 500_000, -750_000));

        let budget = ResidentCells::DEFAULT_CELL_VISIT_BUDGET;
        let near = ResidentCells::plan(&near, budget).unwrap();
        let far = ResidentCells::plan(&far, budget).unwrap();

        assert_eq!(near.cell_visits(), 2 * CELLS_PER_BRICK as u128);
        assert_eq!(far.cell_visits(), near.cell_visits());
        assert_eq!(far.cells().count() as u128, far.cell_visits());
        // Enumeration walks the per-brick boxes in canonical (z, y, x) brick
        // order; the first cell emitted is the first box's min corner.
        assert_eq!(far.boxes().len(), 2);
        assert_eq!(far.cells().next().unwrap(), far.boxes()[0].min);
        assert!(far.boxes()[0].min[2] <= far.boxes()[1].min[2]);
    }

    #[test]
    fn resident_cells_reject_an_oversized_set_before_enumerating() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        solid_at(&mut v, BrickCoord::new(0, 0, 0));
        solid_at(&mut v, BrickCoord::new(9, 9, 9));

        let err = ResidentCells::plan(&v, 1_000).unwrap_err();
        assert_eq!(
            err,
            MeshError::WorkBudgetExceeded {
                cell_visits: 2 * CELLS_PER_BRICK as u128,
                budget: 1_000,
            }
        );
    }

    #[test]
    fn resident_cells_reject_a_brick_that_overflows_i64() {
        let mut v = Volume::new(vid(), CellSizeCode::Quarter);
        solid_at(&mut v, BrickCoord::new(i64::MAX, 0, 0));

        let err = ResidentCells::plan(&v, ResidentCells::DEFAULT_CELL_VISIT_BUDGET).unwrap_err();
        assert_eq!(
            err,
            MeshError::CoordinateOverflow {
                coord: BrickCoord::new(i64::MAX, 0, 0),
            }
        );
        assert_eq!(CellBox::of_brick(BrickCoord::new(i64::MAX, 0, 0)), None);
        assert_eq!(CellBox::of_resident(&v), None);
    }

    #[test]
    fn of_brick_matches_a_hand_computed_box() {
        let b = CellBox::of_brick(BrickCoord::new(-1, 2, 0)).unwrap();
        assert_eq!(b.min, [-32, 64, 0]);
        assert_eq!(b.max, [-1, 95, 31]);
        assert_eq!(b.cell_count(), CELLS_PER_BRICK as u128);
    }
}
