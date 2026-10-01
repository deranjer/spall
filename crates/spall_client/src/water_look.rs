//! How replicated water is *drawn*: a smoothed surface sheet plus the
//! surface-height field the renderer uses for underwater fog and caustics.
//!
//! Presentation only. The water simulation is server authoritative and
//! replicated as keyframes; nothing here feeds back into it.

use spall_protocol::WaterKeyframe;
use spall_render::GpuVertex;
use spall_render::water::{WaterColumn, WaterField, build_water_surface};

use crate::predict::CELL_M;

/// Water under this fraction of a cell (of 255) is not drawn (thin films and
/// spray).
pub const WATER_DRAW_MIN: u8 = 6;

/// Resolution of the surface-height field: one quarter-metre voxel.
const FIELD_CELL_M: f32 = CELL_M;

/// Everything the renderer needs to draw the current water.
#[derive(Debug, Clone, PartialEq)]
pub struct WaterLook {
    pub vertices: Vec<GpuVertex>,
    pub indices: Vec<u32>,
    pub field: WaterField,
}

/// The highest vertical run of wet cells in each column of `frame`, as the
/// column's bottom and top in metres. A run's height is the sum of its cells'
/// fill fractions, so a half-full top cell lowers the surface by half a cell.
/// Lower runs (water under an air gap) are not drawn.
fn top_runs(frame: &WaterKeyframe) -> impl Iterator<Item = (u32, u32, f32, f32)> + '_ {
    let [nx, ny, nz] = frame.dimensions.map(|d| d as usize);
    let cell_m = f64::from(CELL_M) * f64::from(frame.coarsen);
    let base_y = (frame.origin.y as f64) * f64::from(CELL_M);
    (0..nz).flat_map(move |z| {
        (0..nx).filter_map(move |x| {
            let at = |y: usize| frame.fractions[x + nx * (y + ny * z)];
            let mut best = None;
            let mut y = 0;
            while y < ny {
                if at(y) < WATER_DRAW_MIN {
                    y += 1;
                    continue;
                }
                let start = y;
                let mut filled = 0.0;
                while y < ny && at(y) >= WATER_DRAW_MIN {
                    filled += f64::from(at(y)) / 255.0;
                    y += 1;
                }
                let bottom = base_y + start as f64 * cell_m;
                best = Some((bottom as f32, (bottom + filled * cell_m) as f32));
            }
            best.map(|(bottom, top)| (x as u32, z as u32, bottom, top))
        })
    })
}

