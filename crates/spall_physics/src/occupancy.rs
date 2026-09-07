//! Dense solid-occupancy extraction from a [`spall_voxel::Volume`] region.
//!
//! A collision build never walks the sparse brick map directly; it works from
//! an [`OccupancyGrid`] — a contiguous solid mask plus the material id of each
//! solid cell over an axis-aligned cell box. Both collider representations
//! ([`crate::collider`]) and the analytic mass reference ([`crate::mass`]) read
//! this one structure, so they are always describing exactly the same set of
//! cells.

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{Sample, Volume};

/// Why occupancy extraction failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// The region has zero or negative extent on some axis.
    #[error("occupancy region has empty extent {0:?}")]
    EmptyExtent([i64; 3]),
    /// The region is too large to materialise as a dense grid on CI hardware.
    #[error("occupancy region {cells} cells exceeds the {limit} cell budget")]
    TooLarge { cells: u128, limit: u128 },
    /// A cell in the region is not backed by a resident brick. A feasibility
    /// build works on fully resident fixtures; streamed residency is T18.
    #[error("cell {0:?} is not resident")]
    Unresident([i64; 3]),
}

/// Largest dense occupancy grid a single build will allocate: 8 M cells (one
/// bool + one `MaterialId` each ≈ 24 MiB). The 64-brick feasibility body sits
/// just under this; anything larger must be split by the caller.
pub const MAX_GRID_CELLS: u128 = 8 * 1024 * 1024;

/// A dense solid-occupancy grid over `dims` cells. Grid cell `(0, 0, 0)` is the
/// global cell [`OccupancyGrid::origin`]; grid axes are parallel to the world
/// axes (a volume's own rigid transform is applied later, by the physics body).
#[derive(Debug, Clone)]
pub struct OccupancyGrid {
    origin: GlobalCell,
    dims: [u32; 3],
    solid: Vec<bool>,
    material: Vec<MaterialId>,
}

impl OccupancyGrid {
    /// Extracts the solid occupancy of an inclusive global-cell box
    /// `[min, max]`. Every cell in the box must be resident.
    pub fn from_region(
        volume: &Volume,
        min: GlobalCell,
        max: GlobalCell,
    ) -> Result<Self, ExtractError> {
        let extent = [max.x - min.x + 1, max.y - min.y + 1, max.z - min.z + 1];
        if extent.iter().any(|&e| e <= 0) {
            return Err(ExtractError::EmptyExtent(extent));
        }
        let dims = [extent[0] as u32, extent[1] as u32, extent[2] as u32];
        let cells = dims[0] as u128 * dims[1] as u128 * dims[2] as u128;
        if cells > MAX_GRID_CELLS {
            return Err(ExtractError::TooLarge {
                cells,
                limit: MAX_GRID_CELLS,
            });
        }

        let mut solid = vec![false; cells as usize];
        let mut material = vec![MaterialId::AIR; cells as usize];
        for gz in 0..dims[2] {
            for gy in 0..dims[1] {
                for gx in 0..dims[0] {
                    let cell =
                        GlobalCell::new(min.x + gx as i64, min.y + gy as i64, min.z + gz as i64);
                    let sample = volume
                        .sample(cell)
                        .map_err(|_| ExtractError::Unresident([cell.x, cell.y, cell.z]))?;
                    match sample {
                        Sample::Filled(m) => {
                            let idx = Self::linear(dims, gx, gy, gz);
                            solid[idx] = true;
                            material[idx] = m;
                        }
                        Sample::Empty { .. } => {}
                        Sample::Unknown(_) => {
                            return Err(ExtractError::Unresident([cell.x, cell.y, cell.z]));
                        }
                    }
                }
            }
        }
        Ok(Self {
            origin: min,
            dims,
            solid,
            material,
        })
    }

    /// Extracts the occupancy of the tight axis-aligned box that encloses every
    /// *solid* cell of `volume` (not merely its resident bricks — a body's local
    /// grid must not be padded with air, which would displace its origin and
    /// inflate the voxel shape). Returns `None` if the volume has no solid cell.
    pub fn from_volume(volume: &Volume) -> Result<Option<Self>, ExtractError> {
        let coords = volume.resident_brick_coords();
        let mut min = [i64::MAX; 3];
        let mut max = [i64::MIN; 3];
        for c in &coords {
            let base = [c.x * 32, c.y * 32, c.z * 32];
            for lz in 0..32 {
                for ly in 0..32 {
                    for lx in 0..32 {
                        let cell = GlobalCell::new(base[0] + lx, base[1] + ly, base[2] + lz);
                        if let Ok(Sample::Filled(_)) = volume.sample(cell) {
                            min[0] = min[0].min(cell.x);
                            min[1] = min[1].min(cell.y);
                            min[2] = min[2].min(cell.z);
                            max[0] = max[0].max(cell.x);
                            max[1] = max[1].max(cell.y);
                            max[2] = max[2].max(cell.z);
                        }
                    }
                }
            }
        }
        if min[0] > max[0] {
            return Ok(None);
        }
        Self::from_region(
            volume,
            GlobalCell::new(min[0], min[1], min[2]),
            GlobalCell::new(max[0], max[1], max[2]),
        )
        .map(Some)
    }

