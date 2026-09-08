//! The single opaque render pipeline: one WGSL shader, an explicit depth
//! attachment, counter-clockwise front faces, back-face culling.

use bytemuck::{Pod, Zeroable};
use glam::Vec3;
use wgpu::util::DeviceExt;

use crate::camera::Camera;
use crate::vertex::GpuVertex;

/// Which image the fragment shader writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugView {
    /// Sun + sky + baked AO shading.
    Shaded,
    /// World-space normal, mapped to `0.5 * n + 0.5`.
    Normals,
    /// Linear eye-space depth, near = white.
    Depth,
}

impl DebugView {
    fn code(self) -> f32 {
        match self {
            DebugView::Shaded => 0.0,
            DebugView::Normals => 1.0,
            DebugView::Depth => 2.0,
        }
    }

    /// A stable file-name stem for captures.
    pub fn stem(self) -> &'static str {
        match self {
            DebugView::Shaded => "shaded",
            DebugView::Normals => "normals",
            DebugView::Depth => "depth",
        }
    }
}

/// The colour format offscreen captures render to.
pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// The depth format.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    /// World → eye space. Used by the depth debug view to report linear
    /// eye-space depth rather than radial distance from the camera.
    view: [[f32; 4]; 4],
    camera_pos: [f32; 4],
    sun_dir: [f32; 4],
    params: [f32; 4],
}

/// The opaque pipeline plus its per-frame globals bind group.
pub struct ScenePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    globals_buffer: wgpu::Buffer,
    sun_dir: Vec3,
}

impl ScenePipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spall-opaque-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/opaque.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-globals-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spall-opaque-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spall-opaque-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[GpuVertex::LAYOUT],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview: None,
            cache: None,
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            globals_buffer,
            sun_dir: Vec3::new(-0.4, -0.82, -0.4).normalize(),
        }
    }

    pub fn raw(&self) -> &wgpu::RenderPipeline {
        &self.pipeline
    }

    /// Build the palette storage buffer from linear RGB colours indexed by
    /// material id.
    pub fn palette_buffer(&self, device: &wgpu::Device, colors: &[[f32; 4]]) -> wgpu::Buffer {
        let fallback = [[0.5f32, 0.5, 0.5, 1.0]];
        let data: &[[f32; 4]] = if colors.is_empty() { &fallback } else { colors };
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-palette"),
            contents: bytemuck::cast_slice(data),
            usage: wgpu::BufferUsages::STORAGE,
        })
    }

    /// Upload this frame's globals and produce the bind group.
    pub fn frame_bind_group(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &Camera,
        view: DebugView,
        palette: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let globals = Globals {
            view_proj: camera.view_projection().to_cols_array_2d(),
            view: camera.view().to_cols_array_2d(),
            camera_pos: [camera.position.x, camera.position.y, camera.position.z, 0.0],
            sun_dir: [self.sun_dir.x, self.sun_dir.y, self.sun_dir.z, 0.0],
            params: [view.code(), camera.z_near, camera.z_far, 0.0],
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));

        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-globals-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: palette.as_entire_binding(),
                },
            ],
        })
    }
}

/// The direction the sun light travels (for tests / docs).
pub fn default_sun_dir() -> Vec3 {
    Vec3::new(-0.4, -0.82, -0.4).normalize()
}
