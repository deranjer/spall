//! Immutable sky-down terrain queries for authoritative snow exposure.
//! One tile lies within one brick XZ column. Air/full occupancy skips whole
//! bricks; mixed bricks use immutable local reads instead of global hash lookups.

use spall_core::{BrickCoord, CellSizeCode, LocalCell};
use spall_voxel::Volume;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnowLayerGeometry {
    pub exposed_faces: [u64; 4],
    pub geometry_revision: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SnowTileGeometry {
    pub layers: BTreeMap<i32, SnowLayerGeometry>,
    pub visited_bricks: usize,
    pub sampled_cells: usize,
    pub skipped_empty_bricks: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnowTileQueryError {
    UnsupportedCellSize,
    MissingBounds,
    OutsideBounds,
    VerticalSpan,
    CoordinateRange,
    UnknownGeometry,
}

/// Find the first solid below known sky for each of the 256 tile columns.
/// A roof masks lower surfaces, including floors sharing its XZ column. If any
/// unresolved column crosses absent/failed geometry the entire result defers.
/// No snow/climate state is changed, and callers separately exclude fluid cover.
pub fn query_snow_tile(
    volume: &Volume,
    tile_x: i32,
    tile_z: i32,
) -> Result<SnowTileGeometry, SnowTileQueryError> {
    if volume.cell_size() != CellSizeCode::Quarter {
        return Err(SnowTileQueryError::UnsupportedCellSize);
    }
    let bounds = volume.bounds().ok_or(SnowTileQueryError::MissingBounds)?;
    if bounds.max.y.saturating_sub(bounds.min.y) >= 128 {
        return Err(SnowTileQueryError::VerticalSpan);
    }
    let bx = i64::from(tile_x.div_euclid(2));
    let bz = i64::from(tile_z.div_euclid(2));
    if !(bounds.min.x..=bounds.max.x).contains(&bx) || !(bounds.min.z..=bounds.max.z).contains(&bz)
    {
        return Err(SnowTileQueryError::OutsideBounds);
    }
    let min_y = bounds
        .min
        .y
        .checked_mul(32)
        .ok_or(SnowTileQueryError::CoordinateRange)?;
    let max_y = bounds
        .max
        .y
        .checked_mul(32)
        .and_then(|y| y.checked_add(31))
        .ok_or(SnowTileQueryError::CoordinateRange)?;
    i32::try_from(min_y).map_err(|_| SnowTileQueryError::CoordinateRange)?;
    i32::try_from(max_y).map_err(|_| SnowTileQueryError::CoordinateRange)?;
    let local_x = (tile_x.rem_euclid(2) * 16) as u8;
    let local_z = (tile_z.rem_euclid(2) * 16) as u8;
    let mut pending = [u64::MAX; 4];
    let mut result = SnowTileGeometry::default();
    for by in (bounds.min.y..=bounds.max.y).rev() {
        if pending == [0; 4] {
            break;
        }
        let brick = volume
            .snapshot_brick(BrickCoord::new(bx, by, bz))
            .map_err(|_| SnowTileQueryError::OutsideBounds)?
            .ok_or(SnowTileQueryError::UnknownGeometry)?;
        result.visited_bricks += 1;
        match brick.solid_cells() {
            0 => {
                result.skipped_empty_bricks += 1;
                continue;
            }
            32_768 => {
                let y = (by * 32 + 31) as i32;
                result.layers.insert(
                    y,
                    SnowLayerGeometry {
                        exposed_faces: pending,
                        geometry_revision: brick.revision().get(),
                    },
                );
                pending = [0; 4];
                continue;
            }
            _ => {}
        }
        for ly in (0..32u8).rev() {
            if pending == [0; 4] {
                break;
            }
            for index in 0..256 {
                let word = index / 64;
                let bit = 1u64 << (index % 64);
                if pending[word] & bit == 0 {
                    continue;
                }
                let local = LocalCell::new(
                    local_x + (index % 16) as u8,
                    ly,
                    local_z + (index / 16) as u8,
                )
                .expect("aligned tile fits its brick");
                result.sampled_cells += 1;
                if !brick.get(local).is_air() {
                    let y = (by * 32 + i64::from(ly)) as i32;
                    let layer = result.layers.entry(y).or_insert(SnowLayerGeometry {
                        exposed_faces: [0; 4],
                        geometry_revision: brick.revision().get(),
                    });
                    layer.exposed_faces[word] |= bit;
                    pending[word] &= !bit;
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{GlobalCell, MaterialId, Revision, VolumeId};
    use spall_voxel::{Brick, BrickBounds, EditPlan};

    fn roof_fixture() -> Volume {
        let bounds =
            BrickBounds::new(BrickCoord::new(-1, 0, -1), BrickCoord::new(-1, 2, -1)).unwrap();
        let mut volume = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
        volume
            .insert_brick(
                BrickCoord::new(-1, 0, -1),
                Brick::uniform(MaterialId(1), Revision(1)),
            )
            .unwrap();
        let mut roof = Brick::uniform(MaterialId::AIR, Revision(2));
        roof.set_cell(LocalCell::new(16, 0, 16).unwrap(), MaterialId(1));
        volume
            .insert_brick(BrickCoord::new(-1, 1, -1), roof)
            .unwrap();
        volume
            .insert_brick(
                BrickCoord::new(-1, 2, -1),
                Brick::uniform(MaterialId::AIR, Revision(3)),
            )
            .unwrap();
        volume
    }

    #[test]
    fn negative_tile_roof_masks_floor_and_skips_uniform_air() {
        let volume = roof_fixture();
        let result = query_snow_tile(&volume, -1, -1).unwrap();
        assert_eq!(result.layers.len(), 2);
        assert_eq!(result.layers[&32].exposed_faces, [1, 0, 0, 0]);
        assert_eq!(
            result.layers[&31].exposed_faces,
            [u64::MAX ^ 1, u64::MAX, u64::MAX, u64::MAX]
        );
        assert_eq!(result.layers[&32].geometry_revision, 2);
        assert_eq!(result.visited_bricks, 3);
        assert_eq!(result.skipped_empty_bricks, 1);
        assert!(result.sampled_cells <= 32 * 256);
    }

    #[test]
    fn missing_sky_brick_defers_instead_of_publishing_a_partial_surface() {
        let mut volume = roof_fixture();
        volume.evict_brick(BrickCoord::new(-1, 2, -1));
        assert_eq!(
            query_snow_tile(&volume, -1, -1),
            Err(SnowTileQueryError::UnknownGeometry)
        );
        volume.mark_failed(BrickCoord::new(-1, 2, -1)).unwrap();
        assert_eq!(
            query_snow_tile(&volume, -1, -1),
            Err(SnowTileQueryError::UnknownGeometry)
        );
    }

    #[test]
    fn immutable_snapshot_keeps_old_roof_while_committed_removal_exposes_floor() {
        let mut volume = roof_fixture();
        let snapshot = volume.clone();
        let mut edit = EditPlan::new(volume.id());
        edit.set(GlobalCell::new(-16, 32, -16), MaterialId::AIR);
        volume.apply_edit(&edit).unwrap();
        let old = query_snow_tile(&snapshot, -1, -1).unwrap();
        let current = query_snow_tile(&volume, -1, -1).unwrap();
        assert!(old.layers.contains_key(&32));
        assert_eq!(current.layers.len(), 1);
        assert_eq!(current.layers[&31].exposed_faces, [u64::MAX; 4]);
        assert_eq!(current.layers[&31].geometry_revision, 1);
    }

    #[test]
    fn full_upper_brick_answers_without_reading_cells_or_unknown_below() {
        let bounds = BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(0, 15, 0)).unwrap();
        let mut volume = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
        volume
            .insert_brick(
                BrickCoord::new(0, 15, 0),
                Brick::uniform(MaterialId(1), Revision(9)),
            )
            .unwrap();
        let result = query_snow_tile(&volume, 0, 0).unwrap();
        assert_eq!(result.visited_bricks, 1);
        assert_eq!(result.sampled_cells, 0);
        assert_eq!(result.layers[&511].exposed_faces, [u64::MAX; 4]);
    }

    #[test]
    fn invalid_span_and_outside_tiles_are_explicitly_rejected() {
        let volume = roof_fixture();
        assert_eq!(
            query_snow_tile(&volume, 0, 0),
            Err(SnowTileQueryError::OutsideBounds)
        );
        let bounds =
            BrickBounds::new(BrickCoord::new(0, 0, 0), BrickCoord::new(0, 128, 0)).unwrap();
        let tall = Volume::bounded(VolumeId::new(1).unwrap(), CellSizeCode::Quarter, bounds);
        assert_eq!(
            query_snow_tile(&tall, 0, 0),
            Err(SnowTileQueryError::VerticalSpan)
        );
    }
}
