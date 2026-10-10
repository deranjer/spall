//! Presentation replication of the stacked-column water (ENG-167).
//!
//! A region's 1024 columns are split into [`WINDOWS_PER_REGION`] windows of 128
//! columns; each [`ColumnWaterUpdate`] carries one window, either as a baseline
//! (replaces the window) or a delta (changed spans only, valid on a window at
//! exactly `base_seq`). Records ride the ordered water stream, so a client
//! applies them in order; a delta whose base does not match is refused and the
//! window waits for its next baseline. There is no client fluid lockstep:
//! clients render, and may interpolate, the quantized depths.

use std::collections::BTreeMap;
use std::io::Read;

use serde::{Deserialize, Serialize};
use spall_core::Tick;

use crate::records::{Record, RecordError, WireTag};

/// Columns per window.
pub const WINDOW_COLUMNS: u16 = 128;
pub const WINDOWS_PER_REGION: u8 = 8;
/// Spans one update may carry.
pub const MAX_UPDATE_SPANS: usize = 8192;
/// Compressed payload bytes one update may carry (fits a control record).
pub const MAX_UPDATE_PAYLOAD: usize = 48 * 1024;
const SPAN_BYTES: usize = 8;

/// One wet span as replicated: depth in 1/128 cell, 0 meaning dry (delta only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireSpan {
    pub column: u16,
    pub floor_y: i32,
    pub depth_q: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnWaterUpdate {
    pub server_tick: Tick,
    /// The world frame sequence this update brings the window to.
    pub frame_seq: u64,
    /// `0` for a baseline; otherwise the window sequence this delta applies on.
    pub base_seq: u64,
    pub rx: i32,
    pub rz: i32,
    pub window: u8,
    /// zstd of the spans, each `column u16 | floor_y i32 | depth_q u16` little-endian.
    pub payload: Vec<u8>,
}

