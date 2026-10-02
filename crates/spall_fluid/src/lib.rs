//! CPU fluid state and voxel-boundary snapshots for Spall.
//!
//! Water is stored independently from voxel materials. This crate provides
//! bounded domain construction, strict boundary capture, water accounting,
//! and the two-phase MAC/VOF grid solver selected by ENG-103 (`grid_mac`).
//! Rapier coupling, persistence, and network replication are not yet
//! integrated (ENG-105).

pub mod cut_cell;
pub mod fixtures;
pub mod grid_mac;

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{AccessError, Residency, Sample, Volume};

/// Maximum allocated cells for one fluid domain unless the caller chooses a
/// lower bound. Kept deliberately small until solver costs are measured.
pub const DEFAULT_MAX_DOMAIN_CELLS: usize = 2_000_000;

/// A rectangular, cell-aligned fluid simulation domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainSpec {
    origin: GlobalCell,
    dimensions: [u32; 3],
    cell_count: usize,
}

impl DomainSpec {
    /// Creates a domain whose origin is its minimum global solid-cell corner.
    pub fn new(
        origin: GlobalCell,
        dimensions: [u32; 3],
        max_cells: usize,
    ) -> Result<Self, DomainError> {
        if dimensions.contains(&0) {
            return Err(DomainError::EmptyAxis);
        }
        let cell_count = dimensions
            .into_iter()
            .try_fold(1usize, |count, axis| count.checked_mul(axis as usize))
            .ok_or(DomainError::CellCountOverflow)?;
        if cell_count > max_cells {
            return Err(DomainError::CellLimitExceeded {
                requested: cell_count,
                limit: max_cells,
            });
        }
        for (start, length) in [
            (origin.x, dimensions[0]),
            (origin.y, dimensions[1]),
            (origin.z, dimensions[2]),
        ] {
            start
                .checked_add(i64::from(length) - 1)
                .ok_or(DomainError::CoordinateOverflow)?;
        }
        Ok(Self {
            origin,
            dimensions,
            cell_count,
        })
    }

    pub const fn origin(self) -> GlobalCell {
        self.origin
    }

    pub const fn dimensions(self) -> [u32; 3] {
        self.dimensions
    }

    pub const fn cell_count(self) -> usize {
        self.cell_count
    }

    pub(crate) fn index_of(self, cell: GlobalCell) -> Option<usize> {
        let x = cell.x.checked_sub(self.origin.x)?;
        let y = cell.y.checked_sub(self.origin.y)?;
        let z = cell.z.checked_sub(self.origin.z)?;
        if x < 0
            || y < 0
            || z < 0
            || x >= i64::from(self.dimensions[0])
            || y >= i64::from(self.dimensions[1])
            || z >= i64::from(self.dimensions[2])
        {
            return None;
        }
        Some(
            x as usize
                + self.dimensions[0] as usize
                    * (y as usize + self.dimensions[1] as usize * z as usize),
        )
    }

    pub(crate) fn cell_at(self, index: usize) -> GlobalCell {
        let nx = self.dimensions[0] as usize;
        let ny = self.dimensions[1] as usize;
        let x = index % nx;
        let yz = index / nx;
        let y = yz % ny;
        let z = yz / ny;
        GlobalCell::new(
            self.origin.x + x as i64,
            self.origin.y + y as i64,
            self.origin.z + z as i64,
        )
    }
}

/// Errors building the first bounded CPU fluid state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("fluid domains must have nonzero dimensions on every axis")]
    EmptyAxis,
    #[error("fluid domain cell count overflowed usize")]
    CellCountOverflow,
    #[error("fluid domain requests {requested} cells, above the configured limit {limit}")]
    CellLimitExceeded { requested: usize, limit: usize },
    #[error("fluid domain coordinates overflow global cell space")]
    CoordinateOverflow,
    #[error("water amount must be finite and within 0..=1")]
    InvalidWaterAmount,
    #[error("cannot add water to a solid voxel at {0:?}")]
    WaterInSolid(GlobalCell),
    #[error("cell {0:?} is outside the fluid domain")]
    OutsideDomain(GlobalCell),
    #[error("fluid domain dimensions must be multiples of the coarsening factor {factor}")]
    UnalignedCoarsening { factor: u32 },
}

/// A complete, immutable solid mask captured from resident Spall voxel data.
/// Unknown voxel residency is rejected; it is never interpreted as air or a
/// solid boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolidBoundary {
    spec: DomainSpec,
    solid: Vec<bool>,
}

