//! Fixed cell sizes, brush fixed-point units, and pose quantization.
//!
//! These are the exact numeric conventions dependent tasks must reuse rather
//! than re-deriving:
//!
//! * **Cell size** is a small integer code, never a raw float. Code `0` is the
//!   0.25 m terrain cell; code `1` is the 0.0625 m detail cell. A brick never
//!   mixes cell sizes.
//! * **Brush geometry** is expressed in a volume's local cell space in
//!   fixed-point with [`BRUSH_FRACTION_BITS`] fractional bits (1 unit =
//!   1/256 cell). Sphere inclusion compares squared distance in `i128` so it
//!   cannot overflow.
//! * **Pose**: translation is IEEE-754 `f64` metres (finiteness-checked, not
//!   lossily quantized, because world authority positions are `f64`).
//!   Orientation is a unit quaternion quantized to four `i16` components with
//!   scale [`QUAT_SCALE`]; the decoder renormalizes and rejects a zero-norm
//!   quaternion.

use serde::{Deserialize, Serialize};

/// Fixed cell-size codes. The wire/save form is the `u8` discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum CellSizeCode {
    /// 0.25 m — terrain and one-metre placement tools (`4 x 4 x 4` cells).
    Quarter = 0,
    /// 0.0625 m — optional detailed local objects.
    Sixteenth = 1,
}

impl CellSizeCode {
    /// Edge length of one cell in metres.
    pub const fn metres(self) -> f64 {
        match self {
            Self::Quarter => 0.25,
            Self::Sixteenth => 0.0625,
        }
    }

    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Quarter),
            1 => Some(Self::Sixteenth),
            _ => None,
        }
    }
}

/// Fractional bits in a brush fixed-point coordinate. `1 << 8 = 256` steps per
/// cell.
pub const BRUSH_FRACTION_BITS: u32 = 8;
/// Fixed-point units per whole cell.
pub const BRUSH_UNIT: i64 = 1 << BRUSH_FRACTION_BITS;

/// A brush centre in a volume's local cell space, fixed-point. Each axis is
/// cells scaled by [`BRUSH_UNIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrushPoint {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

impl BrushPoint {
    pub const fn from_units(x: i64, y: i64, z: i64) -> Self {
        Self { x, y, z }
    }

    /// Builds a point from whole-cell indices.
    pub const fn from_cells(x: i64, y: i64, z: i64) -> Option<Self> {
        match (
            x.checked_mul(BRUSH_UNIT),
            y.checked_mul(BRUSH_UNIT),
            z.checked_mul(BRUSH_UNIT),
        ) {
            (Some(x), Some(y), Some(z)) => Some(Self { x, y, z }),
            _ => None,
        }
    }
}

/// A sphere brush: fixed-point centre and non-negative fixed-point radius, both
/// in [`BRUSH_UNIT`] units of the target volume's cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawSphereBrush")]
pub struct SphereBrush {
    pub centre: BrushPoint,
    radius_units: i64,
}

#[derive(Deserialize)]
struct RawSphereBrush {
    centre: BrushPoint,
    radius_units: i64,
}

impl TryFrom<RawSphereBrush> for SphereBrush {
    type Error = BrushError;

    fn try_from(raw: RawSphereBrush) -> Result<Self, Self::Error> {
        Self::new(raw.centre, raw.radius_units)
    }
}

/// Error building a brush from untrusted input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BrushError {
    #[error("brush radius is negative")]
    NegativeRadius,
    #[error("brush radius exceeds the permitted maximum")]
    RadiusTooLarge,
}

/// Largest brush radius accepted, in whole cells. Bounds the cell count a
/// single edit can touch before the server plans it.
pub const MAX_BRUSH_RADIUS_CELLS: i64 = 256;

impl SphereBrush {
    pub fn new(centre: BrushPoint, radius_units: i64) -> Result<Self, BrushError> {
        if radius_units < 0 {
            return Err(BrushError::NegativeRadius);
        }
        if radius_units > MAX_BRUSH_RADIUS_CELLS * BRUSH_UNIT {
            return Err(BrushError::RadiusTooLarge);
        }
        Ok(Self {
            centre,
            radius_units,
        })
    }

    pub const fn radius_units(self) -> i64 {
        self.radius_units
    }

    /// Inclusion test: a cell whose centre is at fixed-point `(cx, cy, cz)` is
    /// removed when its squared distance to the brush centre is `<=` the
    /// squared radius. Operands are widened to `i128` before subtraction, and
    /// the squared-distance sum uses checked arithmetic: any point far enough
    /// to overflow it is far outside a bounded-radius sphere and is reported as
    /// not contained rather than panicking.
    pub fn contains_cell_centre(self, cx: i64, cy: i64, cz: i64) -> bool {
        let dx = cx as i128 - self.centre.x as i128;
        let dy = cy as i128 - self.centre.y as i128;
        let dz = cz as i128 - self.centre.z as i128;
        let squared = match dx
            .checked_mul(dx)
            .and_then(|xx| dy.checked_mul(dy).and_then(|yy| xx.checked_add(yy)))
            .and_then(|xy| dz.checked_mul(dz).and_then(|zz| xy.checked_add(zz)))
        {
            Some(value) => value,
            None => return false,
        };
        let radius = self.radius_units as i128;
        squared <= radius * radius
    }
}

/// Quantization scale for quaternion components: a unit component maps to
/// `±32767`.
pub const QUAT_SCALE: f32 = 32_767.0;