    #[inline]
    fn linear(dims: [u32; 3], x: u32, y: u32, z: u32) -> usize {
        (x + dims[0] * (y + dims[1] * z)) as usize
    }

    /// Global cell that maps to grid cell `(0, 0, 0)`.
    pub fn origin(&self) -> GlobalCell {
        self.origin
    }

    /// Grid extent in cells on each axis.
    pub fn dims(&self) -> [u32; 3] {
        self.dims
    }

    /// Number of solid cells.
    pub fn solid_count(&self) -> u64 {
        self.solid.iter().filter(|&&s| s).count() as u64
    }

    /// Whether grid cell `(x, y, z)` is solid. Out-of-range is `false`.
    #[inline]
    pub fn is_solid(&self, x: u32, y: u32, z: u32) -> bool {
        if x >= self.dims[0] || y >= self.dims[1] || z >= self.dims[2] {
            return false;
        }
        self.solid[Self::linear(self.dims, x, y, z)]
    }

    /// Material of grid cell `(x, y, z)`, or `None` if the cell is not solid.
    #[inline]
    pub fn material(&self, x: u32, y: u32, z: u32) -> Option<MaterialId> {
        self.is_solid(x, y, z)
            .then(|| self.material[Self::linear(self.dims, x, y, z)])
    }

    /// Solid cell indices in canonical `(z, y, x)` order, as `[x, y, z]`.
    pub fn solid_indices(&self) -> Vec<[i32; 3]> {
        let mut out = Vec::with_capacity(self.solid_count() as usize);
        for z in 0..self.dims[2] {
            for y in 0..self.dims[1] {
                for x in 0..self.dims[0] {
                    if self.solid[Self::linear(self.dims, x, y, z)] {
                        out.push([x as i32, y as i32, z as i32]);
                    }
                }
            }
        }
        out
    }

    /// Visits every solid cell as `(x, y, z, material)` in canonical order.
    pub fn for_each_solid(&self, mut f: impl FnMut(u32, u32, u32, MaterialId)) {
        for z in 0..self.dims[2] {
            for y in 0..self.dims[1] {
                for x in 0..self.dims[0] {
                    let idx = Self::linear(self.dims, x, y, z);
                    if self.solid[idx] {
                        f(x, y, z, self.material[idx]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_voxel::fixtures;

    fn vid(n: u64) -> spall_core::VolumeId {
        spall_core::VolumeId::new(n).unwrap()
    }

    #[test]
    fn hollow_tower_occupancy_is_a_shell() {
        let v = fixtures::hollow_tower(vid(1));
        let grid = OccupancyGrid::from_volume(&v).unwrap().unwrap();
        // Outer box x,z in -6..=5, y in -8..=23 -> 12 x 32 x 12 cells.
        assert_eq!(grid.dims(), [12, 32, 12]);
        // The interior void is not solid; the shell is.
        let interior = grid.origin();
        // grid cell for global (0,0,0): x = 0 - (-6) = 6, y = 0 - (-8) = 8.
        assert!(!grid.is_solid(6, 8, 6));
        assert!(grid.is_solid(0, 0, 0));
        assert_eq!(interior, GlobalCell::new(-6, -8, -6));
        // The 2-cell shell leaves a real interior void: an 8 x 28 x 8 block of
        // air inside the 12 x 32 x 12 outer box.
        let total = 12 * 32 * 12;
        let void = 8 * 28 * 8;
        assert_eq!(grid.solid_count(), total - void);
        // Every interior cell samples as open.
        for dy in 10..=20 {
            assert!(!grid.is_solid(6, dy, 6), "interior cell {dy} is solid");
        }
    }

    #[test]
    fn unresident_region_is_rejected() {
        let mut v = Volume::new(vid(2), spall_core::CellSizeCode::Quarter);
        v.apply_edit(&spall_voxel::EditPlan::filled_box(
            vid(2),
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(1, 1, 1),
            fixtures::STONE,
        ))
        .unwrap();
        // Region reaches into an absent brick.
        let err =
            OccupancyGrid::from_region(&v, GlobalCell::new(0, 0, 0), GlobalCell::new(40, 1, 1))
                .unwrap_err();
        assert!(matches!(err, ExtractError::Unresident(_)));
    }

    #[test]
    fn oversized_region_is_rejected_before_allocation() {
        let v = fixtures::flat_terrain(vid(3));
        let err = OccupancyGrid::from_region(
            &v,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(4095, 511, 4095),
        )
        .unwrap_err();
        assert!(matches!(err, ExtractError::TooLarge { .. }));
    }
}