impl SolidBoundary {
    /// Reads the solid mask brick by brick: one snapshot per brick overlapping
    /// the domain instead of one hash lookup per cell (about 30 ns per cell,
    /// which made a 3 M-voxel capture ~90 ms on the simulation thread). A brick
    /// that is not resident is reported exactly as a per-cell sample would.
    pub fn capture(volume: &Volume, spec: DomainSpec) -> Result<Self, BoundaryError> {
        let [nx, ny, nz] = spec.dimensions.map(|d| d as usize);
        let origin = spec.origin;
        let lo = [origin.x, origin.y, origin.z];
        let hi = [
            origin.x + nx as i64 - 1,
            origin.y + ny as i64 - 1,
            origin.z + nz as i64 - 1,
        ];
        let edge = i64::from(spall_core::BRICK_EDGE);
        let mut solid = vec![false; spec.cell_count];
        for bz in lo[2].div_euclid(edge)..=hi[2].div_euclid(edge) {
            for by in lo[1].div_euclid(edge)..=hi[1].div_euclid(edge) {
                for bx in lo[0].div_euclid(edge)..=hi[0].div_euclid(edge) {
                    let coord = spall_core::BrickCoord::new(bx, by, bz);
                    let from = [
                        (bx * edge).max(lo[0]),
                        (by * edge).max(lo[1]),
                        (bz * edge).max(lo[2]),
                    ];
                    let to = [
                        (bx * edge + edge - 1).min(hi[0]),
                        (by * edge + edge - 1).min(hi[1]),
                        (bz * edge + edge - 1).min(hi[2]),
                    ];
                    let snapshot = match volume.snapshot_brick(coord) {
                        Ok(Some(snapshot)) => snapshot,
                        // Not resident (or outside bounds): sample the cells so
                        // the error is the one the per-cell path reports.
                        _ => {
                            Self::capture_cells_slowly(volume, spec, from, to, &mut solid)?;
                            continue;
                        }
                    };
                    let uniform = (!snapshot.is_dense()).then(|| {
                        let zero = spall_core::LocalCell::new(0, 0, 0).expect("in-brick");
                        snapshot.get(zero) != MaterialId::AIR
                    });
                    for z in from[2]..=to[2] {
                        for y in from[1]..=to[1] {
                            let row = (from[0] - origin.x) as usize
                                + nx * ((y - origin.y) as usize + ny * (z - origin.z) as usize);
                            let span = (to[0] - from[0] + 1) as usize;
                            if let Some(is_solid) = uniform {
                                solid[row..row + span].fill(is_solid);
                                continue;
                            }
                            let (ly, lz) = ((y - by * edge) as u8, (z - bz * edge) as u8);
                            for (i, x) in (from[0]..=to[0]).enumerate() {
                                let local =
                                    spall_core::LocalCell::new((x - bx * edge) as u8, ly, lz)
                                        .expect("in-brick");
                                solid[row + i] = snapshot.get(local) != MaterialId::AIR;
                            }
                        }
                    }
                }
            }
        }
        Ok(Self { spec, solid })
    }

    fn capture_cells_slowly(
        volume: &Volume,
        spec: DomainSpec,
        from: [i64; 3],
        to: [i64; 3],
        solid: &mut [bool],
    ) -> Result<(), BoundaryError> {
        for z in from[2]..=to[2] {
            for y in from[1]..=to[1] {
                for x in from[0]..=to[0] {
                    let cell = GlobalCell::new(x, y, z);
                    let index = spec.index_of(cell).expect("cell inside the domain");
                    solid[index] = match volume.sample(cell).map_err(BoundaryError::Access)? {
                        Sample::Filled(material) => material != MaterialId::AIR,
                        Sample::Empty { .. } => false,
                        Sample::Unknown(residency) => {
                            return Err(BoundaryError::UnknownCell { cell, residency });
                        }
                    };
                }
            }
        }
        Ok(())
    }

    pub const fn spec(&self) -> DomainSpec {
        self.spec
    }

