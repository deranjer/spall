//! Terrain read at 1 m columns (4 x 4 terrain cells each) for wide water
//! columns (`ColumnConfig::column_cells`).

use spall_core::{BrickCoord, LocalCell, MaterialId};
use spall_voxel::Volume;

use super::{REGION_COLUMNS, REGION_EDGE, RegionKey, RegionTerrain, TerrainSource};

/// Terrain cells per coarse column edge.
pub const COARSE_CELLS: i64 = 4;

/// Terrain read from a voxel volume at one column per 4 x 4 terrain cells,
/// keeping the terrain's vertical resolution. A coarse cell is solid if any
/// terrain cell in it is solid, so a wall one cell thick still holds water
/// (and a channel narrower than a column carries none). Coarse region `(kx,
/// kz)` covers 128 x 128 terrain cells. A missing brick makes the region
/// non-resident.
pub struct CoarseVolumeTerrain<V> {
    volume: V,
    y_lo: i32,
    y_hi: i32,
}

impl<V: std::ops::Deref<Target = Volume> + Send + Sync> CoarseVolumeTerrain<V> {
    pub fn new(volume: V, y_lo: i32, y_hi: i32) -> Self {
        assert!(
            i64::from(y_lo) % REGION_EDGE == 0 && i64::from(y_hi) % REGION_EDGE == 0,
            "CoarseVolumeTerrain range must be brick aligned"
        );
        Self { volume, y_lo, y_hi }
    }
}

impl<V: std::ops::Deref<Target = Volume> + Send + Sync> TerrainSource for CoarseVolumeTerrain<V> {
    fn y_range(&self) -> (i32, i32) {
        (self.y_lo, self.y_hi)
    }

    fn region(&self, key: RegionKey) -> Option<RegionTerrain> {
        let height = (self.y_hi - self.y_lo) as usize;
        let edge = REGION_EDGE as usize;
        let mut solid = vec![false; REGION_COLUMNS * height];
        let bricks_per = COARSE_CELLS; // terrain bricks per coarse region edge
        let (by_lo, by_hi) = (
            i64::from(self.y_lo) / REGION_EDGE,
            i64::from(self.y_hi) / REGION_EDGE,
        );
        for bz in 0..bricks_per {
            for bx in 0..bricks_per {
                for by in by_lo..by_hi {
                    let coord = BrickCoord::new(
                        i64::from(key.0) * bricks_per + bx,
                        by,
                        i64::from(key.1) * bricks_per + bz,
                    );
                    let snapshot = self.volume.snapshot_brick(coord).ok()??;
                    let y_base = (by * REGION_EDGE) as usize - self.y_lo as usize;
                    let zero = LocalCell::new(0, 0, 0).expect("in-brick");
                    if !snapshot.is_dense() && snapshot.get(zero) == MaterialId::AIR {
                        continue;
                    }
                    for lz in 0..edge {
                        for ly in 0..edge {
                            for lx in 0..edge {
                                let local =
                                    LocalCell::new(lx as u8, ly as u8, lz as u8).expect("in-brick");
                                if snapshot.get(local) == MaterialId::AIR {
                                    continue;
                                }
                                // Terrain cell -> coarse column within this region.
                                let cx = (bx as usize * edge + lx) / COARSE_CELLS as usize;
                                let cz = (bz as usize * edge + lz) / COARSE_CELLS as usize;
                                solid[cx + edge * (y_base + ly + height * cz)] = true;
                            }
                        }
                    }
                }
            }
        }
        Some(RegionTerrain { solid })
    }
}
