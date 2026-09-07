//! The quad and triangle mesh types, plus a content digest for pinning
//! fixtures in tests.

use spall_core::MaterialId;

use crate::ao::{CORNER_OFFSETS, ao_factor};
use crate::face::FaceDir;

/// Which face emitter produced a mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshStrategy {
    /// One unit quad per exposed cell face — the reference surface.
    Culled,
    /// Coplanar faces merged by material, facing, and AO tuple.
    Greedy,
}

impl MeshStrategy {
    #[inline]
    pub const fn code(self) -> u8 {
        match self {
            MeshStrategy::Culled => 0,
            MeshStrategy::Greedy => 1,
        }
    }
}

/// A rectangular run of coplanar, same-facing, same-material cell faces.
///
/// `plane` is the integer coordinate along the face's normal axis; `u0`/`v0`
/// and `u_len`/`v_len` are the minimum corner and extent (in cells, `>= 1`) in
/// the face's `(u_axis, v_axis)` tangent plane. A [`MeshStrategy::Culled`] quad
/// always has `u_len == v_len == 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceQuad {
    pub dir: FaceDir,
    pub material: MaterialId,
    pub plane: i64,
    pub u0: i64,
    pub v0: i64,
    pub u_len: i64,
    pub v_len: i64,
    /// Corner occlusion levels `0..=3` in [`CORNER_OFFSETS`] order.
    pub ao: [u8; 4],
}

impl FaceQuad {
    /// Area of this quad in cells squared.
    #[inline]
    pub fn area_cells(&self) -> i64 {
        self.u_len * self.v_len
    }

    /// The four corner cell coordinates in emission order.
    pub fn corners(&self) -> [[i64; 3]; 4] {
        let a = self.dir.normal_axis();
        let (ua, va) = self.dir.tangent_axes();
        let mut out = [[0i64; 3]; 4];
        for (i, (du, dv)) in CORNER_OFFSETS.iter().enumerate() {
            let mut p = [0i64; 3];
            p[a] = self.plane;
            p[ua] = self.u0 + du * self.u_len;
            p[va] = self.v0 + dv * self.v_len;
            out[i] = p;
        }
        out
    }

    /// Expand a merged quad back into its `u_len * v_len` unit faces. Used by
    /// tests to prove greedy output covers exactly the culled face set.
    pub fn unit_faces(&self) -> impl Iterator<Item = (FaceDir, i64, i64, i64)> + '_ {
        let dir = self.dir;
        let plane = self.plane;
        (0..self.v_len).flat_map(move |dv| {
            (0..self.u_len).map(move |du| (dir, plane, self.u0 + du, self.v0 + dv))
        })
    }
}

/// One triangle-mesh vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Volume-local position in metres.
    pub position: [f32; 3],
    /// Outward face normal.
    pub normal: [f32; 3],
    pub material: u32,
    /// Baked ambient-occlusion brightness multiplier, `0.35..=1.0`.
    pub ao: f32,
    /// In-plane tangent coordinates in cells, for procedural surface detail.
    pub local_uv: [f32; 2],
}

