//! Explicit T12 render passes: cascaded sun shadows, HDR opaque shading, and
//! fixed-exposure tone mapping. No render graph is hidden behind this module.

use bytemuck::{Pod, Zeroable};
use glam::camera::rh;
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::camera::Camera;
use crate::scene::Material;
use crate::vertex::GpuVertex;

pub const CASCADE_COUNT: usize = 4;
pub const SHADOW_MAP_SIZE: u32 = 2048;
pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugView {
    Shaded,
    Albedo,
    Normals,
    Depth,
    ShadowCascades,
    Roughness,
}

impl DebugView {
    fn code(self) -> f32 {
        match self {
            Self::Shaded => 0.0,
            Self::Normals => 1.0,
            Self::Depth => 2.0,
            Self::Albedo => 3.0,
            Self::ShadowCascades => 4.0,
            Self::Roughness => 5.0,
        }
    }

    pub fn stem(self) -> &'static str {
        match self {
            Self::Shaded => "shaded",
            Self::Albedo => "albedo",
            Self::Normals => "normals",
            Self::Depth => "depth",
            Self::ShadowCascades => "shadow_cascades",
            Self::Roughness => "roughness",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PassTiming {
    pub shadow_millis: f64,
    pub opaque_millis: f64,
    pub tone_map_millis: f64,
}

impl PassTiming {
    pub fn total(self) -> f64 {
        self.shadow_millis + self.opaque_millis + self.tone_map_millis
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    view: [[f32; 4]; 4],
    light_view_proj: [[[f32; 4]; 4]; CASCADE_COUNT],
    camera_pos: [f32; 4],
    sun_dir: [f32; 4],
    cascade_splits: [f32; 4],
    params: [f32; 4],
    light: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ShadowGlobals {
    light_view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuMaterial {
    base_color: [f32; 4],
    params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ToneGlobals {
    exposure: f32,
    debug_passthrough: f32,
    _pad: [f32; 2],
}

pub struct ScenePipeline {
    opaque: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
    tone_map: wgpu::RenderPipeline,
    scene_layout: wgpu::BindGroupLayout,
    shadow_layout: wgpu::BindGroupLayout,
    tone_layout: wgpu::BindGroupLayout,
    globals_buffer: wgpu::Buffer,
    shadow_globals_buffers: Vec<wgpu::Buffer>,
    tone_globals_buffer: wgpu::Buffer,
    _shadow_texture: wgpu::Texture,
    shadow_sample_view: wgpu::TextureView,
    shadow_layer_views: Vec<wgpu::TextureView>,
    shadow_sampler: wgpu::Sampler,
    linear_sampler: wgpu::Sampler,
    sun_dir: Vec3,
}

impl ScenePipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let opaque_shader = shader(
            device,
            "spall-t12-opaque",
            include_str!("shaders/opaque.wgsl"),
        );
        let shadow_shader = shader(
            device,
            "spall-t12-shadow",
            include_str!("shaders/shadow.wgsl"),
        );
        let tone_shader = shader(
            device,
            "spall-t12-tone-map",
            include_str!("shaders/tonemap.wgsl"),
        );

        let scene_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-t12-scene-layout"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        let shadow_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-t12-shadow-layout"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX)],
        });
        let tone_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-t12-tone-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                uniform_entry(2, wgpu::ShaderStages::FRAGMENT),
            ],
        });

        let opaque = create_opaque_pipeline(device, &opaque_shader, &scene_layout);
        let shadow = create_shadow_pipeline(device, &shadow_shader, &shadow_layout);
        let tone_map = create_tone_pipeline(device, &tone_shader, &tone_layout);
        let globals_buffer = uniform_buffer::<Globals>(device, "spall-t12-globals");
        let shadow_globals_buffers = (0..CASCADE_COUNT)
            .map(|_| uniform_buffer::<ShadowGlobals>(device, "spall-shadow-globals"))
            .collect();
        let tone_globals_buffer = uniform_buffer::<ToneGlobals>(device, "spall-tone-globals");

        let shadow_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-sun-shadow-cascades"),
            size: wgpu::Extent3d {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
                depth_or_array_layers: CASCADE_COUNT as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_sample_view = shadow_texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("spall-shadow-array-view"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let shadow_layer_views = (0..CASCADE_COUNT)
            .map(|layer| {
                shadow_texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("spall-shadow-layer-view"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer as u32,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let shadow_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("spall-shadow-compare-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            compare: Some(wgpu::CompareFunction::LessEqual),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("spall-linear-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Self {
            opaque,
            shadow,
            tone_map,
            scene_layout,
            shadow_layout,
            tone_layout,
            globals_buffer,
            shadow_globals_buffers,
            tone_globals_buffer,
            _shadow_texture: shadow_texture,
            shadow_sample_view,
            shadow_layer_views,
            shadow_sampler,
            linear_sampler,
            sun_dir: default_sun_dir(),
        }
    }

    pub fn opaque(&self) -> &wgpu::RenderPipeline {
        &self.opaque
    }
    pub fn shadow(&self) -> &wgpu::RenderPipeline {
        &self.shadow
    }
    pub fn tone_map(&self) -> &wgpu::RenderPipeline {
        &self.tone_map
    }
    pub fn shadow_layer(&self, index: usize) -> &wgpu::TextureView {
        &self.shadow_layer_views[index]
    }

    pub fn material_buffer(&self, device: &wgpu::Device, materials: &[Material]) -> wgpu::Buffer {
        let fallback = [Material::new([0.5, 0.5, 0.5], 0.8, 0.0)];
        let source = if materials.is_empty() {
            &fallback[..]
        } else {
            materials
        };
        let gpu: Vec<GpuMaterial> = source
            .iter()
            .map(|m| GpuMaterial {
                base_color: [m.base_color[0], m.base_color[1], m.base_color[2], 1.0],
                params: [
                    m.roughness.clamp(0.04, 1.0),
                    m.metallic.clamp(0.0, 1.0),
                    0.0,
                    0.0,
                ],
            })
            .collect();
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-materials"),
            contents: bytemuck::cast_slice(&gpu),
            usage: wgpu::BufferUsages::STORAGE,
        })
    }

    pub fn cascade_data(camera: &Camera, sun_dir: Vec3) -> ([Mat4; CASCADE_COUNT], [f32; 4]) {
        cascade_data(camera, sun_dir)
    }

    pub fn scene_bind_group(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &Camera,
        view: DebugView,
        exposure: f32,
        materials: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let (light_view_proj, cascade_splits) = cascade_data(camera, self.sun_dir);
        let globals = Globals {
            view_proj: camera.view_projection().to_cols_array_2d(),
            view: camera.view().to_cols_array_2d(),
            light_view_proj: light_view_proj.map(|matrix| matrix.to_cols_array_2d()),
            camera_pos: camera.position.extend(0.0).to_array(),
            sun_dir: self.sun_dir.extend(0.0).to_array(),
            cascade_splits,
            params: [view.code(), camera.z_near, camera.z_far, exposure],
            light: [4.5, 0.13, 0.30, 0.0],
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-t12-scene-bind-group"),
            layout: &self.scene_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: materials.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.shadow_sample_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.shadow_sampler),
                },
            ],
        })
    }

    pub fn shadow_bind_group(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        cascade: usize,
        light_view_proj: Mat4,
    ) -> wgpu::BindGroup {
        let buffer = &self.shadow_globals_buffers[cascade];
        queue.write_buffer(
            buffer,
            0,
            bytemuck::bytes_of(&ShadowGlobals {
                light_view_proj: light_view_proj.to_cols_array_2d(),
            }),
        );
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-shadow-bind-group"),
            layout: &self.shadow_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        })
    }

    pub fn tone_bind_group(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        hdr: &wgpu::TextureView,
        exposure: f32,
        debug_passthrough: bool,
    ) -> wgpu::BindGroup {
        queue.write_buffer(
            &self.tone_globals_buffer,
            0,
            bytemuck::bytes_of(&ToneGlobals {
                exposure,
                debug_passthrough: u8::from(debug_passthrough) as f32,
                _pad: [0.0; 2],
            }),
        );
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-tone-bind-group"),
            layout: &self.tone_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(hdr),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.tone_globals_buffer.as_entire_binding(),
                },
            ],
        })
    }
}

