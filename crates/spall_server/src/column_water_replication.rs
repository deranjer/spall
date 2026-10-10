//! Per-session presentation scheduler for the stacked-column water (ENG-167).
//!
//! Each session remembers the region frame it was last sent. Whenever the
//! published frame holds a different region, the publisher diffs it window by
//! window and queues a baseline or a quantized-depth delta, whichever encodes
//! smaller. Records go out in order inside a byte budget, so a slow client
//! sees the newest water later rather than every frame late, and a late joiner
//! starts from baselines of every wet region.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use spall_core::Tick;
use spall_net::WireRecord;
use spall_protocol::column_water_wire::{WINDOWS_PER_REGION, WireSpan, encode_update, window_of};
use spall_sim::column_water::{ColumnWaterFrame, RegionFrame};

use crate::water_replication::{WATER_BURST_BYTES, WATER_BYTES_PER_SECOND};

type RegionKey = (i32, i32);

struct SentRegion {
    frame: Arc<RegionFrame>,
    /// Frame sequence each window was last brought to; 0 means never sent.
    window_seq: [u64; WINDOWS_PER_REGION as usize],
}

pub struct ColumnWaterPublisher {
    sent: HashMap<RegionKey, SentRegion>,
    pending: VecDeque<WireRecord>,
    credit: f64,
    last_tick: u64,
    pub bytes_queued: u64,
}

impl Default for ColumnWaterPublisher {
    fn default() -> Self {
        Self {
            sent: HashMap::new(),
            pending: VecDeque::new(),
            credit: WATER_BURST_BYTES as f64,
            last_tick: 0,
            bytes_queued: 0,
        }
    }
}

fn window_spans(frame: &RegionFrame, window: u8) -> Vec<WireSpan> {
    frame
        .spans
        .iter()
        .filter(|s| window_of(s.column) == window && s.depth_q > 0)
        .map(|s| WireSpan {
            column: s.column,
            floor_y: s.floor_y,
            depth_q: s.depth_q,
        })
        .collect()
}

/// Spans of `new` that differ from `old`, with removals as depth 0.
fn delta_spans(old: &[WireSpan], new: &[WireSpan]) -> Vec<WireSpan> {
    let old_map: HashMap<(u16, i32), u16> = old
        .iter()
        .map(|s| ((s.column, s.floor_y), s.depth_q))
        .collect();
    let new_map: HashMap<(u16, i32), u16> = new
        .iter()
        .map(|s| ((s.column, s.floor_y), s.depth_q))
        .collect();
    let mut out: Vec<WireSpan> = new
        .iter()
        .filter(|s| old_map.get(&(s.column, s.floor_y)) != Some(&s.depth_q))
        .copied()
        .collect();
    out.extend(
        old.iter()
            .filter(|s| !new_map.contains_key(&(s.column, s.floor_y)))
            .map(|s| WireSpan { depth_q: 0, ..*s }),
    );
    out.sort_by_key(|s| (s.column, s.floor_y));
    out
}

impl ColumnWaterPublisher {
    /// Queues what changed since the last queued state, then returns the
    /// records the byte budget allows this tick.
    pub fn poll(
        &mut self,
        tick: u64,
        server_tick: Tick,
        frame: &ColumnWaterFrame,
    ) -> Result<Vec<WireRecord>, String> {
        self.credit = (self.credit
            + tick.saturating_sub(self.last_tick) as f64 * WATER_BYTES_PER_SECOND as f64 / 60.0)
            .min(WATER_BURST_BYTES as f64);
        self.last_tick = tick;
        if self.pending.is_empty() && tick.is_multiple_of(4) {
            self.queue_changes(server_tick, frame)?;
        }
        let mut batch = Vec::new();
        while let Some(record) = self.pending.front() {
            let size = record.encode_framed().map_err(|e| e.to_string())?.len() + 32;
            if self.credit < size as f64 {
                break;
            }
            self.credit -= size as f64;
            self.bytes_queued += size as u64;
            batch.push(self.pending.pop_front().expect("front present"));
        }
        Ok(batch)
    }

