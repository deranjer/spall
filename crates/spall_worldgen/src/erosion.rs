//! Stream-power erosion on the coarse height grid.
//!
//! The implicit scheme of Braun and Willett: each iteration routes flow with a
//! priority flood, then solves `h = (h + F * h_receiver + U dt) / (1 + F)` from
//! the outlets upstream, with `F = K dt sqrt(A) / dx`. It is unconditionally
//! stable and O(n) per iteration. A few thermal talus passes follow, so slopes
//! cannot stand steeper than rock would.
//!
//! Only `+ - * /`, `floor` and `sqrt` touch `f64` here, so the result is
//! platform-exact. It is single-threaded and sequential in a fixed cell order,
//! so it cannot depend on a thread count.

use crate::hydrology::{FP, OUTLET_NONE, accumulate, priority_flood};

/// Diagonal neighbour distance in samples.
const DIAGONAL: f64 = std::f64::consts::SQRT_2;

#[derive(Debug, Clone, Copy)]
pub struct ErosionParams {
    pub iterations: u32,
    /// Erodibility times time step. Larger digs deeper valleys per iteration.
    pub k_dt: f64,
    /// Time step multiplying the uplift field.
    pub dt: f64,
    /// Steepest stable slope in cells of height per sample of distance.
    pub talus: f64,
    /// Thermal relaxation passes after the fluvial iterations.
    pub thermal_passes: u32,
}

impl Default for ErosionParams {
    fn default() -> Self {
        Self {
            iterations: 10,
            k_dt: 0.004,
            dt: 1.0,
            talus: 2.4,
            thermal_passes: 3,
        }
    }
}

fn to_fixed(h: &[f64]) -> Vec<i32> {
    h.iter()
        .map(|v| (v * f64::from(FP)).floor() as i32)
        .collect()
}

/// Erodes `h` (cells) in place. `outlet` cells are fixed boundaries; `uplift`
/// is per-iteration rock uplift in cells (before `dt`).
pub fn erode(h: &mut [f64], n: usize, outlet: &[u8], uplift: &[f64], params: &ErosionParams) {
    let total = n * n;
    assert_eq!(h.len(), total);
    for _ in 0..params.iterations {
        let flood = priority_flood(&to_fixed(h), n, outlet);
        let area = accumulate(&flood);
        for &i in &flood.order {
            let i = i as usize;
            let r = flood.receiver[i] as usize;
            if r == i {
                continue;
            }
            let dist = if (i % n).abs_diff(r % n) + (i / n).abs_diff(r / n) == 2 {
                DIAGONAL
            } else {
                1.0
            };
            // Cells that do not drain downhill (pits, flats) are left alone,
            // so basins stay basins instead of silting up.
            if h[i] > h[r] {
                let f = params.k_dt * f64::from(area[i]).sqrt() / dist;
                h[i] = (h[i] + f * h[r] + params.dt * uplift[i]) / (1.0 + f);
            }
        }
    }
    for _ in 0..params.thermal_passes {
        thermal_pass(h, n, outlet, params.talus);
    }
}

/// One in-place talus pass in index order: wherever a cell stands more than
/// `talus` per sample above a neighbour, a quarter of the excess moves downhill.
fn thermal_pass(h: &mut [f64], n: usize, outlet: &[u8], talus: f64) {
    use crate::hydrology::{DX, DZ};
    for z in 0..n as i64 {
        for x in 0..n as i64 {
            let i = x as usize + n * z as usize;
            if outlet[i] != OUTLET_NONE {
                continue;
            }
            for d in 0..8 {
                let (nx, nz) = (x + DX[d], z + DZ[d]);
                if nx < 0 || nz < 0 || nx >= n as i64 || nz >= n as i64 {
                    continue;
                }
                let j = nx as usize + n * nz as usize;
                if outlet[j] != OUTLET_NONE {
                    continue;
                }
                let limit = if d >= 4 { talus * DIAGONAL } else { talus };
                let excess = h[i] - h[j] - limit;
                if excess > 0.0 {
                    h[i] -= excess * 0.25;
                    h[j] += excess * 0.25;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp() -> (Vec<f64>, usize, Vec<u8>, Vec<f64>) {
        let n = 48;
        let mut h = vec![0.0; n * n];
        let mut outlet = vec![0u8; n * n];
        for z in 0..n {
            for x in 0..n {
                h[x + n * z] = 60.0 + z as f64 * 3.0 + ((x * 7 + z * 13) % 5) as f64;
            }
        }
        for x in 0..n {
            h[x] = 10.0;
            outlet[x] = 1;
        }
        (h, n, outlet, vec![1.0; n * n])
    }

    #[test]
    fn erosion_is_repeatable_and_keeps_outlets_fixed() {
        let (h0, n, outlet, up) = ramp();
        let (mut a, mut b) = (h0.clone(), h0.clone());
        erode(&mut a, n, &outlet, &up, &ErosionParams::default());
        erode(&mut b, n, &outlet, &up, &ErosionParams::default());
        assert_eq!(a, b);
        assert!(a[..n].iter().all(|&h| h == 10.0), "outlet moved");
        assert_ne!(a, h0, "erosion changed nothing");
    }

    #[test]
    fn talus_pass_conserves_mass_and_flattens_cliffs() {
        let n = 16;
        let mut h = vec![0.0; n * n];
        h[8 + n * 8] = 200.0;
        let outlet = vec![0u8; n * n];
        let before: f64 = h.iter().sum();
        for _ in 0..40 {
            thermal_pass(&mut h, n, &outlet, 2.0);
        }
        let after: f64 = h.iter().sum();
        assert!((before - after).abs() < 1e-9, "{before} vs {after}");
        assert!(h[8 + n * 8] < 100.0);
    }
}
