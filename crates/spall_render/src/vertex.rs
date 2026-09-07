//! The GPU vertex layout and the CPU-side transform from a `spall_mesh` mesh.

use bytemuck::{Pod, Zeroable};
use glam::{Mat3, Mat4, Vec3};
use spall_mesh::{Mesh, Vertex as MeshVertex};

/// One vertex as uploaded to the GPU. `#[repr(C)]` and `Pod` so a slice casts
/// straight to bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct GpuVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub local_uv: [f32; 2],
    pub ao: f32,
    pub material: u32,
}

impl GpuVertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x3, // position
            1 => Float32x3, // normal
            2 => Float32x2, // local_uv
            3 => Float32,   // ao
            4 => Uint32,    // material
        ],
    };
}

/// Transform `mesh` by `model` and produce the GPU vertex / index buffers.
/// Normals are carried through the inverse-transpose so a rotated body still
/// lights correctly.
pub fn to_gpu(mesh: &Mesh, model: Mat4) -> (Vec<GpuVertex>, Vec<u32>) {
    let normal_mat = Mat3::from_mat4(model).inverse().transpose();
    let vertices = mesh
        .vertices
        .iter()
        .map(|v: &MeshVertex| {
            let p = model.transform_point3(Vec3::from_array(v.position));
            let n = (normal_mat * Vec3::from_array(v.normal)).normalize_or_zero();
            GpuVertex {
                position: p.to_array(),
                normal: n.to_array(),
                local_uv: v.local_uv,
                ao: v.ao,
                material: v.material,
            }
        })
        .collect();
    (vertices, mesh.indices.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_vertex_is_tightly_packed_at_40_bytes() {
        assert_eq!(std::mem::size_of::<GpuVertex>(), 40);
        assert_eq!(GpuVertex::LAYOUT.array_stride, 40);
    }

    #[test]
    fn identity_model_leaves_positions_and_normals_unchanged() {
        let mesh = Mesh {
            vertices: vec![MeshVertex {
                position: [1.0, 2.0, 3.0],
                normal: [0.0, 1.0, 0.0],
                material: 7,
                ao: 0.5,
                local_uv: [4.0, 5.0],
            }],
            indices: vec![],
        };
        let (gpu, _) = to_gpu(&mesh, Mat4::IDENTITY);
        assert_eq!(gpu[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(gpu[0].normal, [0.0, 1.0, 0.0]);
        assert_eq!(gpu[0].material, 7);
    }

    #[test]
    fn a_yaw_rotates_the_normal_into_world_space() {
        let mesh = Mesh {
            vertices: vec![MeshVertex {
                position: [0.0, 0.0, 0.0],
                normal: [1.0, 0.0, 0.0],
                material: 1,
                ao: 1.0,
                local_uv: [0.0, 0.0],
            }],
            indices: vec![],
        };
        let (gpu, _) = to_gpu(&mesh, Mat4::from_rotation_y(std::f32::consts::FRAC_PI_2));
        // +X rotated 90 deg about +Y goes to -Z.
        let n = gpu[0].normal;
        assert!((n[0]).abs() < 1e-5 && (n[2] + 1.0).abs() < 1e-5, "{n:?}");
    }
}
