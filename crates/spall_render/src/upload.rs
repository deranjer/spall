//! Bounded, reusable GPU mesh buffers.
//!
//! A [`MeshUploader`] keeps one vertex and one index buffer and grows them only
//! when a mesh needs more room, never shrinking. An upload whose byte size
//! exceeds the configured budget is rejected instead of silently allocating.

use bytemuck::cast_slice;
use wgpu::util::DeviceExt;

use crate::context::RenderError;
use crate::vertex::GpuVertex;

/// Per-buffer upload ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadBudget {
    pub max_vertex_bytes: u64,
    pub max_index_bytes: u64,
}

impl Default for UploadBudget {
    /// 16 MiB of vertices and 8 MiB of indices — ample for the T05 fixtures,
    /// small enough to catch a runaway mesh.
    fn default() -> Self {
        Self {
            max_vertex_bytes: 16 << 20,
            max_index_bytes: 8 << 20,
        }
    }
}

/// A drawable mesh resident on the GPU.
pub struct GpuMesh {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
    pub vertex_bytes: u64,
    pub index_bytes: u64,
}

impl GpuMesh {
    /// Upload once, without buffer reuse. Enforces `budget`.
    pub fn create(
        device: &wgpu::Device,
        vertices: &[GpuVertex],
        indices: &[u32],
        budget: UploadBudget,
    ) -> Result<Self, RenderError> {
        let v_bytes = std::mem::size_of_val(vertices) as u64;
        let i_bytes = std::mem::size_of_val(indices) as u64;
        if v_bytes > budget.max_vertex_bytes {
            return Err(RenderError::UploadBudgetExceeded {
                bytes: v_bytes,
                budget: budget.max_vertex_bytes,
            });
        }
        if i_bytes > budget.max_index_bytes {
            return Err(RenderError::UploadBudgetExceeded {
                bytes: i_bytes,
                budget: budget.max_index_bytes,
            });
        }
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-mesh-vertices"),
            contents: cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-mesh-indices"),
            contents: cast_slice(indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        Ok(Self {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            vertex_bytes: v_bytes,
            index_bytes: i_bytes,
        })
    }
}

/// Owns reusable vertex/index buffers so repeated re-meshes of the same scene
/// do not reallocate.
pub struct MeshUploader {
    budget: UploadBudget,
    vertex_buffer: Option<wgpu::Buffer>,
    index_buffer: Option<wgpu::Buffer>,
    vertex_capacity: u64,
    index_capacity: u64,
    pub uploads: u64,
    pub reallocations: u64,
}

impl MeshUploader {
    pub fn new(budget: UploadBudget) -> Self {
        Self {
            budget,
            vertex_buffer: None,
            index_buffer: None,
            vertex_capacity: 0,
            index_capacity: 0,
            uploads: 0,
            reallocations: 0,
        }
    }

    /// Upload `vertices`/`indices`, reusing the existing buffers when they are
    /// already large enough.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[GpuVertex],
        indices: &[u32],
    ) -> Result<GpuDraw<'_>, RenderError> {
        let v_bytes = std::mem::size_of_val(vertices) as u64;
        let i_bytes = std::mem::size_of_val(indices) as u64;
        if v_bytes > self.budget.max_vertex_bytes {
            return Err(RenderError::UploadBudgetExceeded {
                bytes: v_bytes,
                budget: self.budget.max_vertex_bytes,
            });
        }
        if i_bytes > self.budget.max_index_bytes {
            return Err(RenderError::UploadBudgetExceeded {
                bytes: i_bytes,
                budget: self.budget.max_index_bytes,
            });
        }

        if self.vertex_buffer.is_none() || v_bytes > self.vertex_capacity {
            let cap = v_bytes.max(1).next_power_of_two().max(4096);
            self.vertex_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-mesh-vertices"),
                size: cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.vertex_capacity = cap;
            self.reallocations += 1;
        }
        if self.index_buffer.is_none() || i_bytes > self.index_capacity {
            let cap = i_bytes.max(1).next_power_of_two().max(4096);
            self.index_buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-mesh-indices"),
                size: cap,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.index_capacity = cap;
            self.reallocations += 1;
        }

        let vb = self.vertex_buffer.as_ref().expect("just set");
        let ib = self.index_buffer.as_ref().expect("just set");
        if v_bytes > 0 {
            queue.write_buffer(vb, 0, cast_slice(vertices));
        }
        if i_bytes > 0 {
            queue.write_buffer(ib, 0, cast_slice(indices));
        }
        self.uploads += 1;

        Ok(GpuDraw {
            vertex_buffer: vb,
            index_buffer: ib,
            index_count: indices.len() as u32,
        })
    }
}

/// A borrowed view of the uploader's buffers for one draw.
pub struct GpuDraw<'a> {
    pub vertex_buffer: &'a wgpu::Buffer,
    pub index_buffer: &'a wgpu::Buffer,
    pub index_count: u32,
}