/// A triangle mesh: interleaved [`Vertex`] data and a `u32` index buffer. Two
/// triangles per quad, wound counter-clockwise as seen from outside the
/// surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    /// Triangulates `quads` into a mesh. `cell_m` is the volume's cell edge
    /// length in metres.
    pub fn from_quads(quads: &[FaceQuad], cell_m: f64) -> Self {
        let mut mesh = Mesh {
            vertices: Vec::with_capacity(quads.len() * 4),
            indices: Vec::with_capacity(quads.len() * 6),
        };
        let scale = cell_m as f32;
        for quad in quads {
            let base = mesh.vertices.len() as u32;
            let normal = quad.dir.normal_f32();
            let (ua, va) = quad.dir.tangent_axes();
            let corners = quad.corners();
            for (i, corner) in corners.iter().enumerate() {
                mesh.vertices.push(Vertex {
                    position: [
                        corner[0] as f32 * scale,
                        corner[1] as f32 * scale,
                        corner[2] as f32 * scale,
                    ],
                    normal,
                    material: u32::from(quad.material.raw()),
                    ao: ao_factor(quad.ao[i]),
                    local_uv: [corner[ua] as f32, corner[va] as f32],
                });
            }
            mesh.indices
                .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        mesh
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Summary counts for one built mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshStats {
    pub strategy: MeshStrategy,
    pub quad_count: usize,
    pub vertex_count: usize,
    pub triangle_count: usize,
    /// Total exposed surface area in square metres. Independent of strategy.
    pub surface_area_m2: f64,
    /// Number of exposed unit cell faces (equals the culled quad count).
    pub exposed_unit_faces: u64,
    /// Exposed faces emitted against a non-resident halo brick.
    pub unresolved_halo_faces: u64,
}

const DIGEST_DOMAIN: &[u8] = b"spall.mesh.v1";

/// BLAKE3 digest over a mesh's vertices and indices. Stable across runs and
/// platforms for a given quad set; pins fixtures against accidental change.
pub fn mesh_digest(mesh: &Mesh) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&(DIGEST_DOMAIN.len() as u32).to_le_bytes());
    h.update(DIGEST_DOMAIN);
    h.update(&(mesh.vertices.len() as u32).to_le_bytes());
    for v in &mesh.vertices {
        for f in v.position.iter().chain(&v.normal).chain(&v.local_uv) {
            h.update(&f.to_bits().to_le_bytes());
        }
        h.update(&v.ao.to_bits().to_le_bytes());
        h.update(&v.material.to_le_bytes());
    }
    h.update(&(mesh.indices.len() as u32).to_le_bytes());
    for i in &mesh.indices {
        h.update(&i.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// Lowercase hex of [`mesh_digest`].
pub fn mesh_digest_hex(mesh: &Mesh) -> String {
    mesh_digest(mesh)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    fn quad(dir: FaceDir) -> FaceQuad {
        FaceQuad {
            dir,
            material: MaterialId(1),
            plane: dir.plane([0, 0, 0]),
            u0: 0,
            v0: 0,
            u_len: 1,
            v_len: 1,
            ao: [3, 3, 3, 3],
        }
    }

    #[test]
    fn first_triangle_winds_counter_clockwise_about_the_outward_normal() {
        for dir in crate::face::FACE_DIRS {
            let mesh = Mesh::from_quads(&[quad(dir)], 0.25);
            let p: Vec<Vec3> = mesh
                .vertices
                .iter()
                .map(|v| Vec3::from_array(v.position))
                .collect();
            let face_normal = Vec3::from_array(dir.normal_f32());
            let tri_normal = (p[1] - p[0]).cross(p[2] - p[0]);
            assert!(
                tri_normal.dot(face_normal) > 0.0,
                "{dir:?}: triangle normal {tri_normal:?} must agree with {face_normal:?}"
            );
        }
    }

    #[test]
    fn quad_produces_four_vertices_and_two_triangles() {
        let mesh = Mesh::from_quads(&[quad(FaceDir::PosY)], 0.25);
        assert_eq!(mesh.vertices.len(), 4);
        assert_eq!(mesh.triangle_count(), 2);
        // A 1-cell quad at 0.25 m spans 0.25 m on each tangent axis.
        let xs: Vec<f32> = mesh.vertices.iter().map(|v| v.position[0]).collect();
        assert!(xs.iter().any(|x| (*x - 0.25).abs() < 1e-6));
    }

    #[test]
    fn digest_is_stable_and_sensitive_to_geometry() {
        let a = Mesh::from_quads(&[quad(FaceDir::PosY)], 0.25);
        let b = Mesh::from_quads(&[quad(FaceDir::PosY)], 0.25);
        assert_eq!(mesh_digest(&a), mesh_digest(&b));
        let c = Mesh::from_quads(&[quad(FaceDir::NegY)], 0.25);
        assert_ne!(mesh_digest(&a), mesh_digest(&c));
    }

    #[test]
    fn merged_quad_expands_to_its_unit_faces() {
        let q = FaceQuad {
            dir: FaceDir::PosY,
            material: MaterialId(1),
            plane: 1,
            u0: 2,
            v0: -1,
            u_len: 3,
            v_len: 2,
            ao: [3, 3, 3, 3],
        };
        let faces: Vec<_> = q.unit_faces().collect();
        assert_eq!(faces.len(), 6);
        assert!(faces.contains(&(FaceDir::PosY, 1, 2, -1)));
        assert!(faces.contains(&(FaceDir::PosY, 1, 4, 0)));
    }
}