/// Builds the water sheet and height field for `frames`, keeping only columns
/// within `radius_m` (horizontally) of `center_m`: the same window terrain is
/// drawn in, so water never floats in empty space.
pub fn build_water_look<F: std::borrow::Borrow<WaterKeyframe>>(
    frames: &[F],
    center_m: [f64; 3],
    radius_m: f32,
    material: u32,
) -> WaterLook {
    let dim = (2.0 * radius_m / FIELD_CELL_M).ceil() as u32;
    // Snap the field to the voxel grid so it does not shimmer as it moves.
    let snap = |v: f64| {
        ((v - f64::from(radius_m)) / f64::from(FIELD_CELL_M)).floor() as f32 * FIELD_CELL_M
    };
    let mut field = WaterField::new([snap(center_m[0]), snap(center_m[2])], FIELD_CELL_M, dim);
    let (mut vertices, mut indices) = (Vec::new(), Vec::new());
    for frame in frames.iter().map(std::borrow::Borrow::borrow) {
        let cell_m = CELL_M * f32::from(frame.coarsen);
        let origin_xz = [
            ((frame.origin.x as f64) * f64::from(CELL_M)) as f32,
            ((frame.origin.z as f64) * f64::from(CELL_M)) as f32,
        ];
        let near = |ix: u32, iz: u32| {
            let x = f64::from(origin_xz[0]) + (f64::from(ix) + 0.5) * f64::from(cell_m);
            let z = f64::from(origin_xz[1]) + (f64::from(iz) + 0.5) * f64::from(cell_m);
            (x - center_m[0]).abs() <= f64::from(radius_m)
                && (z - center_m[2]).abs() <= f64::from(radius_m)
        };
        let columns: Vec<WaterColumn> = top_runs(frame)
            .filter(|&(ix, iz, ..)| near(ix, iz))
            .map(|(ix, iz, bottom, top)| WaterColumn {
                ix: ix as i32,
                iz: iz as i32,
                bottom,
                top,
            })
            .collect();
        if columns.is_empty() {
            continue;
        }
        for column in &columns {
            field.raise_column(column, origin_xz, cell_m);
        }
        let (v, i) = build_water_surface(&columns, origin_xz, cell_m, material);
        let offset = vertices.len() as u32;
        vertices.extend(v);
        indices.extend(i.into_iter().map(|index| index + offset));
    }
    WaterLook {
        vertices,
        indices,
        field,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{GlobalCell, Tick};

    /// A `w x d` pool, `depth` cells of full water on a one-cell-high floor
    /// row of dry cells, with column heights from `extra(x, z)` (0..=255) in
    /// the cell above.
    fn pool(w: u32, d: u32, extra: impl Fn(u32, u32) -> u8) -> WaterKeyframe {
        let ny = 3usize;
        let mut fractions = vec![0u8; (w as usize) * ny * (d as usize)];
        for z in 0..d {
            for x in 0..w {
                let at = |y: usize| (x + w * (y as u32 + ny as u32 * z)) as usize;
                fractions[at(0)] = 255;
                fractions[at(1)] = extra(x, z);
            }
        }
        WaterKeyframe {
            server_tick: Tick(0),
            frame_seq: 1,
            origin: GlobalCell::new(0, 0, 0),
            dimensions: [w, ny as u32, d],
            coarsen: 1,
            fractions,
        }
    }

    #[test]
    fn a_column_surface_is_its_summed_fill_fractions() {
        // 255 + 128 of a 0.25 m cell above the base.
        let frame = pool(2, 2, |_, _| 128);
        let runs: Vec<_> = top_runs(&frame).collect();
        assert_eq!(runs.len(), 4);
        let (.., bottom, top) = runs[0];
        assert!((bottom - 0.0).abs() < 1e-6);
        assert!((top - (1.0 + 128.0 / 255.0) * CELL_M).abs() < 1e-5, "{top}");
    }

    #[test]
    fn thin_films_are_not_drawn() {
        let frame = pool(2, 2, |_, _| 0);
        let mut dry = frame.clone();
        dry.fractions
            .iter_mut()
            .for_each(|f| *f = WATER_DRAW_MIN - 1);
        assert_eq!(top_runs(&dry).count(), 0);
    }

    #[test]
    fn the_look_covers_the_pool_in_the_field_and_the_mesh() {
        let frame = pool(8, 8, |x, _| if x % 2 == 0 { 255 } else { 128 });
        let look = build_water_look(&[frame], [1.0, 0.0, 1.0], 8.0, 4);
        assert!(!look.indices.is_empty());
        assert!(look.vertices.iter().all(|v| v.material == 4));
        assert!(
            look.indices
                .iter()
                .all(|&i| (i as usize) < look.vertices.len())
        );
        // The pool occupies x, z in 0..2 m: wet there, dry far away.
        assert!(look.field.surface_at(1.0, 1.0).is_some());
        assert!(look.field.surface_at(5.0, 1.0).is_none());
    }

    #[test]
    fn water_outside_the_window_is_dropped() {
        let frame = pool(8, 8, |_, _| 255);
        let look = build_water_look(&[frame], [500.0, 0.0, 500.0], 8.0, 0);
        assert!(look.indices.is_empty());
        assert!(look.field.heights.iter().all(|&h| h < -1.0e8));
    }
}
