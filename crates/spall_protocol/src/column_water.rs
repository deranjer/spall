//! Canonical state of the stacked-column water (ENG-167, schema 3).
//!
//! Only wet spans are stored, each keyed by its region, its column inside the
//! region and the global y of its floor, so restore can check every one
//! against the recovered terrain. Volumes are unsigned fixed-point integers
//! (units of 2^-32 cell^3), never float bytes. Velocities, pressure and
//! sleep state are not stored: recovery is at rest.

use serde::{Deserialize, Serialize};

use crate::water::{MAX_WATER_SOURCE_CELLS, WaterCodecError};

/// Schema number of this record. 1 and 2 are the dense MAC `WaterState`.
pub const COLUMN_WATER_VERSION: u16 = 3;
/// Fixed-point scale: stored volume = round(cells^3 * 2^32).
pub const VOLUME_FIXED_ONE: u64 = 1 << 32;
/// Regions one state may carry (a 4096-cell world has 16,384 regions).
pub const MAX_COLUMN_WATER_REGIONS: usize = 1 << 16;
/// Wet spans one state may carry in total.
pub const MAX_COLUMN_WATER_SPANS: usize = 1 << 25;
/// Columns per region (32 x 32).
pub const REGION_COLUMNS: u16 = 1024;

pub fn volume_to_fixed(cells3: f64) -> u64 {
    (cells3 * VOLUME_FIXED_ONE as f64).round() as u64
}

pub fn volume_from_fixed(fixed: u64) -> f64 {
    fixed as f64 / VOLUME_FIXED_ONE as f64
}

/// One wet span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSpanRecord {
    /// `lx + 32 * lz` inside the region.
    pub column: u16,
    /// Global y of the span floor.
    pub floor_y: i32,
    /// Water volume in 2^-32 cell^3, including overfill.
    pub volume: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnRegionRecord {
    pub rx: i32,
    pub rz: i32,
    /// Sorted by `(column, floor_y)`, no duplicates.
    pub spans: Vec<ColumnSpanRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnWaterState {
    pub version: u16,
    /// Global y range `[y_lo, y_hi)` of the water world.
    pub y_lo: i32,
    pub y_hi: i32,
    pub frame_seq: u64,
    /// Simulated fluid time, spring/drain totals and the unplaced ledger, each
    /// in fixed-point (time in 2^-32 s).
    pub fluid_time_fixed: u64,
    pub spring_added_fixed: u64,
    pub drain_removed_fixed: u64,
    pub unplaced_fixed: u64,
    /// Sorted by `(rx, rz)`, no duplicates.
    pub regions: Vec<ColumnRegionRecord>,
    pub sources: Vec<[i64; 3]>,
    pub gated_sources: [Vec<[i64; 3]>; 3],
    pub sinks: Vec<[i64; 3]>,
    pub gated_rate: u8,
}

impl ColumnWaterState {
    pub fn validate(&self) -> Result<(), WaterCodecError> {
        let bad = |what: &str| {
            WaterCodecError::InvalidChunk(format!("invalid canonical column water state: {what}"))
        };
        if self.version != COLUMN_WATER_VERSION {
            return Err(bad("version"));
        }
        if self.frame_seq == 0 || self.gated_rate > 3 {
            return Err(bad("sequence or rate"));
        }
        if self.y_lo >= self.y_hi
            || self.y_lo.rem_euclid(32) != 0
            || self.y_hi.rem_euclid(32) != 0
            || i64::from(self.y_hi) - i64::from(self.y_lo) > 1 << 16
        {
            return Err(bad("height range"));
        }
        if self.regions.len() > MAX_COLUMN_WATER_REGIONS {
            return Err(bad("too many regions"));
        }
        let mut spans = 0usize;
        let mut prev_region: Option<(i32, i32)> = None;
        for region in &self.regions {
            let key = (region.rx, region.rz);
            if prev_region.is_some_and(|p| p >= key) {
                return Err(bad("regions not strictly ordered"));
            }
            prev_region = Some(key);
            spans = spans
                .checked_add(region.spans.len())
                .filter(|n| *n <= MAX_COLUMN_WATER_SPANS)
                .ok_or_else(|| bad("too many spans"))?;
            let mut prev: Option<(u16, i32)> = None;
            for span in &region.spans {
                let id = (span.column, span.floor_y);
                if span.column >= REGION_COLUMNS
                    || span.floor_y < self.y_lo
                    || span.floor_y >= self.y_hi
                    || span.volume == 0
                    || prev.is_some_and(|p| p >= id)
                {
                    return Err(bad("span record"));
                }
                prev = Some(id);
            }
        }
        for list in std::iter::once(&self.sources)
            .chain(self.gated_sources.iter())
            .chain(std::iter::once(&self.sinks))
        {
            if list.len() > MAX_WATER_SOURCE_CELLS {
                return Err(bad("source list"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ColumnWaterState {
        ColumnWaterState {
            version: COLUMN_WATER_VERSION,
            y_lo: 0,
            y_hi: 128,
            frame_seq: 1,
            fluid_time_fixed: 0,
            spring_added_fixed: 0,
            drain_removed_fixed: 0,
            unplaced_fixed: 0,
            regions: vec![ColumnRegionRecord {
                rx: 0,
                rz: 0,
                spans: vec![
                    ColumnSpanRecord {
                        column: 3,
                        floor_y: 5,
                        volume: volume_to_fixed(2.5),
                    },
                    ColumnSpanRecord {
                        column: 4,
                        floor_y: 5,
                        volume: 7,
                    },
                ],
            }],
            sources: Vec::new(),
            gated_sources: [Vec::new(), Vec::new(), Vec::new()],
            sinks: Vec::new(),
            gated_rate: 0,
        }
    }

    #[test]
    fn fixed_point_round_trip_is_idempotent() {
        for v in [0.0, 1.0, 2.5, 123.456789012345, 1e-9] {
            let once = volume_to_fixed(v);
            assert_eq!(volume_to_fixed(volume_from_fixed(once)), once);
            assert!((volume_from_fixed(once) - v).abs() <= 0.5 / VOLUME_FIXED_ONE as f64);
        }
    }

    #[test]
    fn validation_fails_closed() {
        state().validate().unwrap();
        let mut s = state();
        s.version = 2;
        assert!(s.validate().is_err());
        let mut s = state();
        s.regions[0].spans[1].column = 3;
        assert!(s.validate().is_err(), "duplicate or unordered span");
        let mut s = state();
        s.regions[0].spans[0].floor_y = 999;
        assert!(s.validate().is_err(), "floor outside the height range");
        let mut s = state();
        s.regions[0].spans[0].column = REGION_COLUMNS;
        assert!(s.validate().is_err());
        let mut s = state();
        s.regions.push(s.regions[0].clone());
        assert!(s.validate().is_err(), "duplicate region");
    }
}
