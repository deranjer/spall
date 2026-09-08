//! Cell occupancy sampling for meshing, including the one-cell halo across brick
//! seams.

use spall_core::{BrickCoord, GlobalCell, MaterialId};
use spall_voxel::{AccessError, Residency, Sample, Volume};

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
    /// The tight cell box covering every resident brick of `volume`
    /// (`32^3` cells per brick), or `None` if no brick is resident.
    pub fn of_resident(volume: &Volume) -> Option<Self> {
        let coords = volume.resident_brick_coords();
        let first = coords.first()?;
        let mut min = brick_min_cell(*first);
        let mut max = [min[0] + 31, min[1] + 31, min[2] + 31];
        for c in coords.iter().skip(1) {
            let lo = brick_min_cell(*c);
            for a in 0..3 {
                min[a] = min[a].min(lo[a]);
                max[a] = max[a].max(lo[a] + 31);
            }
        }
        Some(Self { min, max })
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
fn brick_min_cell(coord: BrickCoord) -> [i64; 3] {
    [coord.x * 32, coord.y * 32, coord.z * 32]
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
}
