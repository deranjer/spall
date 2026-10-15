//! Immutable sky-down terrain queries for authoritative snow exposure.
//! One tile lies within one brick XZ column. Air/full occupancy skips whole
//! bricks; mixed bricks use immutable local reads instead of global hash lookups.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, LocalCell};
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
    let bounds = volume.bounds().ok_or(SnowTileQueryError::MissingBounds)?;
    let cell_min = |axis: i64| {
        axis.checked_mul(32)
            .ok_or(SnowTileQueryError::CoordinateRange)
    };
    let cell_max = |axis: i64| {
        axis.checked_mul(32)
            .and_then(|value| value.checked_add(31))
            .ok_or(SnowTileQueryError::CoordinateRange)
    };
    let min = GlobalCell::new(
        cell_min(bounds.min.x)?,
        cell_min(bounds.min.y)?,
        cell_min(bounds.min.z)?,
    );
    let max = GlobalCell::new(
        cell_max(bounds.max.x)?,
        cell_max(bounds.max.y)?,
        cell_max(bounds.max.z)?,
    );
    query_snow_tile_in_region(volume, tile_x, tile_z, min, max)
}

/// Query against an explicit, authoritative terrain cell region. This supports
/// natural worlds whose sparse Volume is intentionally unbounded while their
/// worldgen/collider contract supplies the finite sky and ground extent.
pub fn query_snow_tile_in_region(
    volume: &Volume,
    tile_x: i32,
    tile_z: i32,
    min: GlobalCell,
    max: GlobalCell,
) -> Result<SnowTileGeometry, SnowTileQueryError> {
    if volume.cell_size() != CellSizeCode::Quarter {
        return Err(SnowTileQueryError::UnsupportedCellSize);
    }
    if min.x > max.x || min.y > max.y || min.z > max.z {
        return Err(SnowTileQueryError::OutsideBounds);
    }
    let min_brick = BrickCoord::new(
        min.x.div_euclid(32),
        min.y.div_euclid(32),
        min.z.div_euclid(32),
    );
    let max_brick = BrickCoord::new(
        max.x.div_euclid(32),
        max.y.div_euclid(32),
        max.z.div_euclid(32),
    );
    if max_brick.y.saturating_sub(min_brick.y) >= 128 {
        return Err(SnowTileQueryError::VerticalSpan);
    }
    let bx = i64::from(tile_x.div_euclid(2));
    let bz = i64::from(tile_z.div_euclid(2));
    if !(min_brick.x..=max_brick.x).contains(&bx) || !(min_brick.z..=max_brick.z).contains(&bz) {
        return Err(SnowTileQueryError::OutsideBounds);
    }
    let min_y = min_brick
        .y
        .checked_mul(32)
        .ok_or(SnowTileQueryError::CoordinateRange)?;
    let max_y = max_brick
        .y
        .checked_mul(32)
        .and_then(|y| y.checked_add(31))
        .ok_or(SnowTileQueryError::CoordinateRange)?;
    i32::try_from(min_y).map_err(|_| SnowTileQueryError::CoordinateRange)?;
    i32::try_from(max_y).map_err(|_| SnowTileQueryError::CoordinateRange)?;
    let local_x = (tile_x.rem_euclid(2) * 16) as u8;
    let local_z = (tile_z.rem_euclid(2) * 16) as u8;
    let mut pending = [0u64; 4];
    for index in 0..256 {
        let x = bx * 32 + i64::from(local_x) + (index % 16) as i64;
        let z = bz * 32 + i64::from(local_z) + (index / 16) as i64;
        if (min.x..=max.x).contains(&x) && (min.z..=max.z).contains(&z) {
            pending[index / 64] |= 1u64 << (index % 64);
        }
    }
    let mut result = SnowTileGeometry::default();
    for by in (min_brick.y..=max_brick.y).rev() {
        if min.y > by * 32 + 31 || max.y < by * 32 {
            continue;
        }
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
                let y = (by * 32 + 31).clamp(min.y, max.y) as i32;
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
            let global_y = by * 32 + i64::from(ly);
            if !(min.y..=max.y).contains(&global_y) {
                continue;
            }
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
                    let y = global_y as i32;
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
    fn explicit_region_supports_unbounded_sparse_worldgen_volume() {
        let mut volume = Volume::new(VolumeId::new(9).unwrap(), CellSizeCode::Quarter);
        let mut sky = Brick::uniform(MaterialId::AIR, Revision(1));
        sky.set_cell(LocalCell::new(0, 0, 0).unwrap(), MaterialId(1));
        volume.insert_brick(BrickCoord::new(0, 1, 0), sky).unwrap();
        let result = query_snow_tile_in_region(
            &volume,
            0,
            0,
            GlobalCell::new(0, 32, 0),
            GlobalCell::new(15, 63, 15),
        )
        .unwrap();
        assert_eq!(result.layers.len(), 1);
        assert_eq!(result.layers[&32].exposed_faces[0], 1);
        assert_eq!(result.layers[&32].geometry_revision, 1);
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
