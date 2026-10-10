//! Changed-brick water presentation updates with candidate-first publication.
use crate::{Record, RecordError, WaterKeyframe, WireTag};
use serde::{Deserialize, Serialize};
use spall_core::Tick;

pub const WATER_BRICK_EDGE: u32 = 16;
pub const MAX_WATER_DELTA_BRICKS: u32 = 4096;

/// Compression level for a changed brick's fractions. Deltas are computed on the server's tick
/// thread, where level 3 cost several milliseconds on ticks with much moving water; level 1 is
/// several times faster and any level decodes the same.
const WATER_DELTA_ZSTD_LEVEL: i32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaterDelta {
    pub origin: spall_core::GlobalCell,
    pub server_tick: Tick,
    pub base_seq: u64,
    pub frame_seq: u64,
    pub index: u32,
    pub count: u32,
    /// Brick coordinate relative to the installed fluid domain, X/Y/Z.
    pub brick: [u32; 3],
    /// Zstd-compressed quantized fractions, X fastest, edge clipped.
    pub fractions: Vec<u8>,
}

impl Record for WaterDelta {
    const TAG: WireTag = WireTag::WaterDelta;
    const SCHEMA_VERSION: u16 = 1;
    fn validate(&self) -> Result<(), RecordError> {
        if self.base_seq == 0
            || self.frame_seq <= self.base_seq
            || self.count == 0
            || self.count > MAX_WATER_DELTA_BRICKS
            || self.index >= self.count
            || self.fractions.is_empty()
            || self.fractions.len() > 4160
            || self
                .brick
                .iter()
                .any(|v| *v >= crate::water::MAX_WATER_CELLS as u32)
        {
            return Err(RecordError::Inconsistent("invalid water delta"));
        }
        Ok(())
    }
}

fn brick_shape(frame: &WaterKeyframe, coord: [u32; 3]) -> Option<([usize; 3], [usize; 3])> {
    let mut start = [0; 3];
    let mut size = [0; 3];
    for i in 0..3 {
        let p = coord[i].checked_mul(WATER_BRICK_EDGE)?;
        if p >= frame.dimensions[i] {
            return None;
        }
        start[i] = p as usize;
        size[i] = (frame.dimensions[i] - p).min(WATER_BRICK_EDGE) as usize;
    }
    Some((start, size))
}

fn brick_bytes(frame: &WaterKeyframe, coord: [u32; 3]) -> Vec<u8> {
    let (start, size) = brick_shape(frame, coord).expect("enumerated domain brick");
    let [nx, ny, _] = frame.dimensions.map(|v| v as usize);
    let mut bytes = Vec::with_capacity(size.iter().product());
    for z in start[2]..start[2] + size[2] {
        for y in start[1]..start[1] + size[1] {
            let offset = start[0] + nx * (y + ny * z);
            bytes.extend_from_slice(&frame.fractions[offset..offset + size[0]]);
        }
    }
    bytes
}

/// Whether one brick holds the same bytes in both frames, compared row by row in place: no
/// allocation, which matters because this runs for every brick of the domain on the tick thread.
/// Both frames share dimensions (checked by the caller).
fn brick_equal(before: &WaterKeyframe, after: &WaterKeyframe, coord: [u32; 3]) -> bool {
    let (start, size) = brick_shape(after, coord).expect("enumerated domain brick");
    let [nx, ny, _] = after.dimensions.map(|v| v as usize);
    (start[2]..start[2] + size[2]).all(|z| {
        (start[1]..start[1] + size[1]).all(|y| {
            let offset = start[0] + nx * (y + ny * z);
            before.fractions[offset..offset + size[0]] == after.fractions[offset..offset + size[0]]
        })
    })
}

