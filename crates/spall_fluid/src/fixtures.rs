//! Bounded finite-reservoir scenes built from authoritative Spall voxels.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_voxel::{Brick, EditError, EditPlan, Volume};

use crate::{BoundaryError, DomainError, DomainSpec, SolidBoundary};

pub const RESERVOIR_DIMENSIONS: [u32; 3] = [24, 12, 8];
pub const RESERVOIR_CELL_SIZE_M: f32 = 0.25;
const SOLID_MATERIAL: MaterialId = MaterialId(1);

/// Reference water volume per 1/8 sub-cell. ENG-103's original particle
/// fixtures seeded eight lattice points per open cell, each carrying this f32
/// volume summed in f64; keeping the identical arithmetic keeps every grid
/// scenario's initial volume bit-for-bit comparable with recorded evidence.
const REFERENCE_SUBCELL_VOLUME_M3: f32 = 0.0015625;
const SUBCELLS_PER_CELL: usize = 8;

/// Finite two-reservoir voxel scene: floor, outer side/end walls, a dam at
/// x = 12 cells (times scale), and optional stacked tunnel/pool walls. Also
/// yields the reference water volume of the scenario's declared water boxes.
pub struct ReservoirScene {
    volume: Volume,
    spec: DomainSpec,
    reference_volume_m3: f64,
}

impl ReservoirScene {
    pub fn new_scaled(scale: u32) -> Result<Self, FixtureError> {
        Self::new_configured(scale, false, false)
    }

    /// Flat, pre-filled basin used to measure stability independently of
    /// block collapse.
    pub fn new_stability_basin(scale: u32) -> Result<Self, FixtureError> {
        Self::new_configured(scale, true, false)
    }

    pub fn new_tunnel_under_separate_pool(scale: u32) -> Result<Self, FixtureError> {
        Self::new_configured(scale, false, true)
    }

    fn new_configured(
        scale: u32,
        stability_basin: bool,
        tunnel_pool: bool,
    ) -> Result<Self, FixtureError> {
        if scale == 0 {
            return Err(FixtureError::InvalidParameters);
        }
        let dimensions: [u32; 3] = RESERVOIR_DIMENSIONS
            .map(|v| v.checked_mul(scale).ok_or(FixtureError::InvalidParameters))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| FixtureError::InvalidParameters)?;
        let spec = DomainSpec::new(
            GlobalCell::new(0, 0, 0),
            dimensions,
            dimensions.iter().map(|v| *v as usize).product(),
        )?;
        let volume_id = VolumeId::new(0x103).expect("nonzero fixture volume id");
        let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
        let brick_dims = dimensions.map(|v| v.div_ceil(32));
        for z in 0..brick_dims[2] as i64 {
            for y in 0..brick_dims[1] as i64 {
                for x in 0..brick_dims[0] as i64 {
                    volume.insert_brick(
                        BrickCoord::new(x, y, z),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )?;
                }
            }
        }
        let mut solids = EditPlan::new(volume_id);
        for z in 0..dimensions[2] as i64 {
            for x in 0..dimensions[0] as i64 {
                solids.set(GlobalCell::new(x, 0, z), SOLID_MATERIAL);
            }
            for y in 1..dimensions[1] as i64 {
                solids.set(GlobalCell::new(0, y, z), SOLID_MATERIAL);
                solids.set(
                    GlobalCell::new(dimensions[0] as i64 - 1, y, z),
                    SOLID_MATERIAL,
                );
            }
        }
        for x in 0..dimensions[0] as i64 {
            for y in 1..dimensions[1] as i64 {
                solids.set(GlobalCell::new(x, y, 0), SOLID_MATERIAL);
                solids.set(
                    GlobalCell::new(x, y, dimensions[2] as i64 - 1),
                    SOLID_MATERIAL,
                );
            }
        }
        let dam_x = 12 * scale as i64;
        for y in 1..(10 * scale) as i64 {
            for z in 1..dimensions[2] as i64 - 1 {
                solids.set(GlobalCell::new(dam_x, y, z), SOLID_MATERIAL);
            }
        }
        if tunnel_pool {
            for z in scale as i64..dimensions[2] as i64 - scale as i64 {
                for x in 14 * scale as i64..21 * scale as i64 {
                    solids.set(GlobalCell::new(x, 4 * scale as i64, z), SOLID_MATERIAL);
                }
            }
            // Start the side walls immediately above the tunnel roof. Scaling
            // the roof layer by itself leaves an air-course gap at scale > 1.
            for y in 4 * scale as i64 + 1..10 * scale as i64 {
                for z in scale as i64..7 * scale as i64 {
                    solids.set(GlobalCell::new(14 * scale as i64, y, z), SOLID_MATERIAL);
                    solids.set(GlobalCell::new(20 * scale as i64, y, z), SOLID_MATERIAL);
                }
                for x in 14 * scale as i64..21 * scale as i64 {
                    solids.set(GlobalCell::new(x, y, scale as i64), SOLID_MATERIAL);
                    solids.set(GlobalCell::new(x, y, 7 * scale as i64), SOLID_MATERIAL);
                }
            }
        }
        volume.apply_edit(&solids)?;
        let boundary = SolidBoundary::capture(&volume, spec)?;

