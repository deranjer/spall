//! How much water a river carries and how deep it runs, in the units of the
//! column water solver (cells and seconds).
//!
//! A spring is a fixed discharge. The solver's only flow friction is linear
//! damping, so a river on slope `S` settles at the speed `v = g * S / damping`,
//! and carrying `Q` takes a cross-section `Q / v`. Worldgen seeds each river at
//! that depth so it starts near its running state instead of flooding or
//! draining. The constants mirror `spall_fluid::ColumnConfig` (worldgen may not
//! depend on it); a sandbox test keeps them equal.
//!
//! Only `+ - * /` and `sqrt` touch `f64`, so the result is platform-exact.

/// Gravity in cells/s^2 (9.81 m/s^2 over 0.25 m cells).
pub const GRAVITY_CELLS_S2: f64 = 9.81 / 0.25;
/// Linear flow damping, 1/s.
pub const DAMPING_PER_S: f64 = 2.0;
/// Rain that reaches the river network, in cells of depth per second over the
/// catchment: a 1 km^2 catchment carries about 2.5 m^3/s at 1e-5.
pub const RUNOFF_CELLS_PER_S: f64 = 1.0e-5;
/// Cell^2 of ground in one grid sample (1 m x 1 m at 0.25 m cells).
pub const SAMPLE_AREA_CELLS2: f64 = 16.0;
/// What one drain cell can empty: one cell^3 per solver step (0.05 s).
pub const SINK_CELL_CAPACITY_CELLS3_S: f64 = 20.0;
/// Rivers on flatter ground than this still run at the speed this slope gives.
pub const MIN_SLOPE: f64 = 0.002;

/// Discharge of a river that drains `samples` grid samples, cells^3/s.
pub fn discharge(samples: u64) -> f64 {
    RUNOFF_CELLS_PER_S * samples as f64 * SAMPLE_AREA_CELLS2
}

/// The cube root of `c >= 0` by Newton's method, a fixed 48 iterations.
fn cbrt(c: f64) -> f64 {
    if c <= 0.0 {
        return 0.0;
    }
    let mut y = c.max(1.0);
    for _ in 0..48 {
        y = (2.0 * y + c / (y * y)) / 3.0;
    }
    y
}

/// Depth of water above the floor of a parabolic channel (depth `channel_depth`
/// at half-width `half_width`, both in cells) that carries `q` cells^3/s down
/// `slope`, clamped to 90% of the channel. The wetted area of such a channel
/// filled to `d` is `4/3 * half_width * d^1.5 / sqrt(channel_depth)`.
pub fn uniform_flow_depth(q: f64, slope: f64, channel_depth: f64, half_width: f64) -> f64 {
    let v = GRAVITY_CELLS_S2 * slope.max(MIN_SLOPE) / DAMPING_PER_S;
    let area = q / v;
    // d^1.5 = 3 * area * sqrt(channel_depth) / (4 * half_width)
    let y = cbrt(3.0 * area * channel_depth.sqrt() / (4.0 * half_width));
    (y * y).min(0.9 * channel_depth)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbrt_is_accurate_and_repeatable() {
        for c in [0.0, 1e-6, 0.5, 1.0, 8.0, 27.0, 1e3, 1e9] {
            let y = cbrt(c);
            assert!((y * y * y - c).abs() <= 1e-9 * c.max(1.0), "{c}: {y}");
            assert_eq!(y, cbrt(c));
        }
    }

    #[test]
    fn more_water_or_flatter_ground_runs_deeper_never_past_the_banks() {
        let (depth, half_width) = (4.0, 12.0);
        let base = uniform_flow_depth(5.0, 0.02, depth, half_width);
        assert!(uniform_flow_depth(10.0, 0.02, depth, half_width) > base);
        assert!(uniform_flow_depth(5.0, 0.01, depth, half_width) > base);
        assert!(uniform_flow_depth(1e9, 0.02, depth, half_width) <= 0.9 * depth + 1e-12);
        assert_eq!(uniform_flow_depth(0.0, 0.02, depth, half_width), 0.0);
    }

    #[test]
    fn the_depth_carries_the_discharge() {
        // Solve it back: the wetted area at that depth moves q at the settled speed.
        let (q, slope, depth, half_width) = (3.0, 0.03, 6.0, 10.0);
        let d = uniform_flow_depth(q, slope, depth, half_width);
        assert!(d < 0.9 * depth, "unclamped for this case");
        let area = 4.0 / 3.0 * half_width * d * d.sqrt() / depth.sqrt();
        let v = GRAVITY_CELLS_S2 * slope / DAMPING_PER_S;
        assert!((area * v - q).abs() < 1e-9 * q);
    }

    #[test]
    fn discharge_scales_with_the_catchment() {
        assert_eq!(discharge(0), 0.0);
        assert!((discharge(2_000) - 2.0 * discharge(1_000)).abs() < 1e-12);
    }
}