    /// Refine an immutable voxel boundary by an integer factor while
    /// preserving its physical extent and exact aligned solid occupancy.
    pub fn refined(&self, factor: u32, max_cells: usize) -> Result<Self, DomainError> {
        if factor == 0 {
            return Err(DomainError::EmptyAxis);
        }
        if factor == 1 {
            return Ok(self.clone());
        }
        let dimensions = self
            .spec
            .dimensions()
            .map(|axis| {
                axis.checked_mul(factor)
                    .ok_or(DomainError::CellCountOverflow)
            })
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| DomainError::CellCountOverflow)?;
        let spec = DomainSpec::new(self.spec.origin(), dimensions, max_cells)?;
        let mut solid = Vec::with_capacity(spec.cell_count());
        for index in 0..spec.cell_count() {
            let child = spec.cell_at(index);
            let parent = GlobalCell::new(
                self.spec.origin().x + (child.x - self.spec.origin().x) / i64::from(factor),
                self.spec.origin().y + (child.y - self.spec.origin().y) / i64::from(factor),
                self.spec.origin().z + (child.z - self.spec.origin().z) / i64::from(factor),
            );
            let parent_index = self
                .spec
                .index_of(parent)
                .ok_or(DomainError::CoordinateOverflow)?;
            solid.push(self.solid[parent_index]);
        }
        Ok(Self { spec, solid })
    }

    /// Coarsen a voxel boundary by an integer factor, the inverse addressing of
    /// [`Self::refined`]: coarse cell `origin + k` covers fine cells
    /// `origin + k * factor ..= origin + k * factor + factor - 1` on each axis.
    ///
    /// A coarse cell is solid when at least half of its fine cells are solid.
    /// A one-voxel wall inside a `2³` block fills exactly half of it, so thin
    /// walls stay watertight at factor 2 while sloped banks are not thickened
    /// by a whole coarse cell. Every axis must be a multiple of `factor`.
    pub fn coarsened(&self, factor: u32) -> Result<Self, DomainError> {
        if factor == 0 {
            return Err(DomainError::EmptyAxis);
        }
        if factor == 1 {
            return Ok(self.clone());
        }
        let fine = self.spec.dimensions();
        if fine.iter().any(|axis| axis % factor != 0) {
            return Err(DomainError::UnalignedCoarsening { factor });
        }
        let dimensions = fine.map(|axis| axis / factor);
        let spec = DomainSpec::new(self.spec.origin(), dimensions, self.spec.cell_count())?;
        let f = factor as usize;
        let [nx, ny, _] = fine.map(|axis| axis as usize);
        let threshold = (f * f * f).div_ceil(2);
        let mut solid = Vec::with_capacity(spec.cell_count());
        for index in 0..spec.cell_count() {
            let coarse = spec.cell_at(index);
            let base = [
                (coarse.x - spec.origin().x) as usize * f,
                (coarse.y - spec.origin().y) as usize * f,
                (coarse.z - spec.origin().z) as usize * f,
            ];
            let mut count = 0;
            for z in base[2]..base[2] + f {
                for y in base[1]..base[1] + f {
                    let row = nx * (y + ny * z);
                    count += self.solid[row + base[0]..row + base[0] + f]
                        .iter()
                        .filter(|solid| **solid)
                        .count();
                }
            }
            solid.push(count >= threshold);
        }
        Ok(Self { spec, solid })
    }

    pub fn is_solid(&self, cell: GlobalCell) -> Option<bool> {
        self.spec.index_of(cell).map(|index| self.solid[index])
    }

    pub fn solid_cell_count(&self) -> usize {
        self.solid.iter().filter(|solid| **solid).count()
    }
}

/// Failure to build a boundary from voxel data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryError {
    #[error("voxel access failed: {0}")]
    Access(AccessError),
    #[error(
        "voxel cell {cell:?} is not resident ({residency:?}); fluid boundary capture requires complete geometry"
    )]
    UnknownCell {
        cell: GlobalCell,
        residency: Residency,
    },
}

/// Bounded water amounts stored separately from solid occupancy.
///
/// Amounts are fractions of one fluid cell's volume. Transport deliberately
/// is not provided yet; edits to solid geometry require recapturing the
/// boundary before later stepping can be added safely.
#[derive(Debug, Clone)]
pub struct WaterState {
    boundary: SolidBoundary,
    amounts: Vec<f64>,
}

impl WaterState {
    pub fn new(boundary: SolidBoundary) -> Self {
        let amounts = vec![0.0; boundary.spec.cell_count];
        Self { boundary, amounts }
    }

    pub fn add_water(&mut self, cell: GlobalCell, amount: f64) -> Result<(), DomainError> {
        if !amount.is_finite() || !(0.0..=1.0).contains(&amount) {
            return Err(DomainError::InvalidWaterAmount);
        }
        let index = self
            .boundary
            .spec
            .index_of(cell)
            .ok_or(DomainError::OutsideDomain(cell))?;
        if self.boundary.solid[index] && amount > 0.0 {
            return Err(DomainError::WaterInSolid(cell));
        }
        let updated = self.amounts[index] + amount;
        if updated > 1.0 {
            return Err(DomainError::InvalidWaterAmount);
        }
        self.amounts[index] = updated;
        Ok(())
    }

