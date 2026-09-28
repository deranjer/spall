//! Dense solid-occupancy extraction from a [`spall_voxel::Volume`] region.
//!
//! A collision build never walks the sparse brick map directly; it works from
//! an [`OccupancyGrid`] — a contiguous solid mask plus the material id of each
//! solid cell over an axis-aligned cell box. Both collider representations
//! ([`crate::collider`]) and the analytic mass reference ([`crate::mass`]) read
//! this one structure, so they are always describing exactly the same set of
//! cells.

use spall_core::{BrickCoord, GlobalCell, LocalCell, MaterialId};
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
    /// The requested physics origin cannot be represented as a local cell frame.
    #[error("physics origin cannot be represented in this occupancy grid's cell frame")]
    OriginOutOfRange,
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
        Self::from_region_bricks(volume, min, max)
    }

    /// Brick-at-a-time extraction: a uniform brick is one comparison (air is
    /// skipped outright), a dense brick is read straight from its snapshot
    /// instead of one `Volume::sample` map lookup per cell. Bit-identical to
    /// [`Self::from_region_reference`]; whenever a brick in the region is not
    /// resident (or the region leaves the volume bounds) it defers to that
    /// reference path so the error names exactly the same first unresident cell.
    fn from_region_bricks(
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
        let (lo, hi) = (min.split().0, max.split().0);
        let mut snaps = Vec::new();
        for bz in lo.z..=hi.z {
            for by in lo.y..=hi.y {
                for bx in lo.x..=hi.x {
                    let coord = BrickCoord::new(bx, by, bz);
                    match volume.snapshot_brick(coord) {
                        Ok(Some(snap)) => snaps.push((coord, snap)),
                        _ => return Self::from_region_reference(volume, min, max),
                    }
                }
            }
        }

        let mut solid = vec![false; cells as usize];
        let mut material = vec![MaterialId::AIR; cells as usize];
        for (coord, snap) in &snaps {
            let base = [coord.x * 32, coord.y * 32, coord.z * 32];
            // The brick's overlap with the region, in global cells.
            let g0 = [base[0].max(min.x), base[1].max(min.y), base[2].max(min.z)];
            let g1 = [
                (base[0] + 31).min(max.x),
                (base[1] + 31).min(max.y),
                (base[2] + 31).min(max.z),
            ];
            let uniform = if snap.is_dense() {
                None
            } else {
                LocalCell::from_linear_index(0).map(|c| snap.get(c))
            };
            if uniform.is_some_and(|m| m.is_air()) {
                continue;
            }
            for gz in g0[2]..=g1[2] {
                for gy in g0[1]..=g1[1] {
                    let row = Self::linear(
                        dims,
                        (g0[0] - min.x) as u32,
                        (gy - min.y) as u32,
                        (gz - min.z) as u32,
                    );
                    for (i, gx) in (g0[0]..=g1[0]).enumerate() {
                        let m = match uniform {
                            Some(m) => m,
                            None => {
                                let local = LocalCell::new(
                                    (gx - base[0]) as u8,
                                    (gy - base[1]) as u8,
                                    (gz - base[2]) as u8,
                                )
                                .expect("overlap lies inside the brick");
                                snap.get(local)
                            }
                        };
                        if !m.is_air() {
                            solid[row + i] = true;
                            material[row + i] = m;
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

    fn from_region_reference(
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
        let mut extend = |cell: [i64; 3]| {
            for a in 0..3 {
                min[a] = min[a].min(cell[a]);
                max[a] = max[a].max(cell[a]);
            }
        };
        for c in &coords {
            let base = [c.x * 32, c.y * 32, c.z * 32];
            let Ok(Some(snap)) = volume.snapshot_brick(*c) else {
                continue;
            };
            if !snap.is_dense() {
                // Uniform brick: one comparison decides the whole brick.
                let solid = LocalCell::from_linear_index(0).is_some_and(|l| !snap.get(l).is_air());
                if solid {
                    extend(base);
                    extend([base[0] + 31, base[1] + 31, base[2] + 31]);
                }
                continue;
            }
            for lz in 0..32u8 {
                for ly in 0..32u8 {
                    for lx in 0..32u8 {
                        let local = LocalCell::new(lx, ly, lz).expect("in range");
                        if !snap.get(local).is_air() {
                            extend([
                                base[0] + i64::from(lx),
                                base[1] + i64::from(ly),
                                base[2] + i64::from(lz),
                            ]);
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

    /// Builds a grid directly from a caller-supplied solid mask and per-cell
    /// material, both in canonical `x + dims.x * (y + dims.y * z)` order. Grid
    /// cell `(0, 0, 0)` maps to `origin`. `solid` and `material` must each hold
    /// exactly `dims.x * dims.y * dims.z` entries, and that product must not
    /// exceed [`MAX_GRID_CELLS`].
    ///
    /// This is the constructor for a *derived* occupancy — e.g. a deterministic
    /// coarse-downsample of another grid — that is not a straight read of a
    /// [`Volume`] region.
    pub fn from_solid_mask(
        origin: GlobalCell,
        dims: [u32; 3],
        solid: Vec<bool>,
        material: Vec<MaterialId>,
    ) -> Result<Self, ExtractError> {
        if dims.contains(&0) {
            return Err(ExtractError::EmptyExtent([
                dims[0] as i64,
                dims[1] as i64,
                dims[2] as i64,
            ]));
        }
        let cells = dims[0] as u128 * dims[1] as u128 * dims[2] as u128;
        if cells > MAX_GRID_CELLS {
            return Err(ExtractError::TooLarge {
                cells,
                limit: MAX_GRID_CELLS,
            });
        }
        if solid.len() as u128 != cells || material.len() as u128 != cells {
            return Err(ExtractError::EmptyExtent([
                solid.len() as i64,
                material.len() as i64,
                cells as i64,
            ]));
        }
        Ok(Self {
            origin,
            dims,
            solid,
            material,
        })
    }

    #[inline]
    fn linear(dims: [u32; 3], x: u32, y: u32, z: u32) -> usize {
        (x + dims[0] * (y + dims[1] * z)) as usize
    }

    /// Global cell that maps to grid cell `(0, 0, 0)`.
    pub fn origin(&self) -> GlobalCell {
        self.origin
    }

    /// Returns the same occupancy with its grid origin shifted by the
    /// supplied cell count. Cell contents and ordering remain unchanged.
    pub fn rebase_origin_by_cells(mut self, subtract_cells: [i64; 3]) -> Option<Self> {
        self.origin = GlobalCell::new(
            self.origin.x.checked_sub(subtract_cells[0])?,
            self.origin.y.checked_sub(subtract_cells[1])?,
            self.origin.z.checked_sub(subtract_cells[2])?,
        );
        Some(self)
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

    /// The brick-at-a-time extraction is bit-identical to the original
    /// cell-by-cell reference on uniform, dense, and partially overlapped
    /// bricks (regions that start/stop mid-brick), and defers to it — with the
    /// same error — when a brick is not resident.
    #[test]
    fn brick_extraction_matches_the_cell_by_cell_reference() {
        let scenes = [
            fixtures::separated_regions_scene(vid(1)),
            fixtures::hollow_tower(vid(1)),
            fixtures::giant_collapse_scene(vid(1)),
        ];
        for v in &scenes {
            let full = OccupancyGrid::from_volume(v).unwrap().unwrap();
            let o = full.origin();
            let d = full.dims();
            let hi = GlobalCell::new(
                o.x + i64::from(d[0]) - 1,
                o.y + i64::from(d[1]) - 1,
                o.z + i64::from(d[2]) - 1,
            );
            let inner = [
                (o, hi),
                (
                    GlobalCell::new(o.x + 5, o.y + 3, o.z + 7),
                    GlobalCell::new(hi.x - 9, hi.y - 2, hi.z - 4),
                ),
            ];
            for (lo, hi) in inner {
                if lo.x > hi.x || lo.y > hi.y || lo.z > hi.z {
                    continue;
                }
                let fast = OccupancyGrid::from_region_bricks(v, lo, hi).unwrap();
                let slow = OccupancyGrid::from_region_reference(v, lo, hi).unwrap();
                assert_eq!(fast.origin, slow.origin);
                assert_eq!(fast.dims, slow.dims);
                assert_eq!(fast.solid, slow.solid);
                assert_eq!(fast.material, slow.material);
            }
        }
        // A region reaching an absent brick: same error, same cell.
        let v = fixtures::separated_regions_scene(vid(1));
        let lo = GlobalCell::new(0, 0, 0);
        let hi = GlobalCell::new(400, 5, 5);
        assert_eq!(
            OccupancyGrid::from_region_bricks(&v, lo, hi).unwrap_err(),
            OccupancyGrid::from_region_reference(&v, lo, hi).unwrap_err()
        );
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
    fn from_solid_mask_round_trips_and_checks_lengths() {
        let dims = [2u32, 2, 2];
        let mut solid = vec![false; 8];
        solid[0] = true;
        solid[7] = true;
        let material = vec![MaterialId(3); 8];
        let grid = OccupancyGrid::from_solid_mask(GlobalCell::new(-4, 0, 9), dims, solid, material)
            .unwrap();
        assert_eq!(grid.origin(), GlobalCell::new(-4, 0, 9));
        assert_eq!(grid.solid_count(), 2);
        assert!(grid.is_solid(0, 0, 0));
        assert!(grid.is_solid(1, 1, 1));
        assert_eq!(grid.material(0, 0, 0), Some(MaterialId(3)));

        // Wrong mask length is rejected, not silently truncated.
        assert!(
            OccupancyGrid::from_solid_mask(
                GlobalCell::new(0, 0, 0),
                [2, 2, 2],
                vec![false; 4],
                vec![MaterialId::AIR; 8],
            )
            .is_err()
        );
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