impl Record for ColumnWaterUpdate {
    const TAG: WireTag = WireTag::ColumnWaterUpdate;
    const SCHEMA_VERSION: u16 = 1;
    fn validate(&self) -> Result<(), RecordError> {
        if self.window >= WINDOWS_PER_REGION
            || self.frame_seq == 0
            || (self.base_seq != 0 && self.frame_seq <= self.base_seq)
            || self.payload.len() > MAX_UPDATE_PAYLOAD
        {
            return Err(RecordError::Inconsistent("invalid column water update"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ColumnWaterWireError {
    #[error("column water update carries too many spans ({0})")]
    TooManySpans(usize),
    #[error("column water update payload is {0} bytes, above the record limit")]
    PayloadTooLarge(usize),
    #[error("column water update does not decode: {0}")]
    Decode(String),
    #[error("span column {0} is outside its window")]
    ColumnOutsideWindow(u16),
    /// A delta arrived for a window whose sequence is not its base.
    #[error("delta base {base} does not match window sequence {have:?}")]
    BaseMismatch { base: u64, have: Option<u64> },
}

/// The window a column belongs to.
pub fn window_of(column: u16) -> u8 {
    (column / WINDOW_COLUMNS) as u8
}

pub fn encode_update(
    server_tick: Tick,
    frame_seq: u64,
    base_seq: u64,
    key: (i32, i32),
    window: u8,
    spans: &[WireSpan],
) -> Result<ColumnWaterUpdate, ColumnWaterWireError> {
    if spans.len() > MAX_UPDATE_SPANS {
        return Err(ColumnWaterWireError::TooManySpans(spans.len()));
    }
    let mut raw = Vec::with_capacity(spans.len() * SPAN_BYTES);
    for s in spans {
        if window_of(s.column) != window {
            return Err(ColumnWaterWireError::ColumnOutsideWindow(s.column));
        }
        raw.extend_from_slice(&s.column.to_le_bytes());
        raw.extend_from_slice(&s.floor_y.to_le_bytes());
        raw.extend_from_slice(&s.depth_q.to_le_bytes());
    }
    // Level 1: encoded on the tick thread, and these payloads are tiny.
    let payload = zstd::stream::encode_all(raw.as_slice(), 1)
        .map_err(|e| ColumnWaterWireError::Decode(e.to_string()))?;
    if payload.len() > MAX_UPDATE_PAYLOAD {
        return Err(ColumnWaterWireError::PayloadTooLarge(payload.len()));
    }
    Ok(ColumnWaterUpdate {
        server_tick,
        frame_seq,
        base_seq,
        rx: key.0,
        rz: key.1,
        window,
        payload,
    })
}

pub fn decode_spans(update: &ColumnWaterUpdate) -> Result<Vec<WireSpan>, ColumnWaterWireError> {
    let decoder = zstd::stream::Decoder::new(update.payload.as_slice())
        .map_err(|e| ColumnWaterWireError::Decode(e.to_string()))?;
    let mut raw = Vec::new();
    decoder
        .take((MAX_UPDATE_SPANS * SPAN_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|e| ColumnWaterWireError::Decode(e.to_string()))?;
    if raw.len() > MAX_UPDATE_SPANS * SPAN_BYTES || !raw.len().is_multiple_of(SPAN_BYTES) {
        return Err(ColumnWaterWireError::Decode("bad span length".into()));
    }
    let spans: Vec<WireSpan> = raw
        .chunks_exact(SPAN_BYTES)
        .map(|c| WireSpan {
            column: u16::from_le_bytes([c[0], c[1]]),
            floor_y: i32::from_le_bytes([c[2], c[3], c[4], c[5]]),
            depth_q: u16::from_le_bytes([c[6], c[7]]),
        })
        .collect();
    if let Some(bad) = spans.iter().find(|s| window_of(s.column) != update.window) {
        return Err(ColumnWaterWireError::ColumnOutsideWindow(bad.column));
    }
    Ok(spans)
}

#[derive(Debug, Clone, Default)]
struct WindowState {
    seq: Option<u64>,
    spans: BTreeMap<(u16, i32), u16>,
}

#[derive(Debug, Clone, Default)]
struct RegionState {
    windows: [WindowState; WINDOWS_PER_REGION as usize],
}

/// What applying one update changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub key: (i32, i32),
    pub baseline: bool,
    /// The region now holds no water at all.
    pub region_empty: bool,
}

/// A client's copy of the replicated water: wet spans by region.
#[derive(Debug, Clone, Default)]
pub struct ColumnWaterReplica {
    regions: BTreeMap<(i32, i32), RegionState>,
    /// Newest frame sequence applied.
    pub frame_seq: u64,
    pub baselines: u64,
    pub deltas: u64,
}

impl ColumnWaterReplica {
    pub fn apply(&mut self, update: &ColumnWaterUpdate) -> Result<Applied, ColumnWaterWireError> {
        let spans = decode_spans(update)?;
        let key = (update.rx, update.rz);
        let baseline = update.base_seq == 0;
        let region = self.regions.entry(key).or_default();
        let window = &mut region.windows[usize::from(update.window)];
        if baseline {
            window.spans = spans
                .into_iter()
                .filter(|s| s.depth_q > 0)
                .map(|s| ((s.column, s.floor_y), s.depth_q))
                .collect();
            self.baselines += 1;
        } else {
            if window.seq != Some(update.base_seq) {
                let have = window.seq;
                if self.regions.get(&key).is_some_and(region_is_blank) {
                    self.regions.remove(&key);
                }
                return Err(ColumnWaterWireError::BaseMismatch {
                    base: update.base_seq,
                    have,
                });
            }
            for s in spans {
                if s.depth_q == 0 {
                    window.spans.remove(&(s.column, s.floor_y));
                } else {
                    window.spans.insert((s.column, s.floor_y), s.depth_q);
                }
            }
            self.deltas += 1;
        }
        window.seq = Some(update.frame_seq);
        self.frame_seq = self.frame_seq.max(update.frame_seq);
        let region_empty = region.windows.iter().all(|w| w.spans.is_empty());
        if region_empty {
            // An all-dry region needs no storage; its window sequences stay so
            // a later delta is still refused if it skipped a baseline.
            let all_known = region.windows.iter().all(|w| w.seq.is_some());
            if all_known {
                self.regions.remove(&key);
            }
        }
        Ok(Applied {
            key,
            baseline,
            region_empty,
        })
    }

    /// Wet spans of a region as `(column, floor_y, depth_q)`, column order.
    pub fn region_spans(&self, key: (i32, i32)) -> Vec<WireSpan> {
        let Some(region) = self.regions.get(&key) else {
            return Vec::new();
        };
        let mut out: Vec<WireSpan> = region
            .windows
            .iter()
            .flat_map(|w| {
                w.spans
                    .iter()
                    .map(|(&(column, floor_y), &depth_q)| WireSpan {
                        column,
                        floor_y,
                        depth_q,
                    })
            })
            .collect();
        out.sort_by_key(|s| (s.column, s.floor_y));
        out
    }

    /// Wet spans of one window of a region, column order.
    pub fn window_spans(&self, key: (i32, i32), window: u8) -> Vec<WireSpan> {
        let Some(region) = self.regions.get(&key) else {
            return Vec::new();
        };
        region
            .windows
            .get(usize::from(window))
            .map(|w| {
                w.spans
                    .iter()
                    .map(|(&(column, floor_y), &depth_q)| WireSpan {
                        column,
                        floor_y,
                        depth_q,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn region_keys(&self) -> Vec<(i32, i32)> {
        self.regions.keys().copied().collect()
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
        self.frame_seq = 0;
    }
}

fn region_is_blank(region: &RegionState) -> bool {
    region
        .windows
        .iter()
        .all(|w| w.seq.is_none() && w.spans.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(column: u16, floor_y: i32, depth_q: u16) -> WireSpan {
        WireSpan {
            column,
            floor_y,
            depth_q,
        }
    }

    #[test]
    fn baseline_then_delta_round_trips_through_the_record() {
        let mut replica = ColumnWaterReplica::default();
        let base = encode_update(
            Tick(1),
            10,
            0,
            (2, -1),
            1,
            &[span(130, 5, 640), span(200, 5, 64)],
        )
        .unwrap();
        base.validate().unwrap();
        let applied = replica.apply(&base).unwrap();
        assert!(applied.baseline);
        assert_eq!(replica.region_spans((2, -1)).len(), 2);

        let delta = encode_update(
            Tick(2),
            11,
            10,
            (2, -1),
            1,
            &[span(130, 5, 700), span(200, 5, 0)],
        )
        .unwrap();
        replica.apply(&delta).unwrap();
        assert_eq!(replica.region_spans((2, -1)), vec![span(130, 5, 700)]);
        assert_eq!(
            (replica.baselines, replica.deltas, replica.frame_seq),
            (1, 1, 11)
        );
    }

    #[test]
    fn a_delta_on_the_wrong_base_is_refused_and_changes_nothing() {
        let mut replica = ColumnWaterReplica::default();
        let delta = encode_update(Tick(2), 11, 10, (0, 0), 0, &[span(1, 0, 5)]).unwrap();
        assert!(matches!(
            replica.apply(&delta),
            Err(ColumnWaterWireError::BaseMismatch { have: None, .. })
        ));
        assert_eq!(replica.region_count(), 0);
        let base = encode_update(Tick(1), 20, 0, (0, 0), 0, &[span(1, 0, 5)]).unwrap();
        replica.apply(&base).unwrap();
        assert!(replica.apply(&delta).is_err(), "stale base");
        assert_eq!(replica.region_spans((0, 0)), vec![span(1, 0, 5)]);
    }

    #[test]
    fn columns_must_belong_to_the_window_and_limits_hold() {
        assert!(matches!(
            encode_update(Tick(0), 1, 0, (0, 0), 0, &[span(128, 0, 1)]),
            Err(ColumnWaterWireError::ColumnOutsideWindow(128))
        ));
        let many: Vec<_> = (0..=MAX_UPDATE_SPANS)
            .map(|i| span(0, i as i32, 1))
            .collect();
        assert!(matches!(
            encode_update(Tick(0), 1, 0, (0, 0), 0, &many),
            Err(ColumnWaterWireError::TooManySpans(_))
        ));
        let mut bad = encode_update(Tick(0), 1, 0, (0, 0), 0, &[span(0, 0, 1)]).unwrap();
        bad.window = 9;
        assert!(bad.validate().is_err());
        let mut bomb = bad.clone();
        bomb.window = 0;
        bomb.payload = zstd::stream::encode_all(&vec![0u8; 4 * 1024 * 1024][..], 1).unwrap();
        assert!(decode_spans(&bomb).is_err(), "decompression is bounded");
    }

    #[test]
    fn an_emptied_region_is_dropped_once_every_window_is_known() {
        let mut replica = ColumnWaterReplica::default();
        for w in 0..WINDOWS_PER_REGION {
            let spans = if w == 0 {
                vec![span(0, 0, 9)]
            } else {
                Vec::new()
            };
            replica
                .apply(&encode_update(Tick(0), 1, 0, (0, 0), w, &spans).unwrap())
                .unwrap();
        }
        assert_eq!(replica.region_count(), 1);
        replica
            .apply(&encode_update(Tick(0), 2, 1, (0, 0), 0, &[span(0, 0, 0)]).unwrap())
            .unwrap();
        assert_eq!(replica.region_count(), 0);
    }
}
