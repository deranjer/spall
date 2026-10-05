//! Shared bounded ENG-122 probe/viewer fixtures. No runtime/game integration.
use crate::cut_cell::{CutCellGeometry, GeometryLimits};
use crate::phase_water::{PhaseError, PhaseLimits, PhaseWater};
use crate::{DomainSpec, SolidBoundary};
use spall_core::GlobalCell;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseReferenceScene {
    Channel,
    LowDam,
    FullWall,
}
impl PhaseReferenceScene {
    pub fn dimensions(self) -> [u32; 3] {
        if self == Self::Channel {
            [48, 18, 12]
        } else {
            [9, 6, 3]
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Channel => "Channel surge",
            Self::LowDam => "Low dam",
            Self::FullWall => "Full wall",
        }
    }
    pub fn build(self) -> Result<PhaseWater, PhaseError> {
        let dims = self.dimensions();
        let spec = DomainSpec::new(GlobalCell::new(0, 0, 0), dims, 30_000)
            .map_err(|_| PhaseError::InvalidState)?;
        let mut solid = Vec::with_capacity(spec.cell_count());
        let mut seed = Vec::with_capacity(spec.cell_count());
        for z in 0..dims[2] {
            for y in 0..dims[1] {
                for x in 0..dims[0] {
                    let wall = if self == Self::Channel {
                        y < 3 || (x >= 18 && !(3..9).contains(&z) && y < 12)
                    } else {
                        x == 4 && (self == Self::FullWall || y < 5)
                    };
                    let wet = !wall
                        && if self == Self::Channel {
                            x < 18 && y < 9
                        } else {
                            x < 4 && y < 2
                        };
                    solid.push(wall);
                    seed.push(if wet { 1.0 } else { 0.0 });
                }
            }
        }
        let geometry = Arc::new(CutCellGeometry::build(
            &SolidBoundary { spec, solid },
            3,
            GeometryLimits {
                max_fine_cells: 30_000,
                max_components: 30_000,
                max_portals: 90_000,
            },
        )?);
        PhaseWater::new(
            geometry,
            &seed,
            0.25,
            PhaseLimits {
                max_fine_cells: 30_000,
                max_faces: 90_000,
                max_basins: 30_000,
            },
        )
    }
}