fn shader(device: &wgpu::Device, label: &str, source: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    })
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn uniform_buffer<T>(device: &wgpu::Device, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: std::mem::size_of::<T>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_opaque_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-t12-opaque-layout"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-t12-opaque-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[GpuVertex::LAYOUT],
            compilation_options: Default::default(),
        },
        primitive: opaque_primitive(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::Less,
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: HDR_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview: None,
        cache: None,
    })
}

fn create_shadow_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-shadow-layout"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-shadow-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[GpuVertex::LAYOUT],
            compilation_options: Default::default(),
        },
        primitive: opaque_primitive(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: true,
            depth_compare: wgpu::CompareFunction::Less,
            stencil: Default::default(),
            bias: wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 2.0,
                clamp: 0.0,
            },
        }),
        multisample: Default::default(),
        fragment: None,
        multiview: None,
        cache: None,
    })
}

fn create_tone_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-tone-layout"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-tone-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: COLOR_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview: None,
        cache: None,
    })
}

fn opaque_primitive() -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        strip_index_format: None,
        front_face: wgpu::FrontFace::Ccw,
        cull_mode: Some(wgpu::Face::Back),
        unclipped_depth: false,
        polygon_mode: wgpu::PolygonMode::Fill,
        conservative: false,
    }
}

fn cascade_data(camera: &Camera, sun_dir: Vec3) -> ([Mat4; CASCADE_COUNT], [f32; 4]) {
    let near = camera.z_near.max(0.01);
    let far = camera.z_far.max(near + 0.01);
    let mut splits = [0.0; CASCADE_COUNT];
    for (i, split) in splits.iter_mut().enumerate() {
        let p = (i + 1) as f32 / CASCADE_COUNT as f32;
        let log = near * (far / near).powf(p);
        let uniform = near + (far - near) * p;
        *split = log * 0.65 + uniform * 0.35;
    }

    let mut matrices = [Mat4::IDENTITY; CASCADE_COUNT];
    let mut slice_near = near;
    for i in 0..CASCADE_COUNT {
        let slice_far = splits[i];
        let centre_depth = (slice_near + slice_far) * 0.5;
        let centre = camera.position + camera.forward() * centre_depth;
        let far_half_h = (camera.fov_y * 0.5).tan() * slice_far;
        let far_half_w = far_half_h * camera.aspect;
        let depth_radius = (slice_far - slice_near) * 0.5;
        let radius =
            (far_half_w * far_half_w + far_half_h * far_half_h + depth_radius * depth_radius)
                .sqrt()
                .max(1.0);
        let texel = (radius * 2.0) / SHADOW_MAP_SIZE as f32;
        let centre = (centre / texel).round() * texel;
        let light_eye = centre - sun_dir.normalize() * radius * 2.5;
        let up = if sun_dir.normalize().dot(Vec3::Y).abs() > 0.95 {
            Vec3::Z
        } else {
            Vec3::Y
        };
        let light_view = rh::view::look_at_mat4(light_eye, centre, up);
        let light_proj =
            rh::proj::directx::orthographic(-radius, radius, -radius, radius, 0.0, radius * 5.0);
        matrices[i] = light_proj * light_view;
        slice_near = slice_far;
    }
    (matrices, splits)
}