pub fn water_deltas(before: &WaterKeyframe, after: &WaterKeyframe) -> Option<Vec<WaterDelta>> {
    for frame in [before, after] {
        let n = frame
            .dimensions
            .iter()
            .try_fold(1usize, |a, b| a.checked_mul(*b as usize))?;
        if n == 0 || n > crate::water::MAX_WATER_CELLS || frame.fractions.len() != n {
            return None;
        }
    }
    if before.origin != after.origin
        || before.dimensions != after.dimensions
        || before.coarsen != after.coarsen
        || before.frame_seq >= after.frame_seq
    {
        return None;
    }
    let d = after.dimensions.map(|v| v.div_ceil(WATER_BRICK_EDGE));
    let n = d.iter().try_fold(1u32, |a, b| a.checked_mul(*b))?;
    if n > MAX_WATER_DELTA_BRICKS {
        return None;
    }
    let mut result = Vec::new();
    for z in 0..d[2] {
        for y in 0..d[1] {
            for x in 0..d[0] {
                let brick = [x, y, z];
                if !brick_equal(before, after, brick) {
                    let fractions = brick_bytes(after, brick);
                    result.push(WaterDelta {
                        origin: after.origin,
                        server_tick: after.server_tick,
                        base_seq: before.frame_seq,
                        frame_seq: after.frame_seq,
                        index: result.len() as u32,
                        count: 0,
                        brick,
                        fractions: zstd::stream::encode_all(
                            fractions.as_slice(),
                            WATER_DELTA_ZSTD_LEVEL,
                        )
                        .ok()?,
                    });
                }
            }
        }
    }
    if result.is_empty() {
        result.push(WaterDelta {
            origin: after.origin,
            server_tick: after.server_tick,
            base_seq: before.frame_seq,
            frame_seq: after.frame_seq,
            index: 0,
            count: 1,
            brick: [0; 3],
            fractions: zstd::stream::encode_all(
                brick_bytes(after, [0; 3]).as_slice(),
                WATER_DELTA_ZSTD_LEVEL,
            )
            .ok()?,
        });
    }
    let count = result.len() as u32;
    for r in &mut result {
        r.count = count;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::GlobalCell;
    fn frame(seq: u64) -> WaterKeyframe {
        WaterKeyframe {
            server_tick: Tick(seq),
            frame_seq: seq,
            origin: GlobalCell::new(-16, 0, 0),
            dimensions: [35, 3, 17],
            coarsen: 1,
            fractions: vec![0; 35 * 3 * 17],
        }
    }
    #[test]
    fn changed_bricks_publish_atomically_and_clip_edges() {
        let before = frame(1);
        let mut after = frame(2);
        after.fractions[0] = 100;
        *after.fractions.last_mut().unwrap() = 230;
        let deltas = water_deltas(&before, &after).unwrap();
        assert_eq!(deltas.len(), 2);
        let mut assembler = WaterDeltaAssembler::default();
        assembler.install(before.clone());
        let first = crate::decode_control(&crate::encode_control(&deltas[0]).unwrap()).unwrap();
        assert!(assembler.push(first).unwrap().is_none());
        assert_eq!(assembler.current, Some(before));
        assert_eq!(assembler.push(deltas[1].clone()).unwrap(), Some(after));
    }
    /// The in-place comparison picks exactly the bricks a byte-for-byte comparison of the
    /// extracted bricks would, on a domain whose edge bricks are clipped.
    #[test]
    fn the_in_place_comparison_agrees_with_comparing_extracted_bricks() {
        let before = frame(1);
        let mut after = frame(2);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..40 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = (state % after.fractions.len() as u64) as usize;
            after.fractions[index] = (state >> 32) as u8 | 1;
        }
        let d = after.dimensions.map(|v| v.div_ceil(WATER_BRICK_EDGE));
        let mut expected = Vec::new();
        for z in 0..d[2] {
            for y in 0..d[1] {
                for x in 0..d[0] {
                    let brick = [x, y, z];
                    assert_eq!(
                        brick_equal(&before, &after, brick),
                        brick_bytes(&before, brick) == brick_bytes(&after, brick),
                        "brick {brick:?}"
                    );
                    if brick_bytes(&before, brick) != brick_bytes(&after, brick) {
                        expected.push(brick);
                    }
                }
            }
        }
        let published: Vec<[u32; 3]> = water_deltas(&before, &after)
            .unwrap()
            .iter()
            .map(|delta| delta.brick)
            .collect();
        assert_eq!(published, expected);
    }
    #[test]
    fn gap_reorder_duplicate_bomb_and_repair_preserve_the_installed_frame() {
        let before = frame(1);
        let mut after = frame(2);
        after.fractions[0] = 100;
        *after.fractions.last_mut().unwrap() = 230;
        let deltas = water_deltas(&before, &after).unwrap();
        let mut assembler = WaterDeltaAssembler::default();
        assembler.install(before.clone());
        assert!(assembler.push(deltas[1].clone()).unwrap().is_none());
        assembler.push(deltas[0].clone()).unwrap();
        let mut duplicate = deltas[1].clone();
        duplicate.brick = deltas[0].brick;
        assert!(assembler.push(duplicate).is_err());
        assert_eq!(assembler.current, Some(before.clone()));
        let mut bomb = deltas[0].clone();
        bomb.fractions = zstd::stream::encode_all(&vec![0; 100000][..], 3).unwrap();
        assert!(assembler.push(bomb).is_err());
        assert_eq!(assembler.current, Some(before));
        assembler.install(after.clone());
        assert!(assembler.push(deltas[0].clone()).unwrap().is_none());
        assert_eq!(assembler.current, Some(after));
    }
}

