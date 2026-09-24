//! Runtime spatial grouping above the authoritative brick grid.
//!
//! A region coordinate is derived from a brick coordinate and a caller-owned
//! layout. It is not a persistent identity: changing the layout changes the
//! grouping without rewriting voxel coordinates or topology.

use std::collections::{BTreeMap, BTreeSet};

use spall_core::BrickCoord;

/// Coarse spatial address for one runtime region in a world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegionCoord {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl RegionCoord {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }
}

/// Errors constructing or applying a region layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegionLayoutError {
    #[error("region span must be nonzero on every axis")]
    ZeroSpan,
    #[error("region bounds overflow the signed brick-coordinate range")]
    CoordinateOverflow,
    #[error("region activation radii are invalid")]
    InvalidInterestRadii,
    #[error("active region limit {limit} exceeded (at least {requested_at_least} requested)")]
    ActiveRegionLimit {
        requested_at_least: usize,
        limit: usize,
    },
    #[error("active brick limit {limit} exceeded (at least {requested_at_least} required)")]
    ActiveBrickLimit {
        requested_at_least: u128,
        limit: usize,
    },
}

/// Maps the signed brick lattice into configurable coarse spatial partitions.
///
/// Spans are measured in bricks and may differ per axis. Euclidean division
/// keeps negative brick coordinates in consistently sized regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionLayout {
    bricks_per_region: [u32; 3],
}

impl RegionLayout {
    pub fn new(bricks_per_region: [u32; 3]) -> Result<Self, RegionLayoutError> {
        if bricks_per_region.contains(&0) {
            return Err(RegionLayoutError::ZeroSpan);
        }
        Ok(Self { bricks_per_region })
    }

    pub const fn bricks_per_region(self) -> [u32; 3] {
        self.bricks_per_region
    }

    pub fn region_of(self, brick: BrickCoord) -> RegionCoord {
        RegionCoord::new(
            brick.x.div_euclid(i64::from(self.bricks_per_region[0])),
            brick.y.div_euclid(i64::from(self.bricks_per_region[1])),
            brick.z.div_euclid(i64::from(self.bricks_per_region[2])),
        )
    }

    /// Inclusive brick-coordinate bounds for one region.
    pub fn brick_bounds(
        self,
        region: RegionCoord,
    ) -> Result<(BrickCoord, BrickCoord), RegionLayoutError> {
        let spans = self.bricks_per_region.map(i64::from);
        let min = [region.x, region.y, region.z]
            .into_iter()
            .zip(spans)
            .map(|(coordinate, span)| {
                coordinate
                    .checked_mul(span)
                    .ok_or(RegionLayoutError::CoordinateOverflow)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let max = min
            .iter()
            .zip(spans)
            .map(|(&minimum, span)| {
                minimum
                    .checked_add(span - 1)
                    .ok_or(RegionLayoutError::CoordinateOverflow)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            BrickCoord::new(min[0], min[1], min[2]),
            BrickCoord::new(max[0], max[1], max[2]),
        ))
    }

    /// Groups coordinates by region in deterministic region and brick order.
    pub fn group_bricks(
        self,
        bricks: impl IntoIterator<Item = BrickCoord>,
    ) -> BTreeMap<RegionCoord, Vec<BrickCoord>> {
        let mut grouped = BTreeMap::<RegionCoord, Vec<BrickCoord>>::new();
        for brick in bricks {
            grouped
                .entry(self.region_of(brick))
                .or_default()
                .push(brick);
        }
        for coords in grouped.values_mut() {
            coords.sort_by_key(|coord| coord.sort_key());
        }
        grouped
    }

    /// Expands a set of active regions to its complete brick footprint.
    /// Fails before enumeration when the rectangular footprint exceeds the
    /// caller's work or memory limit.
    pub fn active_bricks(
        self,
        regions: &ActiveRegions,
        max_bricks: usize,
    ) -> Result<Vec<BrickCoord>, RegionLayoutError> {
        let [sx, sy, sz] = self.bricks_per_region.map(u128::from);
        let per_region = sx.checked_mul(sy).and_then(|n| n.checked_mul(sz)).ok_or(
            RegionLayoutError::ActiveBrickLimit {
                requested_at_least: u128::MAX,
                limit: max_bricks,
            },
        )?;
        let requested = per_region.checked_mul(regions.coords.len() as u128).ok_or(
            RegionLayoutError::ActiveBrickLimit {
                requested_at_least: u128::MAX,
                limit: max_bricks,
            },
        )?;
        if requested > max_bricks as u128 {
            return Err(RegionLayoutError::ActiveBrickLimit {
                requested_at_least: requested,
                limit: max_bricks,
            });
        }

        let mut bricks = Vec::with_capacity(requested as usize);
        for &region in &regions.coords {
            let (min, max) = self.brick_bounds(region)?;
            for z in min.z..=max.z {
                for y in min.y..=max.y {
                    for x in min.x..=max.x {
                        bricks.push(BrickCoord::new(x, y, z));
                    }
                }
            }
        }
        bricks.sort_by_key(|coord| coord.sort_key());
        Ok(bricks)
    }
}

/// Enter/retain radii measured in whole regions. The retain radius must be at
/// least the enter radius to provide hysteresis at region boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionInterestRadii {
    pub enter: i64,
    pub retain: i64,
}

impl RegionInterestRadii {
    pub fn new(enter: i64, retain: i64) -> Option<Self> {
        (enter >= 0 && retain >= enter).then_some(Self { enter, retain })
    }
}

/// Bounded active-region selection for one or more players/interest centers.
/// A failed update leaves the previous selection unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveRegions {
    coords: BTreeSet<RegionCoord>,
}