    pub fn amount_at(&self, cell: GlobalCell) -> Option<f64> {
        self.boundary
            .spec
            .index_of(cell)
            .map(|index| self.amounts[index])
    }

    /// Total water volume measured in fluid-cell volumes.
    pub fn total_water(&self) -> f64 {
        self.amounts.iter().sum()
    }

    pub fn occupied_water_cells(&self) -> usize {
        self.amounts.iter().filter(|amount| **amount > 0.0).count()
    }

    /// Approximate retained bytes for the domain arrays, excluding allocator
    /// metadata and the caller-owned source voxel volume.
    pub fn allocated_bytes(&self) -> usize {
        self.amounts.capacity() * size_of::<f64>()
            + self.boundary.solid.capacity() * size_of::<bool>()
    }

    pub fn boundary(&self) -> &SolidBoundary {
        &self.boundary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, CELLS_PER_BRICK, CellSizeCode, LocalCell, Revision, VolumeId};
    use spall_voxel::Brick;

    /// The brick-wise capture equals a per-cell reference over a domain that
    /// straddles uniform solid, uniform air, and dense bricks, and reports a
    /// non-resident brick as unknown.
    #[test]
    fn brick_wise_capture_matches_the_per_cell_reference() {
        let id = VolumeId::new(1).unwrap();
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        let stone = MaterialId(1);
        // (0,0,0) uniform stone, (1,0,0) uniform air, (0,1,0) dense mixed.
        volume
            .insert_brick(BrickCoord::new(0, 0, 0), Brick::uniform(stone, Revision(1)))
            .unwrap();
        volume
            .insert_brick(
                BrickCoord::new(1, 0, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        volume
            .insert_brick(
                BrickCoord::new(0, 1, 0),
                Brick::uniform(MaterialId::AIR, Revision(1)),
            )
            .unwrap();
        let mut plan = spall_voxel::EditPlan::new(id);
        for z in 0..32 {
            for x in 0..32 {
                if (x * 7 + z * 3) % 5 < 2 {
                    plan.set(GlobalCell::new(x, 32 + (x + z) % 9, z), stone);
                }
            }
        }
        volume.apply_edit(&plan).unwrap();
        let spec = DomainSpec::new(GlobalCell::new(-0, 10, 3), [60, 40, 20], 1 << 20).unwrap();
        // Brick (1,1,0) is absent: capture must fail like the per-cell path.
        assert!(matches!(
            SolidBoundary::capture(&volume, spec),
            Err(BoundaryError::UnknownCell { .. })
        ));
        volume
            .insert_brick(BrickCoord::new(1, 1, 0), Brick::uniform(stone, Revision(1)))
            .unwrap();
        // Bricks in z=-1 and z=1 do not overlap this domain (z 3..=22).
        let fast = SolidBoundary::capture(&volume, spec).unwrap();
        let mut reference = Vec::with_capacity(spec.cell_count());
        for index in 0..spec.cell_count() {
            reference.push(match volume.sample(spec.cell_at(index)).unwrap() {
                Sample::Filled(material) => material != MaterialId::AIR,
                Sample::Empty { .. } => false,
                Sample::Unknown(_) => unreachable!("all overlapping bricks are resident"),
            });
        }
        assert_eq!(fast.solid, reference);
        assert!(reference.iter().any(|s| *s) && reference.iter().any(|s| !*s));
    }

    fn air_volume_for(spec: DomainSpec) -> Volume {
        let mut volume = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let min = spec.origin;
        let max = GlobalCell::new(
            min.x + i64::from(spec.dimensions[0]) - 1,
            min.y + i64::from(spec.dimensions[1]) - 1,
            min.z + i64::from(spec.dimensions[2]) - 1,
        );
        let min_brick = min.split().0;
        let max_brick = max.split().0;
        for z in min_brick.z..=max_brick.z {
            for y in min_brick.y..=max_brick.y {
                for x in min_brick.x..=max_brick.x {
                    volume
                        .insert_brick(
                            BrickCoord::new(x, y, z),
                            Brick::uniform(MaterialId::AIR, Revision(1)),
                        )
                        .unwrap();
                }
            }
        }
        volume
    }

    fn set_solid(volume: &mut Volume, cell: GlobalCell, material: MaterialId) {
        let (brick_coord, local) = cell.split();
        let snapshot = volume.snapshot_brick(brick_coord).unwrap().unwrap();
        let mut cells = vec![MaterialId::AIR; CELLS_PER_BRICK];
        for (index, value) in cells.iter_mut().enumerate() {
            *value = snapshot.get(LocalCell::from_linear_index(index as u16).unwrap());
        }
        let mut brick = Brick::restored(&cells, snapshot.revision(), snapshot.is_edited());
        brick.set_cell(local, material);
        volume.insert_brick(brick_coord, brick).unwrap();
    }

    #[test]
    fn boundary_snapshot_reads_real_voxels_across_brick_seam() {
        let spec = DomainSpec::new(GlobalCell::new(30, 0, 0), [4, 1, 1], 16).unwrap();
        let mut volume = air_volume_for(spec);
        set_solid(&mut volume, GlobalCell::new(32, 0, 0), MaterialId(1));

        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        assert_eq!(boundary.solid_cell_count(), 1);
        assert_eq!(boundary.is_solid(GlobalCell::new(32, 0, 0)), Some(true));
        assert_eq!(boundary.is_solid(GlobalCell::new(31, 0, 0)), Some(false));
    }

    #[test]
    fn missing_residency_fails_closed_instead_of_becoming_air() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [1, 1, 1], 8).unwrap();
        let volume = Volume::new(VolumeId::new(2).unwrap(), CellSizeCode::Quarter);
        assert_eq!(
            SolidBoundary::capture(&volume, spec).unwrap_err(),
            BoundaryError::UnknownCell {
                cell: GlobalCell::new(0, 0, 0),
                residency: Residency::Absent,
            }
        );
    }