    fn queue_changes(&mut self, server_tick: Tick, frame: &ColumnWaterFrame) -> Result<(), String> {
        let seq = frame.seq;
        for (&key, new) in frame.regions.iter() {
            let state = self.sent.get(&key);
            if state.is_some_and(|s| Arc::ptr_eq(&s.frame, new) || *s.frame == **new) {
                if let Some(s) = self.sent.get_mut(&key) {
                    s.frame = Arc::clone(new);
                }
                continue;
            }
            let mut window_seq = state.map_or([0; WINDOWS_PER_REGION as usize], |s| s.window_seq);
            for window in 0..WINDOWS_PER_REGION {
                let new_spans = window_spans(new, window);
                let old_spans = state.map_or_else(Vec::new, |s| window_spans(&s.frame, window));
                if new_spans == old_spans && window_seq[usize::from(window)] != 0 {
                    continue;
                }
                if new_spans.is_empty() && old_spans.is_empty() && state.is_none() {
                    // Nothing wet here and nothing ever sent: the client
                    // starts a window empty. Still mark it known so a later
                    // delta has a base.
                }
                let base = window_seq[usize::from(window)];
                let baseline = encode_update(server_tick, seq, 0, key, window, &new_spans)
                    .map_err(|e| e.to_string())?;
                let record = if base == 0 {
                    baseline
                } else {
                    let delta = encode_update(
                        server_tick,
                        seq,
                        base,
                        key,
                        window,
                        &delta_spans(&old_spans, &new_spans),
                    )
                    .map_err(|e| e.to_string())?;
                    if delta.payload.len() < baseline.payload.len() {
                        delta
                    } else {
                        baseline
                    }
                };
                window_seq[usize::from(window)] = seq;
                self.pending
                    .push_back(WireRecord::ColumnWaterUpdate(record));
            }
            self.sent.insert(
                key,
                SentRegion {
                    frame: Arc::clone(new),
                    window_seq,
                },
            );
        }
        // Regions that dried up: empty baselines for every window that held water.
        let gone: Vec<RegionKey> = self
            .sent
            .keys()
            .filter(|k| !frame.regions.contains_key(k))
            .copied()
            .collect();
        for key in gone {
            let state = self.sent.remove(&key).expect("key came from the map");
            for window in 0..WINDOWS_PER_REGION {
                if window_spans(&state.frame, window).is_empty() {
                    continue;
                }
                let record = encode_update(server_tick, seq, 0, key, window, &[])
                    .map_err(|e| e.to_string())?;
                self.pending
                    .push_back(WireRecord::ColumnWaterUpdate(record));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::ColumnWaterReplica;
    use spall_sim::column_water::FrameSpan;
    use std::collections::BTreeMap;

    fn region(rx: i32, spans: &[(u16, i32, u16)]) -> Arc<RegionFrame> {
        Arc::new(RegionFrame {
            key: (rx, 0),
            spans: spans
                .iter()
                .map(|&(column, floor_y, depth_q)| FrameSpan {
                    column,
                    floor_y,
                    depth_q,
                })
                .collect(),
        })
    }

    fn frame(seq: u64, regions: Vec<Arc<RegionFrame>>) -> ColumnWaterFrame {
        ColumnWaterFrame {
            seq,
            fluid_time_s: 0.0,
            volume_m3: 0.0,
            spring_added_m3: 0.0,
            drain_removed_m3: 0.0,
            regions: Arc::new(
                regions
                    .into_iter()
                    .map(|r| (r.key, r))
                    .collect::<BTreeMap<_, _>>(),
            ),
            changed: Vec::new(),
        }
    }

    fn deliver(
        publisher: &mut ColumnWaterPublisher,
        replica: &mut ColumnWaterReplica,
        tick: u64,
        frame: &ColumnWaterFrame,
    ) -> usize {
        let mut n = 0;
        for record in publisher.poll(tick, Tick(tick), frame).unwrap() {
            let WireRecord::ColumnWaterUpdate(update) = record else {
                panic!("water records only")
            };
            replica.apply(&update).unwrap();
            n += 1;
        }
        n
    }

    #[test]
    fn a_session_converges_through_baselines_deltas_and_drying() {
        let mut publisher = ColumnWaterPublisher::default();
        let mut replica = ColumnWaterReplica::default();
        let a = region(0, &[(3, 5, 640), (200, 5, 64), (900, 2, 128)]);
        let f1 = frame(1, vec![a.clone(), region(1, &[(0, 0, 10)])]);
        let sent = deliver(&mut publisher, &mut replica, 0, &f1);
        assert!(sent >= 2);
        assert_eq!(replica.region_spans((0, 0)).len(), 3);

        // One depth changes, one span dries, one appears: deltas only.
        let a2 = region(0, &[(3, 5, 700), (900, 2, 128), (901, 2, 5)]);
        let f2 = frame(2, vec![a2, region(1, &[(0, 0, 10)])]);
        deliver(&mut publisher, &mut replica, 4, &f2);
        let spans = replica.region_spans((0, 0));
        assert_eq!(
            spans
                .iter()
                .map(|s| (s.column, s.depth_q))
                .collect::<Vec<_>>(),
            vec![(3, 700), (900, 128), (901, 5)]
        );
        assert!(replica.deltas > 0, "an in-place change goes out as a delta");

        // Region 1 dries up entirely.
        let f3 = frame(3, vec![f2.regions[&(0, 0)].clone()]);
        deliver(&mut publisher, &mut replica, 8, &f3);
        assert!(replica.region_spans((1, 0)).is_empty());

        // An unchanged frame costs nothing.
        assert_eq!(deliver(&mut publisher, &mut replica, 12, &f3), 0);
    }

    /// A span that quantizes to depth zero is dry on the wire. A region of only
    /// such films must not strand the client without a base for later deltas.
    #[test]
    fn zero_depth_films_never_break_the_delta_chain() {
        let mut publisher = ColumnWaterPublisher::default();
        let mut replica = ColumnWaterReplica::default();
        deliver(
            &mut publisher,
            &mut replica,
            0,
            &frame(1, vec![region(0, &[(5, 1, 0)])]),
        );
        assert!(replica.region_spans((0, 0)).is_empty());
        // Water arrives, then a film again, then real water: every step applies.
        deliver(
            &mut publisher,
            &mut replica,
            4,
            &frame(2, vec![region(0, &[(5, 1, 300)])]),
        );
        deliver(
            &mut publisher,
            &mut replica,
            8,
            &frame(3, vec![region(0, &[(5, 1, 0), (6, 1, 2)])]),
        );
        deliver(
            &mut publisher,
            &mut replica,
            12,
            &frame(4, vec![region(0, &[(5, 1, 40), (6, 1, 2)])]),
        );
        let spans = replica.region_spans((0, 0));
        assert_eq!(
            spans
                .iter()
                .map(|s| (s.column, s.depth_q))
                .collect::<Vec<_>>(),
            vec![(5, 40), (6, 2)]
        );
    }

    #[test]
    fn a_late_joiner_starts_from_baselines_and_the_budget_bounds_egress() {
        let mut regions = Vec::new();
        for rx in 0..40 {
            let spans: Vec<(u16, i32, u16)> = (0..1000u16)
                .map(|c| {
                    (
                        c,
                        3,
                        ((u32::from(c).wrapping_mul(2654435761u32) >> 16) as u16).max(1),
                    )
                })
                .collect();
            regions.push(region(rx, &spans));
        }
        let f = frame(5, regions);
        let mut publisher = ColumnWaterPublisher::default();
        let mut replica = ColumnWaterReplica::default();
        for tick in 0..600u64 {
            deliver(&mut publisher, &mut replica, tick, &f);
            assert!(
                publisher.bytes_queued
                    <= WATER_BURST_BYTES as u64 + tick * WATER_BYTES_PER_SECOND as u64 / 60,
                "tick {tick}: {} bytes over budget",
                publisher.bytes_queued
            );
        }
        // Random depths do not compress, so the budget is the limit: some but
        // not all regions arrived in 10 s, and what arrived is exact.
        assert!(replica.region_count() > 0);
        for key in replica.region_keys() {
            assert_eq!(replica.region_spans(key).len(), 1000);
        }
    }
}