impl ActiveRegions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn coords(&self) -> impl Iterator<Item = RegionCoord> + '_ {
        self.coords.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.coords.len()
    }

    pub fn is_empty(&self) -> bool {
        self.coords.is_empty()
    }

    /// Updates a Chebyshev cube around every center. Existing regions use the
    /// larger retain radius; all new regions use enter. `max_regions` is a hard
    /// bound, and exceeding it does not partially change the active set.
    pub fn update(
        &mut self,
        layout: RegionLayout,
        centers: impl IntoIterator<Item = BrickCoord>,
        radii: RegionInterestRadii,
        max_regions: usize,
    ) -> Result<(), RegionLayoutError> {
        if radii.enter < 0 || radii.retain < radii.enter {
            return Err(RegionLayoutError::InvalidInterestRadii);
        }
        let centers: Vec<_> = centers.into_iter().map(|c| layout.region_of(c)).collect();
        let mut next = BTreeSet::new();
        for &center in &centers {
            insert_cube(&mut next, center, radii.enter, max_regions)?;
        }
        for &prior in &self.coords {
            if centers
                .iter()
                .any(|center| region_distance(prior, *center) <= radii.retain as u64)
            {
                insert_one(&mut next, prior, max_regions)?;
            }
        }
        self.coords = next;
        Ok(())
    }
}

fn insert_cube(
    target: &mut BTreeSet<RegionCoord>,
    center: RegionCoord,
    radius: i64,
    limit: usize,
) -> Result<(), RegionLayoutError> {
    let min = [center.x, center.y, center.z]
        .map(|coordinate| coordinate.checked_sub(radius))
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(RegionLayoutError::CoordinateOverflow)?;
    let max = [center.x, center.y, center.z]
        .map(|coordinate| coordinate.checked_add(radius))
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(RegionLayoutError::CoordinateOverflow)?;
    for z in min[2]..=max[2] {
        for y in min[1]..=max[1] {
            for x in min[0]..=max[0] {
                insert_one(target, RegionCoord::new(x, y, z), limit)?;
            }
        }
    }
    Ok(())
}

fn insert_one(
    target: &mut BTreeSet<RegionCoord>,
    coord: RegionCoord,
    limit: usize,
) -> Result<(), RegionLayoutError> {
    if target.insert(coord) && target.len() > limit {
        return Err(RegionLayoutError::ActiveRegionLimit {
            requested_at_least: target.len(),
            limit,
        });
    }
    Ok(())
}

fn region_distance(a: RegionCoord, b: RegionCoord) -> u64 {
    a.x.abs_diff(b.x)
        .max(a.y.abs_diff(b.y))
        .max(a.z.abs_diff(b.z))
}
