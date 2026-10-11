//! Canonical state of the stacked-column water (ENG-167; schema 4 adds
//! discharge springs, ENG-188).
//!
//! Only wet spans are stored, each keyed by its region, its column inside the
//! region and the global y of its floor, so restore can check every one
//! against the recovered terrain. Volumes are unsigned fixed-point integers
//! (units of 2^-32 cell^3), never float bytes. Velocities, pressure and
//! sleep state are not stored: recovery is at rest.

use serde::{Deserialize, Serialize};

use crate::water::{MAX_WATER_SOURCE_CELLS, WaterCodecError};

/// Schema number of this record. 1 and 2 are the dense MAC `WaterState`; 3 had
/// no discharge springs and is rejected (older saves are not migrated).
pub const COLUMN_WATER_VERSION: u16 = 4;
/// Fixed-point scale of a discharge: stored flow = round(cells^3/s * 2^16).
pub const DISCHARGE_FIXED_ONE: u32 = 1 << 16;
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

/// A spring with a fixed discharge: the span holding `cell` gains `flow` cells^3
/// of water per second, whatever the channel carries away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DischargeRecord {
    pub cell: [i64; 3],
    /// Cells^3 per second in units of 2^-16 (never float bytes).
    pub flow_fixed: u32,
}

pub fn discharge_to_fixed(cells3_per_s: f64) -> u32 {
    (cells3_per_s * f64::from(DISCHARGE_FIXED_ONE))
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}

pub fn discharge_from_fixed(fixed: u32) -> f64 {
    f64::from(fixed) / f64::from(DISCHARGE_FIXED_ONE)
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
    /// Springs with a fixed discharge.
    pub discharges: Vec<DischargeRecord>,
}

impl ColumnWaterState {
    pub fn validate(&self) -> Result<(), WaterCodecError> {
        self.validate_inner(false)
    }

    fn validate_inner(&self, allow_empty_regions: bool) -> Result<(), WaterCodecError> {
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
            if region.spans.is_empty() && !allow_empty_regions {
                return Err(bad("empty region"));
            }
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
        if self.discharges.len() > MAX_WATER_SOURCE_CELLS {
            return Err(bad("discharge list"));
        }
        Ok(())
    }
}

/// Changed regions since the previous journal record, replacing those regions
/// of the last state wholesale (a region with no spans is now dry). Scalars are
/// the new absolute values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnWaterDelta {
    pub frame_seq: u64,
    pub fluid_time_fixed: u64,
    pub spring_added_fixed: u64,
    pub drain_removed_fixed: u64,
    pub unplaced_fixed: u64,
    pub gated_rate: u8,
    /// Sorted by `(rx, rz)`, no duplicates; each region's spans are ordered
    /// like [`ColumnRegionRecord::spans`].
    pub regions: Vec<ColumnRegionRecord>,
}

impl ColumnWaterDelta {
    /// Checks the delta against the height range it will be applied under.
    pub fn validate(&self, y_lo: i32, y_hi: i32) -> Result<(), WaterCodecError> {
        // Reuse the state validator on a shell carrying just these regions.
        let shell = ColumnWaterState {
            version: COLUMN_WATER_VERSION,
            y_lo,
            y_hi,
            frame_seq: self.frame_seq.max(1),
            fluid_time_fixed: 0,
            spring_added_fixed: 0,
            drain_removed_fixed: 0,
            unplaced_fixed: 0,
            regions: self.regions.clone(),
            sources: Vec::new(),
            gated_sources: [Vec::new(), Vec::new(), Vec::new()],
            sinks: Vec::new(),
            gated_rate: self.gated_rate,
            discharges: Vec::new(),
        };
        shell.validate_inner(true)
    }
}

impl ColumnWaterState {
    /// Applies a journaled delta: replaces the listed regions and takes the new
    /// scalars. The state stays valid or is left unchanged on error.
    pub fn apply_delta(&mut self, delta: &ColumnWaterDelta) -> Result<(), WaterCodecError> {
        delta.validate(self.y_lo, self.y_hi)?;
        if delta.frame_seq < self.frame_seq {
            return Err(WaterCodecError::InvalidChunk(
                "column water delta goes back in time".into(),
            ));
        }
        let mut merged: std::collections::BTreeMap<(i32, i32), ColumnRegionRecord> = self
            .regions
            .iter()
            .map(|r| ((r.rx, r.rz), r.clone()))
            .collect();
        for region in &delta.regions {
            if region.spans.is_empty() {
                merged.remove(&(region.rx, region.rz));
            } else {
                merged.insert((region.rx, region.rz), region.clone());
            }
        }
        let regions: Vec<_> = merged.into_values().collect();
        let mut next = self.clone();
        next.regions = regions;
        next.frame_seq = delta.frame_seq;
        next.fluid_time_fixed = delta.fluid_time_fixed;
        next.spring_added_fixed = delta.spring_added_fixed;
        next.drain_removed_fixed = delta.drain_removed_fixed;
        next.unplaced_fixed = delta.unplaced_fixed;
        next.gated_rate = delta.gated_rate;
        next.validate()?;
        *self = next;
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
            discharges: Vec::new(),
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
    fn deltas_replace_regions_and_stay_valid() {
        let mut s = state();
        let delta = ColumnWaterDelta {
            frame_seq: 5,
            fluid_time_fixed: 9,
            spring_added_fixed: 0,
            drain_removed_fixed: 0,
            unplaced_fixed: 3,
            gated_rate: 1,
            regions: vec![
                ColumnRegionRecord {
                    rx: 0,
                    rz: 0,
                    spans: vec![ColumnSpanRecord {
                        column: 9,
                        floor_y: 2,
                        volume: 11,
                    }],
                },
                ColumnRegionRecord {
                    rx: 1,
                    rz: 0,
                    spans: vec![ColumnSpanRecord {
                        column: 0,
                        floor_y: 0,
                        volume: 1,
                    }],
                },
            ],
        };
        s.apply_delta(&delta).unwrap();
        assert_eq!(s.regions.len(), 2);
        assert_eq!(s.regions[0].spans.len(), 1, "region 0 replaced wholesale");
        assert_eq!((s.frame_seq, s.unplaced_fixed, s.gated_rate), (5, 3, 1));
        let dry = ColumnWaterDelta {
            frame_seq: 6,
            regions: vec![ColumnRegionRecord {
                rx: 0,
                rz: 0,
                spans: Vec::new(),
            }],
            ..delta.clone()
        };
        s.apply_delta(&dry).unwrap();
        assert_eq!(s.regions.len(), 1, "an emptied region is removed");
        let back = ColumnWaterDelta {
            frame_seq: 1,
            ..dry
        };
        let before = s.clone();
        assert!(s.apply_delta(&back).is_err());
        assert_eq!(s, before, "a refused delta changes nothing");
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
