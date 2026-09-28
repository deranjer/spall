//! Instanced axis-aligned cubes: the second geometry-submission path beside
//! greedy meshes. One shared unit-cube mesh is drawn per instance with a world
//! offset, per-axis size in metres, a rotation, and a material index into the
//! same table the mesh path uses, so both paths share `fs_main`, shadows and
//! the tone map. The interactive game draws terrain surface cells and detached
//! body cells this way; the editor viewport uses greedy meshes.

use bytemuck::{Pod, Zeroable};

/// One cube. 48 bytes; `#[repr(C)]` and `Pod` so a slice casts to bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct CubeInstance {
    /// Cube centre, world metres.
    pub offset: [f32; 3],
    /// Index into the material table (`MaterialId` for manifest materials).
    pub material: u32,
    /// Edge lengths in metres along the cube's own axes.
    pub size: [f32; 3],
    pub _pad: f32,
    /// Unit quaternion `[x, y, z, w]`, applied about the cube centre.
    pub rotation: [f32; 4],
}

impl CubeInstance {
    pub const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

    pub const fn new(offset: [f32; 3], material: u32, size: [f32; 3], rotation: [f32; 4]) -> Self {
        Self {
            offset,
            material,
            size,
            _pad: 0.0,
            rotation,
        }
    }

    /// Per-instance attributes, bound at vertex-buffer slot 1.
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<CubeInstance>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Instance,
        // Explicit offsets: `size` is followed by 4 bytes of padding, so the
        // sequential `vertex_attr_array!` offsets would misplace `rotation`.
        attributes: &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 3, // offset
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 12,
                shader_location: 4, // material
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 16,
                shader_location: 5, // size
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 32,
                shader_location: 6, // rotation
            },
        ],
    };
}

/// One vertex of the shared unit cube (edge 1, centred on the origin).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct CubeVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Face-local `0..1`, matching the mesh path's per-cell `local_uv`.
    pub local_uv: [f32; 2],
}

impl CubeVertex {
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<CubeVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x3, // position
            1 => Float32x3, // normal
            2 => Float32x2, // local_uv
        ],
    };
}

struct Face {
    normal: [f32; 3],
    u: [f32; 3],
    v: [f32; 3],
}

/// `u x v == normal` on every face, so the corner order below winds
/// counter-clockwise seen from outside.
const FACES: [Face; 6] = [
    Face {
        normal: [1.0, 0.0, 0.0],
        u: [0.0, 1.0, 0.0],
        v: [0.0, 0.0, 1.0],
    },
    Face {
        normal: [-1.0, 0.0, 0.0],
        u: [0.0, 0.0, 1.0],
        v: [0.0, 1.0, 0.0],
    },
    Face {
        normal: [0.0, 1.0, 0.0],
        u: [0.0, 0.0, 1.0],
        v: [1.0, 0.0, 0.0],
    },
    Face {
        normal: [0.0, -1.0, 0.0],
        u: [1.0, 0.0, 0.0],
        v: [0.0, 0.0, 1.0],
    },
    Face {
        normal: [0.0, 0.0, 1.0],
        u: [1.0, 0.0, 0.0],
        v: [0.0, 1.0, 0.0],
    },
    Face {
        normal: [0.0, 0.0, -1.0],
        u: [0.0, 1.0, 0.0],
        v: [1.0, 0.0, 0.0],
    },
];

/// The shared unit cube: 24 vertices, 36 indices.
pub fn unit_cube() -> (Vec<CubeVertex>, Vec<u16>) {
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for face in FACES {
        let base = vertices.len() as u16;
        for (su, sv) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let position = std::array::from_fn(|axis| {
                (face.normal[axis] + face.u[axis] * su + face.v[axis] * sv) * 0.5
            });
            vertices.push(CubeVertex {
                position,
                normal: face.normal,
                local_uv: [su * 0.5 + 0.5, sv * 0.5 + 0.5],
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (vertices, indices)
}

/// A resident, growable instance buffer. Contents are replaced with
/// [`Self::set`]; the GPU buffer is reused while the data fits, so a frame that
/// changes nothing uploads nothing and a frame that changes a few thousand
/// instances does not allocate.
#[derive(Default)]
pub struct InstanceSet {
    buffer: Option<wgpu::Buffer>,
    capacity: usize,
    count: u32,
}

impl InstanceSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Bytes currently allocated on the GPU for this set.
    pub fn allocated_bytes(&self) -> u64 {
        (self.capacity * std::mem::size_of::<CubeInstance>()) as u64
    }

    /// Replace the contents. Returns the bytes written (0 for an empty set).
    pub fn set(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        self.count = instances.len() as u32;
        if instances.is_empty() {
            return 0;
        }
        if self.buffer.is_none() || instances.len() > self.capacity {
            self.capacity = (instances.len() + instances.len() / 2).max(256);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-cube-instances"),
                size: self.allocated_bytes(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let bytes: &[u8] = bytemuck::cast_slice(instances);
        if let Some(buffer) = &self.buffer {
            queue.write_buffer(buffer, 0, bytes);
        }
        bytes.len() as u64
    }

    pub(crate) fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref().filter(|_| self.count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_and_vertex_layouts_match_their_rust_structs() {
        assert_eq!(std::mem::size_of::<CubeInstance>(), 48);
        assert_eq!(CubeInstance::LAYOUT.array_stride, 48);
        let offset_of = |location| {
            CubeInstance::LAYOUT
                .attributes
                .iter()
                .find(|a| a.shader_location == location)
                .map(|a| a.offset as usize)
        };
        assert_eq!(
            offset_of(3),
            Some(std::mem::offset_of!(CubeInstance, offset))
        );
        assert_eq!(
            offset_of(4),
            Some(std::mem::offset_of!(CubeInstance, material))
        );
        assert_eq!(offset_of(5), Some(std::mem::offset_of!(CubeInstance, size)));
        assert_eq!(
            offset_of(6),
            Some(std::mem::offset_of!(CubeInstance, rotation))
        );
        assert_eq!(std::mem::size_of::<CubeVertex>(), 32);
    }

    #[test]
    fn the_unit_cube_is_closed_outward_facing_and_unit_sized() {
        let (vertices, indices) = unit_cube();
        assert_eq!((vertices.len(), indices.len()), (24, 36));
        for v in &vertices {
            assert!(v.position.iter().all(|c| (c.abs() - 0.5).abs() < 1e-6));
        }
        // Every triangle's geometric normal agrees with its stored normal.
        for tri in indices.chunks_exact(3) {
            let p = |i: u16| glam::Vec3::from_array(vertices[usize::from(i)].position);
            let n = (p(tri[1]) - p(tri[0]))
                .cross(p(tri[2]) - p(tri[0]))
                .normalize();
            let stored = glam::Vec3::from_array(vertices[usize::from(tri[0])].normal);
            assert!(n.dot(stored) > 0.999, "inward-facing triangle {tri:?}");
        }
    }
}