/// A unit quaternion quantized to four `i16` components (`x, y, z, w`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuantizedQuat {
    pub x: i16,
    pub y: i16,
    pub z: i16,
    pub w: i16,
}

/// Error decoding a quantized quaternion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("quantized quaternion has zero magnitude")]
pub struct ZeroQuaternion;

impl QuantizedQuat {
    /// Quantizes a quaternion. The input is normalized first; a zero-norm
    /// quaternion is rejected.
    pub fn from_unit(x: f32, y: f32, z: f32, w: f32) -> Result<Self, ZeroQuaternion> {
        let norm = (x * x + y * y + z * z + w * w).sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            return Err(ZeroQuaternion);
        }
        let q = |v: f32| {
            let scaled = (v / norm * QUAT_SCALE).round();
            scaled.clamp(-QUAT_SCALE, QUAT_SCALE) as i16
        };
        Ok(Self {
            x: q(x),
            y: q(y),
            z: q(z),
            w: q(w),
        })
    }

    /// Dequantizes to a renormalized unit quaternion `(x, y, z, w)`.
    pub fn to_unit(self) -> Result<[f32; 4], ZeroQuaternion> {
        let x = self.x as f32 / QUAT_SCALE;
        let y = self.y as f32 / QUAT_SCALE;
        let z = self.z as f32 / QUAT_SCALE;
        let w = self.w as f32 / QUAT_SCALE;
        let norm = (x * x + y * y + z * z + w * w).sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            return Err(ZeroQuaternion);
        }
        Ok([x / norm, y / norm, z / norm, w / norm])
    }
}

/// A rigid pose: `f64` metre translation plus a quantized orientation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub translation_m: [f64; 3],
    pub rotation: QuantizedQuat,
}

/// Error raised when a pose contains a non-finite translation component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("pose translation is not finite")]
pub struct NonFinitePose;

impl Pose {
    /// Validates that every translation component is finite.
    pub fn checked(self) -> Result<Self, NonFinitePose> {
        if self.translation_m.iter().all(|v| v.is_finite()) {
            Ok(self)
        } else {
            Err(NonFinitePose)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_size_codes_round_trip_and_reject_unknown() {
        assert_eq!(CellSizeCode::from_u8(0), Some(CellSizeCode::Quarter));
        assert_eq!(CellSizeCode::from_u8(1), Some(CellSizeCode::Sixteenth));
        assert_eq!(CellSizeCode::from_u8(2), None);
        assert_eq!(CellSizeCode::Quarter.metres(), 0.25);
    }

    #[test]
    fn sphere_inclusion_is_exact_at_the_boundary() {
        let brush =
            SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 2 * BRUSH_UNIT).unwrap();
        // Exactly on the radius is included.
        assert!(brush.contains_cell_centre(2 * BRUSH_UNIT, 0, 0));
        // One unit past is excluded.
        assert!(!brush.contains_cell_centre(2 * BRUSH_UNIT + 1, 0, 0));
    }

    #[test]
    fn sphere_inclusion_does_not_overflow_on_extreme_inputs() {
        let brush = SphereBrush::new(
            BrushPoint::from_units(i64::MIN, 0, 0),
            MAX_BRUSH_RADIUS_CELLS * BRUSH_UNIT,
        )
        .unwrap();
        // The full i64 span between centre and cell must not panic.
        assert!(!brush.contains_cell_centre(i64::MAX, i64::MAX, i64::MIN));
    }

    #[test]
    fn brush_rejects_negative_and_oversized_radius() {
        let centre = BrushPoint::from_cells(0, 0, 0).unwrap();
        assert_eq!(
            SphereBrush::new(centre, -1),
            Err(BrushError::NegativeRadius)
        );
        assert_eq!(
            SphereBrush::new(centre, MAX_BRUSH_RADIUS_CELLS * BRUSH_UNIT + 1),
            Err(BrushError::RadiusTooLarge)
        );
    }

    #[test]
    fn quaternion_quantization_round_trips_within_tolerance() {
        let (x, y, z, w) = (0.0_f32, 0.0, 0.382_683_4, 0.923_879_5); // 45° about Z
        let q = QuantizedQuat::from_unit(x, y, z, w).unwrap();
        let back = q.to_unit().unwrap();
        for (a, b) in back.iter().zip([x, y, z, w]) {
            assert!((a - b).abs() < 1.0e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn zero_quaternion_is_rejected_both_directions() {
        assert_eq!(
            QuantizedQuat::from_unit(0.0, 0.0, 0.0, 0.0),
            Err(ZeroQuaternion)
        );
        let zero = QuantizedQuat {
            x: 0,
            y: 0,
            z: 0,
            w: 0,
        };
        assert_eq!(zero.to_unit(), Err(ZeroQuaternion));
    }

    #[test]
    fn pose_rejects_non_finite_translation() {
        let good = Pose {
            translation_m: [1.0, -2.0, 3.5],
            rotation: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
        };
        assert!(good.checked().is_ok());

        let bad = Pose {
            translation_m: [1.0, f64::INFINITY, 0.0],
            rotation: good.rotation,
        };
        assert_eq!(bad.checked(), Err(NonFinitePose));

        let nan = Pose {
            translation_m: [f64::NAN, 0.0, 0.0],
            rotation: good.rotation,
        };
        assert_eq!(nan.checked(), Err(NonFinitePose));
    }
}