#[derive(Default)]
pub struct WaterDeltaAssembler {
    current: Option<WaterKeyframe>,
    partial: Option<(WaterKeyframe, WaterDelta)>,
}

impl WaterDeltaAssembler {
    /// The last installed full frame, if any.
    pub fn current(&self) -> Option<&WaterKeyframe> {
        self.current.as_ref()
    }

    pub fn install(&mut self, frame: WaterKeyframe) {
        self.current = Some(frame);
        self.partial = None;
    }
    pub fn push(&mut self, delta: WaterDelta) -> Result<Option<WaterKeyframe>, RecordError> {
        if let Err(e) = delta.validate() {
            self.partial = None;
            return Err(e);
        }
        let Some(current) = &self.current else {
            return Ok(None);
        };
        if current.frame_seq != delta.base_seq || current.origin != delta.origin {
            self.partial = None;
            return Ok(None);
        }
        if delta.index == 0 {
            self.partial = Some((current.clone(), delta.clone()));
        }
        let Some((candidate, previous)) = &mut self.partial else {
            return Ok(None);
        };
        let expected = if delta.index == 0 {
            0
        } else {
            previous.index + 1
        };
        let compatible = delta.frame_seq == previous.frame_seq
            && delta.count == previous.count
            && delta.server_tick == previous.server_tick
            && delta.base_seq == previous.base_seq
            && delta.index == expected
            && (delta.index == 0
                || (delta.brick[2], delta.brick[1], delta.brick[0])
                    > (previous.brick[2], previous.brick[1], previous.brick[0]));
        let Some((start, size)) = brick_shape(candidate, delta.brick) else {
            self.partial = None;
            return Err(RecordError::Inconsistent("water delta brick bounds"));
        };
        if !compatible {
            self.partial = None;
            return Err(RecordError::Inconsistent("water delta sequence"));
        }
        use std::io::Read;
        let expected: usize = size.iter().product();
        let decoded = zstd::stream::Decoder::new(delta.fractions.as_slice()).and_then(|d| {
            let mut bytes = Vec::with_capacity(expected);
            d.take(expected as u64 + 1).read_to_end(&mut bytes)?;
            Ok(bytes)
        });
        let bytes = match decoded {
            Ok(bytes) if bytes.len() == expected => bytes,
            _ => {
                self.partial = None;
                return Err(RecordError::Inconsistent("water delta compressed payload"));
            }
        };
        let [nx, ny, _] = candidate.dimensions.map(|v| v as usize);
        let mut cursor = 0;
        for z in start[2]..start[2] + size[2] {
            for y in start[1]..start[1] + size[1] {
                let offset = start[0] + nx * (y + ny * z);
                candidate.fractions[offset..offset + size[0]]
                    .copy_from_slice(&bytes[cursor..cursor + size[0]]);
                cursor += size[0];
            }
        }
        *previous = delta.clone();
        if delta.index + 1 == delta.count {
            let (mut frame, _) = self.partial.take().expect("candidate present");
            frame.frame_seq = delta.frame_seq;
            frame.server_tick = delta.server_tick;
            self.current = Some(frame.clone());
            return Ok(Some(frame));
        }
        Ok(None)
    }
}