    #[test]
    fn water_accounting_is_independent_and_refuses_solids_or_overfill() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [2, 1, 1], 8).unwrap();
        let mut volume = air_volume_for(spec);
        set_solid(&mut volume, GlobalCell::new(1, 0, 0), MaterialId(2));
        let boundary = SolidBoundary::capture(&volume, spec).unwrap();
        let mut water = WaterState::new(boundary);
        water.add_water(GlobalCell::new(0, 0, 0), 0.25).unwrap();
        water.add_water(GlobalCell::new(0, 0, 0), 0.5).unwrap();

        assert_eq!(water.total_water(), 0.75);
        assert_eq!(water.occupied_water_cells(), 1);
        assert_eq!(
            water.allocated_bytes(),
            2 * (size_of::<f64>() + size_of::<bool>())
        );
        assert_eq!(
            water.add_water(GlobalCell::new(1, 0, 0), 0.1),
            Err(DomainError::WaterInSolid(GlobalCell::new(1, 0, 0)))
        );
        assert_eq!(
            water.add_water(GlobalCell::new(0, 0, 0), 0.3),
            Err(DomainError::InvalidWaterAmount)
        );
    }

    #[test]
    fn coarsening_keeps_one_voxel_walls_and_drops_isolated_voxels() {
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), [4, 2, 2], 16).unwrap();
        let mut volume = air_volume_for(spec);
        // A one-voxel wall at x = 1 fills half of the first 2³ block.
        for y in 0..2 {
            for z in 0..2 {
                set_solid(&mut volume, GlobalCell::new(1, y, z), MaterialId(1));
            }
        }
        // One stray voxel in the second block is not a barrier.
        set_solid(&mut volume, GlobalCell::new(3, 1, 1), MaterialId(1));
        let fine = SolidBoundary::capture(&volume, spec).unwrap();
        let coarse = fine.coarsened(2).unwrap();
        assert_eq!(coarse.spec().dimensions(), [2, 1, 1]);
        assert_eq!(coarse.spec().origin(), spec.origin());
        assert_eq!(coarse.is_solid(GlobalCell::new(0, 0, 0)), Some(true));
        assert_eq!(coarse.is_solid(GlobalCell::new(1, 0, 0)), Some(false));
        assert_eq!(
            fine.coarsened(3).unwrap_err(),
            DomainError::UnalignedCoarsening { factor: 3 }
        );
    }

    #[test]
    fn domain_limits_are_checked_before_allocation() {
        assert_eq!(
            DomainSpec::new(GlobalCell::new(0, 0, 0), [10, 10, 10], 999),
            Err(DomainError::CellLimitExceeded {
                requested: 1000,
                limit: 999
            })
        );
        assert_eq!(
            DomainSpec::new(GlobalCell::new(i64::MAX, 0, 0), [2, 1, 1], 8),
            Err(DomainError::CoordinateOverflow)
        );
        assert_eq!(
            DomainSpec::new(GlobalCell::new(0, 0, 0), [0, 1, 1], 8),
            Err(DomainError::EmptyAxis)
        );
    }
}
