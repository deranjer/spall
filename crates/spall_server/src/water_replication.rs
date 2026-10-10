//! Per-session water presentation scheduler. Full frames repair delta gaps.
use spall_net::WireRecord;
use spall_protocol::{WaterDelta, WaterKeyframe, encode_water_keyframe, water_deltas};
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

pub const WATER_BYTES_PER_SECOND: usize = 128 * 1024;
pub const WATER_BURST_BYTES: usize = 128 * 1024;
const KEYFRAME_TICKS: u64 = 60;

/// One water frame shared by every session's publisher. Its full-keyframe encoding (a whole
/// frame compressed into records) is identical for every client, so it is made once per frame
/// instead of once per client per frame, and the frame itself is shared rather than cloned.
pub struct SharedFrame {
    frame: Arc<WaterKeyframe>,
    full: OnceCell<Result<Vec<WireRecord>, String>>,
    /// Deltas from an earlier frame to this one, by that frame's sequence number. Clients that
    /// are all one frame behind share a single computation.
    deltas: RefCell<HashMap<u64, Option<Vec<WaterDelta>>>>,
}

impl SharedFrame {
    pub fn new(frame: Arc<WaterKeyframe>) -> Self {
        Self {
            frame,
            full: OnceCell::new(),
            deltas: RefCell::new(HashMap::new()),
        }
    }

    fn deltas_from(&self, base: &WaterKeyframe) -> Option<Vec<WaterDelta>> {
        self.deltas
            .borrow_mut()
            .entry(base.frame_seq)
            .or_insert_with(|| water_deltas(base, &self.frame))
            .clone()
    }

    pub fn frame(&self) -> &Arc<WaterKeyframe> {
        &self.frame
    }

    fn full_records(&self) -> Result<VecDeque<WireRecord>, String> {
        self.full
            .get_or_init(|| {
                let frame = &self.frame;
                encode_water_keyframe(
                    frame.server_tick,
                    frame.frame_seq,
                    frame.origin,
                    frame.dimensions,
                    u32::from(frame.coarsen),
                    &frame.fractions,
                )
                .map(|v| v.into_iter().map(WireRecord::WaterSnapshot).collect())
                .map_err(|e| e.to_string())
            })
            .clone()
            .map(VecDeque::from)
    }
}

pub struct WaterPublisher {
    base: Option<Arc<WaterKeyframe>>,
    pending: VecDeque<WireRecord>,
    target: Option<Arc<WaterKeyframe>>,
    last_keyframe_tick: u64,
    credit: f64,
    last_tick: u64,
    pub bytes_queued: u64,
}

impl Default for WaterPublisher {
    fn default() -> Self {
        Self {
            base: None,
            pending: VecDeque::new(),
            target: None,
            last_keyframe_tick: 0,
            credit: WATER_BURST_BYTES as f64,
            last_tick: 0,
            bytes_queued: 0,
        }
    }
}

impl WaterPublisher {
    #[cfg(test)]
    pub fn poll(&mut self, tick: u64, frame: &WaterKeyframe) -> Result<Vec<WireRecord>, String> {
        self.poll_shared(tick, &SharedFrame::new(Arc::new(frame.clone())))
    }

    pub fn poll_shared(
        &mut self,
        tick: u64,
        shared: &SharedFrame,
    ) -> Result<Vec<WireRecord>, String> {
        let frame = &**shared.frame();
        self.credit = (self.credit
            + tick.saturating_sub(self.last_tick) as f64 * WATER_BYTES_PER_SECOND as f64 / 60.0)
            .min(WATER_BURST_BYTES as f64);
        self.last_tick = tick;
        if self.pending.is_empty() && tick.is_multiple_of(4) {
            let full = self.base.is_none()
                || tick.saturating_sub(self.last_keyframe_tick) >= KEYFRAME_TICKS;
            if full
                || self
                    .base
                    .as_ref()
                    .is_some_and(|b| b.frame_seq != frame.frame_seq)
            {
                let delta_span = spall_sim::prof::Span::start("water.deltas");
                let deltas = if full {
                    None
                } else {
                    self.base.as_ref().and_then(|b| shared.deltas_from(b))
                };
                drop(delta_span);
                let choose_span = spall_sim::prof::Span::start("water.choose_encoding");
                let full_records = || shared.full_records();
                self.pending = if let Some(deltas) = deltas {
                    let delta_records: VecDeque<_> =
                        deltas.into_iter().map(WireRecord::WaterDelta).collect();
                    let full_records = full_records()?;
                    let bytes = |records: &VecDeque<WireRecord>| {
                        records
                            .iter()
                            .map(|r| {
                                r.encode_framed()
                                    .map(|b| b.len())
                                    .unwrap_or(usize::MAX / 4096)
                            })
                            .sum::<usize>()
                    };
                    if bytes(&delta_records) < bytes(&full_records) {
                        delta_records
                    } else {
                        self.last_keyframe_tick = tick;
                        full_records
                    }
                } else {
                    self.last_keyframe_tick = tick;
                    full_records()?
                };
                drop(choose_span);
                self.target = Some(Arc::clone(shared.frame()));
            }
        }
        let batch_span = spall_sim::prof::Span::start("water.batch");
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
        if self.pending.is_empty()
            && let Some(target) = self.target.take()
        {
            self.base = Some(target);
        }
        drop(batch_span);
        Ok(batch)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{GlobalCell, Tick};
    use spall_protocol::{WaterAssembler, WaterDeltaAssembler};
    #[test]
    fn budget_bounds_sustained_egress_and_sparse_edits_use_deltas() {
        let mut seed = 7u64;
        let bytes = (0..128 * 8 * 128)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 56) as u8
            })
            .collect();
        let mut frame = WaterKeyframe {
            server_tick: Tick(0),
            frame_seq: 1,
            origin: GlobalCell::new(0, 0, 0),
            dimensions: [128, 8, 128],
            coarsen: 1,
            fractions: bytes,
        };
        let mut publisher = WaterPublisher::default();
        let mut full = WaterAssembler::default();
        let mut deltas = WaterDeltaAssembler::default();
        let mut received = None;
        let mut delta_count = 0;
        for tick in 0..600 {
            if tick == 30 {
                frame.frame_seq = 2;
                frame.server_tick = Tick(tick);
                frame.fractions[0] ^= 0xff;
            }
            for record in publisher.poll(tick, &frame).unwrap() {
                match record {
                    WireRecord::WaterSnapshot(chunk) => {
                        if let Some(f) = full.push(chunk).unwrap() {
                            deltas.install(f.clone());
                            received = Some(f);
                        }
                    }
                    WireRecord::WaterDelta(delta) => {
                        delta_count += 1;
                        if let Some(f) = deltas.push(delta).unwrap() {
                            received = Some(f);
                        }
                    }
                    _ => panic!("water records only"),
                }
            }
            assert!(
                publisher.bytes_queued
                    <= WATER_BURST_BYTES as u64 + tick * WATER_BYTES_PER_SECOND as u64 / 60
            );
        }
        assert!(delta_count > 0);
        assert_eq!(received, Some(frame));
        eprintln!(
            "water byte-budget: {} bytes over 10 s, burst {}, rate {} B/s, delta records {}",
            publisher.bytes_queued, WATER_BURST_BYTES, WATER_BYTES_PER_SECOND, delta_count
        );
    }
}