pub fn default_sun_dir() -> Vec3 {
    Vec3::new(-0.4, -0.82, -0.4).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_splits_are_ordered_and_cover_the_camera_range() {
        let camera = Camera {
            z_near: 0.25,
            z_far: 200.0,
            ..Default::default()
        };
        let (_, splits) = cascade_data(&camera, default_sun_dir());
        assert!(splits[0] > camera.z_near);
        assert!(splits.windows(2).all(|w| w[0] < w[1]));
        assert!((splits[3] - camera.z_far).abs() < 0.001);
    }

    #[test]
    fn cascade_matrices_are_finite_and_stable_for_sub_texel_camera_motion() {
        let camera = Camera {
            position: Vec3::new(3.0, 4.0, 8.0),
            z_far: 100.0,
            ..Default::default()
        };
        let (a, _) = cascade_data(&camera, default_sun_dir());
        assert!(a.iter().all(|m| m.is_finite()));
        let moved = Camera {
            position: camera.position + Vec3::splat(1.0e-5),
            ..camera
        };
        let (b, _) = cascade_data(&moved, default_sun_dir());
        assert!((a[0] - b[0]).abs_diff_eq(Mat4::ZERO, 1.0e-4));
    }

    #[test]
    fn material_gpu_layout_is_storage_buffer_safe() {
        assert_eq!(std::mem::size_of::<GpuMaterial>(), 32);
    }
}
