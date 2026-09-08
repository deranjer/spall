//! The six axis-aligned face directions and their tangent bases.
//!
//! Each direction fixes an outward normal and an ordered `(u, v)` tangent pair
//! chosen so that `u_dir x v_dir == normal`. Emitting a quad's corners in the
//! order `(u0,v0) -> (u1,v0) -> (u1,v1) -> (u0,v1)` then gives counter-clockwise
//! winding seen from outside the surface, which is what the renderer treats as
//! front-facing.

/// One of the six axis-aligned cell faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FaceDir {
    NegX = 0,
    PosX = 1,
    NegY = 2,
    PosY = 3,
    NegZ = 4,
    PosZ = 5,
}

/// Every face direction in canonical order.
pub const FACE_DIRS: [FaceDir; 6] = [
    FaceDir::NegX,
    FaceDir::PosX,
    FaceDir::NegY,
    FaceDir::PosY,
    FaceDir::NegZ,
    FaceDir::PosZ,
];

impl FaceDir {
    /// Stable numeric code, matching the enum discriminant.
    #[inline]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The axis the normal runs along: `0 = X`, `1 = Y`, `2 = Z`.
    #[inline]
    pub const fn normal_axis(self) -> usize {
        match self {
            FaceDir::NegX | FaceDir::PosX => 0,
            FaceDir::NegY | FaceDir::PosY => 1,
            FaceDir::NegZ | FaceDir::PosZ => 2,
        }
    }

    /// `+1` for a positive face, `-1` for a negative one.
    #[inline]
    pub const fn sign(self) -> i64 {
        match self {
            FaceDir::PosX | FaceDir::PosY | FaceDir::PosZ => 1,
            FaceDir::NegX | FaceDir::NegY | FaceDir::NegZ => -1,
        }
    }

    /// Outward unit normal as an integer triple.
    #[inline]
    pub const fn normal(self) -> [i64; 3] {
        let mut n = [0i64; 3];
        n[self.normal_axis()] = self.sign();
        n
    }

    /// Outward normal as `f32`, for vertex attributes.
    #[inline]
    pub fn normal_f32(self) -> [f32; 3] {
        let [x, y, z] = self.normal();
        [x as f32, y as f32, z as f32]
    }

    /// The `(u_axis, v_axis)` tangent pair. `unit_u(u_axis) x unit_v(v_axis)`
    /// equals [`FaceDir::normal`].
    #[inline]
    pub const fn tangent_axes(self) -> (usize, usize) {
        match self {
            FaceDir::PosX => (1, 2),
            FaceDir::NegX => (2, 1),
            FaceDir::PosY => (2, 0),
            FaceDir::NegY => (0, 2),
            FaceDir::PosZ => (0, 1),
            FaceDir::NegZ => (1, 0),
        }
    }

    /// The neighbour cell that must be non-solid for this face of `cell` to be
    /// exposed: `cell + normal`.
    #[inline]
    pub const fn neighbour(self, cell: [i64; 3]) -> [i64; 3] {
        let n = self.normal();
        [cell[0] + n[0], cell[1] + n[1], cell[2] + n[2]]
    }

    /// Integer coordinate of this face's plane along the normal axis, for a
    /// solid cell whose minimum corner is at `cell`.
    #[inline]
    pub const fn plane(self, cell: [i64; 3]) -> i64 {
        let a = self.normal_axis();
        match self.sign() {
            1 => cell[a] + 1,
            _ => cell[a],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cross(a: [i64; 3], b: [i64; 3]) -> [i64; 3] {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    }

    fn unit(axis: usize) -> [i64; 3] {
        let mut v = [0i64; 3];
        v[axis] = 1;
        v
    }

    #[test]
    fn tangent_basis_is_right_handed_about_the_outward_normal() {
        for dir in FACE_DIRS {
            let (u, v) = dir.tangent_axes();
            assert_eq!(
                cross(unit(u), unit(v)),
                dir.normal(),
                "u x v must equal the outward normal for {dir:?}"
            );
        }
    }

    #[test]
    fn plane_and_neighbour_bracket_the_solid_cell() {
        let cell = [4, -2, 7];
        assert_eq!(FaceDir::PosX.plane(cell), 5);
        assert_eq!(FaceDir::NegX.plane(cell), 4);
        assert_eq!(FaceDir::PosX.neighbour(cell), [5, -2, 7]);
        assert_eq!(FaceDir::NegY.neighbour(cell), [4, -3, 7]);
    }

    #[test]
    fn codes_are_dense_and_ordered() {
        for (i, dir) in FACE_DIRS.iter().enumerate() {
            assert_eq!(dir.code() as usize, i);
        }
    }
}
