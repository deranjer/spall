//! Adapts replicated column water to the dense per-region frames the current
//! water look consumes (ENG-167). Presentation only; the proper span-surface
//! mesh arrives with the rendering work item (W4), which makes this adapter
//! disappear.

use spall_core::{GlobalCell, Tick};
use spall_protocol::WaterKeyframe;
use spall_protocol::column_water_wire::WireSpan;

/// Depth units per cell on the wire.
const DEPTH_SCALE: f64 = 128.0;
const REGION_EDGE: usize = 32;

/// One region's wet spans as a tight dense frame (a 32 x height x 32 box
/// starting at the lowest wet floor), or `None` if the region holds no water.
pub fn region_keyframe(
    key: (i32, i32),
    spans: &[WireSpan],
    server_tick: Tick,
    frame_seq: u64,
) -> Option<WaterKeyframe> {
    let top_of = |s: &WireSpan| s.floor_y + (f64::from(s.depth_q) / DEPTH_SCALE).ceil() as i32;
    let y_min = spans.iter().map(|s| s.floor_y).min()?;
    let y_max = spans.iter().map(top_of).max()?;
    let ny = (y_max - y_min).max(1) as usize;
    let mut fractions = vec![0u8; REGION_EDGE * ny * REGION_EDGE];
    for s in spans {
        let (lx, lz) = (
            usize::from(s.column) % REGION_EDGE,
            usize::from(s.column) / REGION_EDGE,
        );
        let depth = f64::from(s.depth_q) / DEPTH_SCALE;
        let full = depth.floor() as usize;
        let base = (s.floor_y - y_min) as usize;
        let at = |y: usize| lx + REGION_EDGE * (y + ny * lz);
        for y in 0..full {
            fractions[at(base + y)] = 255;
        }
        let part = depth - full as f64;
        if part > 0.0 && base + full < ny {
            let cell = &mut fractions[at(base + full)];
            *cell = (*cell).max((part * 255.0).round() as u8);
        }
    }
    Some(WaterKeyframe {
        server_tick,
        frame_seq,
        origin: GlobalCell::new(
            i64::from(key.0) * REGION_EDGE as i64,
            i64::from(y_min),
            i64::from(key.1) * REGION_EDGE as i64,
        ),
        dimensions: [REGION_EDGE as u32, ny as u32, REGION_EDGE as u32],
        coarsen: 1,
        fractions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_become_a_tight_dense_box() {
        let spans = [
            WireSpan {
                column: 33, // lx 1, lz 1
                floor_y: 10,
                depth_q: 128 * 2 + 64, // 2.5 cells
            },
            WireSpan {
                column: 0,
                floor_y: 11,
                depth_q: 128,
            },
        ];
        let f = region_keyframe((2, -1), &spans, Tick(7), 9).unwrap();
        assert_eq!(f.origin, GlobalCell::new(64, 10, -32));
        assert_eq!(f.dimensions, [32, 3, 32]);
        let at = |x: usize, y: usize, z: usize| f.fractions[x + 32 * (y + 3 * z)];
        assert_eq!((at(1, 0, 1), at(1, 1, 1)), (255, 255));
        assert_eq!(at(1, 2, 1), 128, "half-full top cell");
        assert_eq!((at(0, 0, 0), at(0, 1, 0), at(0, 2, 0)), (0, 255, 0));
        assert!(region_keyframe((0, 0), &[], Tick(0), 1).is_none());
    }
}