        let s = scale as i64;
        let (z_min, z_max) = (2 * s, 6 * s);
        let mut water_boxes = Vec::new();
        if stability_basin {
            water_boxes.push((
                GlobalCell::new(2 * s, 1, s),
                GlobalCell::new(11 * s, 4 * s, dimensions[2] as i64 - 1),
            ));
        } else {
            water_boxes.push((
                GlobalCell::new(3 * s, 1, z_min),
                GlobalCell::new(11 * s, 5 * s, z_max),
            ));
            water_boxes.push((
                GlobalCell::new(14 * s, 1, z_min),
                GlobalCell::new(21 * s, 3 * s, z_max),
            ));
        }
        if tunnel_pool {
            water_boxes.push((
                GlobalCell::new(15 * s, 5 * s, 2 * s),
                GlobalCell::new(20 * s, 7 * s, 6 * s),
            ));
        }
        let mut reference_volume_m3 = 0.0f64;
        for (min, max) in water_boxes {
            let open = open_cells_in_box(&boundary, min, max)?;
            for _ in 0..open * SUBCELLS_PER_CELL {
                reference_volume_m3 += f64::from(REFERENCE_SUBCELL_VOLUME_M3);
            }
        }
        if reference_volume_m3 == 0.0 {
            return Err(FixtureError::NoWater);
        }
        Ok(Self {
            volume,
            spec,
            reference_volume_m3,
        })
    }

    pub fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Captured grid bounds of the finite-reservoir scene.
    pub const fn domain(&self) -> DomainSpec {
        self.spec
    }

    /// Water volume of the scenario's declared water boxes, counting only
    /// open (non-solid) cells.
    pub const fn reference_volume_m3(&self) -> f64 {
        self.reference_volume_m3
    }
}

fn open_cells_in_box(
    boundary: &SolidBoundary,
    min: GlobalCell,
    max_exclusive: GlobalCell,
) -> Result<usize, FixtureError> {
    let spec = boundary.spec();
    let inside = |c: GlobalCell| {
        let o = spec.origin();
        let d = spec.dimensions();
        (0..3).all(|a| {
            let (v, lo, n) = match a {
                0 => (c.x, o.x, d[0]),
                1 => (c.y, o.y, d[1]),
                _ => (c.z, o.z, d[2]),
            };
            v >= lo && v < lo + i64::from(n)
        })
    };
    let last = GlobalCell::new(
        max_exclusive.x - 1,
        max_exclusive.y - 1,
        max_exclusive.z - 1,
    );
    if min.x >= max_exclusive.x
        || min.y >= max_exclusive.y
        || min.z >= max_exclusive.z
        || !inside(min)
        || !inside(last)
    {
        return Err(FixtureError::WaterBoxOutsideDomain);
    }
    let mut open = 0;
    for z in min.z..max_exclusive.z {
        for y in min.y..max_exclusive.y {
            for x in min.x..max_exclusive.x {
                if boundary.is_solid(GlobalCell::new(x, y, z)) == Some(false) {
                    open += 1;
                }
            }
        }
    }
    Ok(open)
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("scale must be nonzero")]
    InvalidParameters,
    #[error("invalid bounded fixture domain: {0}")]
    Domain(#[from] DomainError),
    #[error("voxel storage failed: {0}")]
    VoxelAccess(#[from] spall_voxel::AccessError),
    #[error("voxel edit failed: {0}")]
    Edit(#[from] EditError),
    #[error("voxel boundary capture failed: {0}")]
    Boundary(#[from] BoundaryError),
    #[error("a declared water box lies outside the domain")]
    WaterBoxOutsideDomain,
    #[error("the declared water boxes contain no open cells")]
    NoWater,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values recorded from the original particle fixtures (count x f32
    /// 0.0015625 summed in f64) before they were removed; the grid evidence
    /// in docs/reports/ENG-103.md was produced from exactly these volumes.
    #[test]
    fn reference_volumes_match_recorded_particle_fixture_bits() {
        type Build = fn(u32) -> Result<ReservoirScene, FixtureError>;
        let cases: [(Build, u32, u64); 9] = [
            (ReservoirScene::new_scaled, 1, 0x4002_6666_6b00_0000),
            (
                ReservoirScene::new_stability_basin,
                1,
                0x4000_3333_3740_0000,
            ),
            (
                ReservoirScene::new_tunnel_under_separate_pool,
                1,
                0x4006_6666_6c00_0000,
            ),
            (ReservoirScene::new_scaled, 2, 0x4035_6666_6bc0_0000),
            (
                ReservoirScene::new_stability_basin,
                2,
                0x4034_7999_9eb8_0000,
            ),
            (
                ReservoirScene::new_tunnel_under_separate_pool,
                2,
                0x4039_6666_6cc0_0000,
            ),
            (ReservoirScene::new_scaled, 3, 0x4052_e666_6b20_0000),
            (
                ReservoirScene::new_stability_basin,
                3,
                0x4052_9000_04a4_0000,
            ),
            (
                ReservoirScene::new_tunnel_under_separate_pool,
                3,
                0x4056_4666_6bf8_0000,
            ),
        ];
        for (build, scale, bits) in cases {
            let scene = build(scale).unwrap();
            assert_eq!(
                scene.reference_volume_m3().to_bits(),
                bits,
                "scale {scale}: {}",
                scene.reference_volume_m3()
            );
        }
    }

    #[test]
    fn scene_walls_and_dam_are_solid_and_reservoirs_open() {
        let scene = ReservoirScene::new_scaled(1).unwrap();
        let boundary = SolidBoundary::capture(scene.volume(), scene.domain()).unwrap();
        assert_eq!(boundary.is_solid(GlobalCell::new(5, 0, 4)), Some(true));
        assert_eq!(boundary.is_solid(GlobalCell::new(0, 3, 4)), Some(true));
        assert_eq!(boundary.is_solid(GlobalCell::new(12, 3, 4)), Some(true));
        assert_eq!(boundary.is_solid(GlobalCell::new(12, 10, 4)), Some(false));
        assert_eq!(boundary.is_solid(GlobalCell::new(5, 3, 4)), Some(false));
        assert_eq!(boundary.is_solid(GlobalCell::new(16, 3, 4)), Some(false));
    }
}
