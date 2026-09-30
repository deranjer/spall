//! Deterministic lattice value noise.
//!
//! Output must be bit-identical on every platform, because a world is a pure
//! function of its seed. So this module uses integer hashing and only
//! `+ - * /`, `floor` and comparisons on `f64`: no `sin`, `cos`, `powf` or
//! other libm calls whose last bit can differ between targets.

#[inline]
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Uniform value in `[-1, 1]` at an integer lattice point.
#[inline]
fn lattice(seed: u64, x: i64, y: i64, z: i64) -> f64 {
    let h = mix(
        seed.wrapping_add((x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))
            ^ (y as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
            ^ (z as u64).wrapping_mul(0x1656_67b1_9e37_79f9),
    );
    (h >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

#[inline]
fn fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Uniform value in `[0, 1)` for an integer key: independent draws, no spatial
/// correlation (unlike [`value2`]).
pub fn hash01(seed: u64, key: u64) -> f64 {
    (mix(seed ^ mix(key)) >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth value noise in `[-1, 1]`.
pub fn value3(seed: u64, x: f64, y: f64, z: f64) -> f64 {
    let (xf, yf, zf) = (x.floor(), y.floor(), z.floor());
    let (xi, yi, zi) = (xf as i64, yf as i64, zf as i64);
    let (u, v, w) = (fade(x - xf), fade(y - yf), fade(z - zf));
    let c = |dx, dy, dz| lattice(seed, xi + dx, yi + dy, zi + dz);
    let x00 = lerp(c(0, 0, 0), c(1, 0, 0), u);
    let x10 = lerp(c(0, 1, 0), c(1, 1, 0), u);
    let x01 = lerp(c(0, 0, 1), c(1, 0, 1), u);
    let x11 = lerp(c(0, 1, 1), c(1, 1, 1), u);
    lerp(lerp(x00, x10, v), lerp(x01, x11, v), w)
}

/// Smooth 2D value noise in `[-1, 1]`.
pub fn value2(seed: u64, x: f64, z: f64) -> f64 {
    let (xf, zf) = (x.floor(), z.floor());
    let (xi, zi) = (xf as i64, zf as i64);
    let (u, w) = (fade(x - xf), fade(z - zf));
    let c = |dx, dz| lattice(seed, xi + dx, 0, zi + dz);
    lerp(lerp(c(0, 0), c(1, 0), u), lerp(c(0, 1), c(1, 1), u), w)
}

#[inline]
fn octave_seed(seed: u64, octave: u32) -> u64 {
    mix(seed ^ (u64::from(octave) + 1).wrapping_mul(0xa076_1d64_78bd_642f))
}

/// Fractal 2D noise, normalized to `[-1, 1]`.
pub fn fbm2(seed: u64, x: f64, z: f64, octaves: u32) -> f64 {
    let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
    for o in 0..octaves {
        sum += amp * value2(octave_seed(seed, o), x * freq, z * freq);
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Fractal 3D noise, normalized to `[-1, 1]`.
pub fn fbm3(seed: u64, x: f64, y: f64, z: f64, octaves: u32) -> f64 {
    let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
    for o in 0..octaves {
        sum += amp * value3(octave_seed(seed, o), x * freq, y * freq, z * freq);
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Ridged 2D noise in `[0, 1]`: sharp crests where the base noise crosses zero.
pub fn ridged2(seed: u64, x: f64, z: f64, octaves: u32) -> f64 {
    let (mut sum, mut norm, mut amp, mut freq) = (0.0, 0.0, 1.0, 1.0);
    for o in 0..octaves {
        let n = value2(octave_seed(seed, o), x * freq, z * freq);
        let r = 1.0 - n.abs();
        sum += amp * r * r;
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Hermite smoothstep. Edges may be given in either order.
pub fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_bounded_and_repeatable() {
        for i in 0..2000 {
            let (x, y, z) = (i as f64 * 0.37 - 300.0, i as f64 * 0.11, i as f64 * 0.73);
            for n in [
                value3(7, x, y, z),
                value2(7, x, z),
                fbm2(7, x, z, 4),
                fbm3(7, x, y, z, 3),
            ] {
                assert!((-1.0..=1.0).contains(&n), "{n}");
            }
            assert!((0.0..=1.0).contains(&ridged2(7, x, z, 4)));
            assert_eq!(fbm3(7, x, y, z, 3), fbm3(7, x, y, z, 3));
        }
    }

    #[test]
    fn seeds_decorrelate_and_noise_is_continuous() {
        let differing = (0..200)
            .filter(|i| value2(1, *i as f64 * 1.3, 0.5) != value2(2, *i as f64 * 1.3, 0.5))
            .count();
        assert!(differing > 190);
        // Continuity across lattice boundaries: tiny step, tiny change.
        for i in 0..200 {
            let x = i as f64 * 0.5;
            assert!((value2(3, x, 2.0) - value2(3, x + 1e-6, 2.0)).abs() < 1e-4);
        }
    }

    #[test]
    fn value_noise_uses_the_whole_range() {
        let (mut lo, mut hi) = (1.0_f64, -1.0_f64);
        for i in 0..20000 {
            let n = value2(9, i as f64 * 0.91, i as f64 * 0.37);
            lo = lo.min(n);
            hi = hi.max(n);
        }
        assert!(lo < -0.8 && hi > 0.8, "{lo} {hi}");
    }
}
