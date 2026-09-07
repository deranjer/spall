//! Integer world coordinates.
//!
//! Geometry is addressed by integer cells. A brick is a fixed `32 x 32 x 32`
//! block of cells; `BrickCoord` indexes bricks and `LocalCell` indexes a cell
//! inside one brick. Global cells convert to `(brick, local)` with Euclidean
//! division so that negative coordinates behave: global cell `-1` maps to brick
//! `-1`, local `31`.
//!
//! All conversions use checked arithmetic and refuse to wrap.

use serde::{Deserialize, Serialize};

/// Cells along one edge of a brick.
pub const BRICK_EDGE: u32 = 32;
/// Cells contained in one brick (`32 * 32 * 32`).
pub const CELLS_PER_BRICK: usize = (BRICK_EDGE * BRICK_EDGE * BRICK_EDGE) as usize;

const EDGE_I64: i64 = BRICK_EDGE as i64;

/// A cell position inside a single brick. Each axis is `0..32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LocalCell {
    x: u8,
    y: u8,
    z: u8,
}

impl LocalCell {
    /// Returns `None` if any axis is `>= 32`.
    pub const fn new(x: u8, y: u8, z: u8) -> Option<Self> {
        if x as u32 >= BRICK_EDGE || y as u32 >= BRICK_EDGE || z as u32 >= BRICK_EDGE {
            return None;
        }
        Some(Self { x, y, z })
    }

    #[inline]
    pub const fn x(self) -> u8 {
        self.x
    }
    #[inline]
    pub const fn y(self) -> u8 {
        self.y
    }
    #[inline]
    pub const fn z(self) -> u8 {
        self.z
    }

    /// Canonical linear index: `x + 32 * (y + 32 * z)`, always `0..32768`.
    #[inline]
    pub const fn linear_index(self) -> u16 {
        let x = self.x as u16;
        let y = self.y as u16;
        let z = self.z as u16;
        x + BRICK_EDGE as u16 * (y + BRICK_EDGE as u16 * z)
    }

    /// Inverse of [`LocalCell::linear_index`]. `None` if `index >= 32768`.
    pub const fn from_linear_index(index: u16) -> Option<Self> {
        if index as usize >= CELLS_PER_BRICK {
            return None;
        }
        let edge = BRICK_EDGE as u16;
        let x = index % edge;
        let y = (index / edge) % edge;
        let z = index / (edge * edge);
        Some(Self {
            x: x as u8,
            y: y as u8,
            z: z as u8,
        })
    }
}

/// Index of a brick in the infinite brick lattice. Signed on every axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BrickCoord {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl BrickCoord {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }

    /// Canonical sort key: `(z, y, x)` so a brick stream is stable and
    /// independent of insertion order.
    #[inline]
    pub const fn sort_key(self) -> (i64, i64, i64) {
        (self.z, self.y, self.x)
    }
}

/// A cell in global integer space, independent of brick boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GlobalCell {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

/// Raised when a coordinate conversion would overflow `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("coordinate arithmetic overflowed i64")]
pub struct CoordOverflow;

impl GlobalCell {
    pub const fn new(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }

    /// Splits a global cell into its brick and in-brick local cell using
    /// Euclidean division. Never fails: every `i64` triple has a home.
    pub const fn split(self) -> (BrickCoord, LocalCell) {
        let (bx, lx) = split_axis(self.x);
        let (by, ly) = split_axis(self.y);
        let (bz, lz) = split_axis(self.z);
        (
            BrickCoord {
                x: bx,
                y: by,
                z: bz,
            },
            // `rem_euclid(32)` is always `0..32`, so `new` cannot return `None`.
            match LocalCell::new(lx, ly, lz) {
                Some(cell) => cell,
                None => unreachable!(),
            },
        )
    }

    /// Rebuilds a global cell from a brick and local cell, with checked
    /// arithmetic. Returns `Err` only on `i64` overflow at the extreme edges of
    /// the coordinate range.
    pub const fn from_parts(brick: BrickCoord, local: LocalCell) -> Result<Self, CoordOverflow> {
        let x = match join_axis(brick.x, local.x) {
            Some(v) => v,
            None => return Err(CoordOverflow),
        };
        let y = match join_axis(brick.y, local.y) {
            Some(v) => v,
            None => return Err(CoordOverflow),
        };
        let z = match join_axis(brick.z, local.z) {
            Some(v) => v,
            None => return Err(CoordOverflow),
        };
        Ok(Self { x, y, z })
    }
}

#[inline]
const fn split_axis(global: i64) -> (i64, u8) {
    // `div_euclid` / `rem_euclid` give a non-negative remainder in `0..32`.
    (
        global.div_euclid(EDGE_I64),
        global.rem_euclid(EDGE_I64) as u8,
    )
}

#[inline]
const fn join_axis(brick: i64, local: u8) -> Option<i64> {
    match brick.checked_mul(EDGE_I64) {
        Some(base) => base.checked_add(local as i64),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance list: cells straddling brick `0` and brick `-1`/`-2`.
    #[test]
    fn euclidean_split_matches_specified_boundaries() {
        let cases = [
            (-33_i64, -2_i64, 31_u8),
            (-32, -1, 0),
            (-1, -1, 31),
            (0, 0, 0),
            (31, 0, 31),
            (32, 1, 0),
        ];
        for (global, brick_x, local_x) in cases {
            let (brick, local) = GlobalCell::new(global, 7, -9).split();
            assert_eq!(brick.x, brick_x, "brick for global {global}");
            assert_eq!(local.x(), local_x, "local for global {global}");
            // Round-trips exactly.
            let back = GlobalCell::from_parts(brick, local).unwrap();
            assert_eq!(back.x, global);
        }
    }

    #[test]
    fn split_join_round_trips_over_a_wide_range() {
        for global in (-2048..2048).step_by(7) {
            let cell = GlobalCell::new(global, global * 2, -global);
            let (brick, local) = cell.split();
            assert_eq!(GlobalCell::from_parts(brick, local).unwrap(), cell);
        }
    }

    #[test]
    fn extreme_inputs_report_overflow_instead_of_wrapping() {
        let (brick, local) = GlobalCell::new(i64::MAX, 0, 0).split();
        assert_eq!(brick.x, i64::MAX.div_euclid(EDGE_I64));
        // Rebuilding the top brick with a non-zero local cell overflows.
        let max_local = LocalCell::new(31, 0, 0).unwrap();
        assert_eq!(
            GlobalCell::from_parts(BrickCoord::new(i64::MAX, 0, 0), max_local),
            Err(CoordOverflow)
        );
        let _ = local;
    }

    #[test]
    fn local_cell_rejects_out_of_range_axes() {
        assert!(LocalCell::new(31, 31, 31).is_some());
        assert!(LocalCell::new(32, 0, 0).is_none());
        assert!(LocalCell::new(0, 200, 0).is_none());
    }

    #[test]
    fn linear_index_is_the_canonical_formula_and_round_trips() {
        for z in 0..32u8 {
            for y in [0u8, 1, 15, 31] {
                for x in [0u8, 7, 31] {
                    let cell = LocalCell::new(x, y, z).unwrap();
                    let expected = x as u16 + 32 * (y as u16 + 32 * z as u16);
                    assert_eq!(cell.linear_index(), expected);
                    assert_eq!(LocalCell::from_linear_index(expected), Some(cell));
                }
            }
        }
        assert_eq!(LocalCell::from_linear_index(32_768), None);
    }
}
