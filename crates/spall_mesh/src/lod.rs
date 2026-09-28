//! Experimental render-only LOD transition helpers.
//!
//! This module currently handles one narrow case: stitching the boundary
//! profiles of two axis-aligned heightfield chunks at an integer resolution
//! ratio. It is not used by authoritative voxel, collision, or support code.

use spall_core::MaterialId;

use crate::face::FaceDir;
use crate::mesh::FaceQuad;

/// A vertical boundary patch between a fine and a coarser heightfield edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeightfieldSeamPatch {
    /// Vertical quads that close the difference between the two edge profiles.
    pub quads: Vec<FaceQuad>,
    /// Fine edge intervals whose heights differ from the expanded coarse edge.
    pub mismatched_intervals: usize,
    /// Total vertical difference, in cell-face units, before stitching.
    pub unmatched_area_cells: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LodSeamError {
    #[error("LOD ratio must be at least 2")]
    InvalidRatio,
    #[error("coarse profile length times ratio does not match fine profile length")]
    ProfileLengthMismatch,
    #[error("seam coordinates overflow the signed cell-coordinate range")]
    CoordinateOverflow,
}

/// Creates render-only vertical quads between corresponding heightfield edges.
/// Each coarse sample covers `ratio` fine intervals. A patch faces toward the
/// lower side of the profile step. Heights are in integer cells; no voxel data
/// is sampled or modified by this helper.
pub fn stitch_heightfield_edge(
    fine_heights: &[i64],
    coarse_heights: &[i64],
    ratio: usize,
    seam_x: i64,
    fine_z0: i64,
    material: MaterialId,
) -> Result<HeightfieldSeamPatch, LodSeamError> {
    if ratio < 2 {
        return Err(LodSeamError::InvalidRatio);
    }
    if coarse_heights.len().checked_mul(ratio) != Some(fine_heights.len()) {
        return Err(LodSeamError::ProfileLengthMismatch);
    }

    let mut quads = Vec::new();
    let mut mismatched_intervals = 0usize;
    let mut unmatched_area_cells = 0u64;
    for (fine_i, &fine_y) in fine_heights.iter().enumerate() {
        let coarse_y = coarse_heights[fine_i / ratio];
        if fine_y == coarse_y {
            continue;
        }
        let z = fine_z0
            .checked_add(i64::try_from(fine_i).map_err(|_| LodSeamError::CoordinateOverflow)?)
            .ok_or(LodSeamError::CoordinateOverflow)?;
        let low = fine_y.min(coarse_y);
        let high = fine_y.max(coarse_y);
        let height = high
            .checked_sub(low)
            .ok_or(LodSeamError::CoordinateOverflow)?;
        let (dir, u0, v0, u_len, v_len) = if fine_y > coarse_y {
            (FaceDir::PosX, low, z, height, 1)
        } else {
            (FaceDir::NegX, z, low, 1, height)
        };
        quads.push(FaceQuad {
            dir,
            material,
            plane: seam_x,
            u0,
            v0,
            u_len,
            v_len,
            ao: [0; 4],
        });
        mismatched_intervals += 1;
        unmatched_area_cells = unmatched_area_cells
            .checked_add(height as u64)
            .ok_or(LodSeamError::CoordinateOverflow)?;
    }

    Ok(HeightfieldSeamPatch {
        quads,
        mismatched_intervals,
        unmatched_area_cells,
    })
}
