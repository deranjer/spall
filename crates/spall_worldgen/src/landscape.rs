//! The coarse landscape: landform, erosion and hydrology run end to end on one
//! grid (one sample per metre). Later stages (climate, biomes, the 3D fill)
//! sample this grid; nothing here knows about voxels or materials.

use crate::erosion::{ErosionParams, erode};
use crate::hydrology::{
    FP, HydroParams, Hydrology, OUTLET_BASIN, OUTLET_NONE, OUTLET_SEA, TerminalBasin,
    breach_large_pits,
};
use crate::landform::{self, Landform};
use crate::spec::SEA_LEVEL;
use crate::spec::{GenError, MAX_SIZE_CELLS, MIN_SIZE_CELLS};

/// World cells per grid sample (1 m).
pub const SAMPLE_CELLS: i64 = 4;

pub struct Landscape {
    pub n: usize,
    pub seed: u64,
    pub landform: Landform,
    pub hydrology: Hydrology,
}

impl Landscape {
    /// `size_cells` is the arena edge; it must be a multiple of [`SAMPLE_CELLS`].
    pub fn generate(seed: u64, size_cells: u32) -> Result<Self, GenError> {
        if !(MIN_SIZE_CELLS..=MAX_SIZE_CELLS).contains(&size_cells)
            || i64::from(size_cells) % SAMPLE_CELLS != 0
        {
            return Err(GenError::InvalidSize(size_cells));
        }
        let n = (i64::from(size_cells) / SAMPLE_CELLS) as usize;
        let mut landform = landform::generate(seed, n);
        let sea = f64::from(SEA_LEVEL);

        let mut outlet = vec![OUTLET_NONE; n * n];
        for (o, &h) in outlet.iter_mut().zip(&landform.height) {
            if h < sea {
                *o = OUTLET_SEA;
            }
        }
        for b in &landform.basins {
            outlet[b.x + n * b.z] = OUTLET_BASIN;
        }
        // Broad basins drain instead of becoming lakes the size of a county.
        let params = HydroParams::for_grid(n, SEA_LEVEL * FP);
        let mut raw: Vec<i32> = landform
            .height
            .iter()
            .map(|h| (h * f64::from(FP)).floor() as i32)
            .collect();
        breach_large_pits(&mut raw, n, &outlet, params.max_lake_area, 10);
        for (h, &r) in landform.height.iter_mut().zip(&raw) {
            *h = h.min(f64::from(r) / f64::from(FP));
        }
        erode(
            &mut landform.height,
            n,
            &outlet,
            &landform.uplift,
            &ErosionParams::default(),
        );

        // Drainage on the eroded surface. Eroded land that slipped below the
        // sea joins the sea.
        let fixed: Vec<i32> = landform
            .height
            .iter()
            .map(|h| (h * f64::from(FP)).floor() as i32)
            .collect();
        let sea_fp = SEA_LEVEL * FP;
        for (o, &h) in outlet.iter_mut().zip(&fixed) {
            if *o == OUTLET_NONE && h < sea_fp {
                *o = OUTLET_SEA;
            }
        }
        let basins: Vec<TerminalBasin> = landform
            .basins
            .iter()
            .map(|b| TerminalBasin {
                cell: (b.x + n * b.z) as u32,
                level: (b.level * f64::from(FP)).floor() as i32,
            })
            .collect();
        let hydrology = Hydrology::compute(&fixed, n, &outlet, &basins, &params);
        Ok(Self {
            n,
            seed,
            landform,
            hydrology,
        })
    }
}
