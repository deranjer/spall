//! Explicit T12 render passes: cascaded sun shadows, HDR opaque shading, and
//! fixed-exposure tone mapping. No render graph is hidden behind this module.

use bytemuck::{Pod, Zeroable};
use glam::camera::rh;
use glam::{Mat4, Vec3};
use wgpu::util::DeviceExt;

use crate::camera::Camera;
use crate::environment::Environment;
use crate::indirect::{IndirectPipeline, IndirectResources};
use crate::instances::{CubeInstance, CubeVertex};
use crate::scene::Material;
use crate::sky_visibility::{SkyPipeline, SkyVisibility};
use crate::vertex::GpuVertex;
use crate::water::WaterField;

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
    /// T13 irradiance only: direct sun and T12 ambient are disabled.
    IndirectOnly,
    /// Sun shadow visibility alone (`1` lit, `0` shadowed), after PCSS.
    ShadowVisibility,
    /// Sky visibility alone (`1` open sky, `0` enclosed) at each surface.
    SkyVisibility,
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
            Self::IndirectOnly => 6.0,
            Self::ShadowVisibility => 7.0,
            Self::SkyVisibility => 8.0,
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
            Self::IndirectOnly => "indirect_only",
            Self::ShadowVisibility => "shadow_visibility",
            Self::SkyVisibility => "sky_visibility",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PassTiming {
    pub indirect_trace_millis: f64,
    pub indirect_denoise_millis: f64,
    pub shadow_millis: f64,
    pub opaque_millis: f64,
    pub tone_map_millis: f64,
}

impl PassTiming {
    pub fn total(self) -> f64 {
        self.indirect_trace_millis
            + self.indirect_denoise_millis
            + self.shadow_millis
            + self.opaque_millis
            + self.tone_map_millis
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
    /// Sun tint in `rgb`, intensity in `w`.
    sun: [f32; 4],
    sky_color: [f32; 4],
    ground_color: [f32; 4],
    moon_dir: [f32; 4],
    moon: [f32; 4],
    moon_phase: [f32; 4],
    /// Point lights: position in `xyz`, range in `w`.
    point_lights: [[f32; 4]; MAX_POINT_LIGHTS],
    /// Point light colour times intensity in `rgb`.
    point_colors: [[f32; 4]; MAX_POINT_LIGHTS],
    /// Light count in `x`.
    point_count: [f32; 4],
    /// `x`: 1 when the camera is under the water surface; `y`: that surface's
    /// height; `z`: water animation time in seconds; `w`: 1 when a water field
    /// is bound at all.
    water_cam: [f32; 4],
    /// The water field's `xy` origin, cell size `z` and cells per side `w`.
    water_field: [f32; 4],
    /// Distance fog colour (linear, the sky's horizon colour).
    fog_color: [f32; 4],
    /// Distance fog: `x` where it starts and `y` where it is complete, in
    /// metres of horizontal distance from the camera; `y <= x` turns it off.
    fog_range: [f32; 4],
}

/// Most point lights the opaque pass shades with at once (the nearest ones).
pub const MAX_POINT_LIGHTS: usize = 8;

/// A real-time light for the opaque pass: no shadows, smooth falloff to zero at
/// `radius` metres. Independent of the lighting cache, which cannot resolve an
/// emitter smaller than about a metre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointLight {
    pub position: [f32; 3],
    /// Linear colour times intensity.
    pub color: [f32; 3],
    pub radius: f32,
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

fn gpu_materials(materials: &[Material]) -> Vec<GpuMaterial> {
    let fallback = [Material::new([0.5, 0.5, 0.5], 0.8, 0.0)];
    let source = if materials.is_empty() {
        &fallback[..]
    } else {
        materials
    };
    source
        .iter()
        .map(|m| GpuMaterial {
            base_color: [m.base_color[0], m.base_color[1], m.base_color[2], m.opacity],
            params: [
                m.roughness.clamp(0.04, 1.0),
                m.metallic.clamp(0.0, 1.0),
                m.emissive.max(0.0),
                m.jitter.clamp(0.0, 0.5),
            ],
        })
        .collect()
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ToneGlobals {
    exposure: f32,
    debug_passthrough: f32,
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SkyboxGlobals {
    right: [f32; 4],
    up: [f32; 4],
    forward: [f32; 4],
    sun_direction: [f32; 4],
    moon_direction: [f32; 4],
    horizon: [f32; 4],
    zenith: [f32; 4],
    cloud: [f32; 4],
    params: [f32; 4],
    moon_color: [f32; 4],
    moon_params: [f32; 4],
}

/// The water height field as last supplied, plus its GPU copy.
#[derive(Default)]
struct WaterState {
    field: Option<WaterField>,
    gpu: Option<(wgpu::Texture, wgpu::TextureView)>,
    time_s: f32,
    /// A replacement field being uploaded a slice per frame (see
    /// [`ScenePipeline::replace_water_field`]); the field above keeps drawing
    /// until it is complete.
    staged: Option<StagedWaterField>,
    /// The texture of the field last replaced, kept for the next replacement.
    spare: Option<(wgpu::Texture, wgpu::TextureView)>,
}

struct StagedWaterField {
    field: WaterField,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Rows uploaded so far.
    next_row: u32,
}

pub struct ScenePipeline {
    skybox: wgpu::RenderPipeline,
    skybox_buffer: wgpu::Buffer,
    skybox_bind: wgpu::BindGroup,
    opaque: wgpu::RenderPipeline,
    shadow: wgpu::RenderPipeline,
    opaque_cube: wgpu::RenderPipeline,
    opaque_terrain: wgpu::RenderPipeline,
    transparent_cube: wgpu::RenderPipeline,
    water_surface: wgpu::RenderPipeline,
    water_grid: wgpu::RenderPipeline,
    shadow_cube: wgpu::RenderPipeline,
    tone_map: wgpu::RenderPipeline,
    scene_layout: wgpu::BindGroupLayout,
    water: std::sync::Mutex<WaterState>,
    /// 1x1 "no water anywhere" height field bound until a real one is set.
    _water_none_texture: wgpu::Texture,
    water_none_view: wgpu::TextureView,
    shadow_layout: wgpu::BindGroupLayout,
    tone_layout: wgpu::BindGroupLayout,
    globals_buffer: wgpu::Buffer,
    point_lights: std::sync::Mutex<Vec<PointLight>>,
    /// Distance fog start and end (metres); `[0, 0]` is off.
    fog_range: std::sync::Mutex<[f32; 2]>,
    /// 0..=1: how far the fog colour is pulled from the sky horizon to a bright grey-white.
    fog_whiteout: std::sync::Mutex<f32>,
    shadow_globals_buffers: Vec<wgpu::Buffer>,
    tone_globals_buffer: wgpu::Buffer,
    shadow_texture: wgpu::Texture,
    /// The terrain-only depth of each cascade, kept between frames and copied
    /// into `shadow_texture` before the moving casters are drawn on top.
    shadow_static_texture: wgpu::Texture,
    shadow_static_layer_views: Vec<wgpu::TextureView>,
    /// `shadow_static_texture` with the foliage drawn over it: the terrain's
    /// depth is copied in and the foliage rasterised on top only when either
    /// changes, and this is what is copied into `shadow_texture` each frame.
    shadow_merged_texture: wgpu::Texture,
    shadow_merged_layer_views: Vec<wgpu::TextureView>,
    shadow_sample_view: wgpu::TextureView,
    shadow_layer_views: Vec<wgpu::TextureView>,
    shadow_sampler: wgpu::Sampler,
    linear_sampler: wgpu::Sampler,
    indirect: IndirectPipeline,
    sky: SkyPipeline,
    /// Bound wherever no sky occupancy exists; the shader then uses the
    /// legacy hemispheric ambient.
    sky_disabled: wgpu::BindGroup,
    _sky_disabled_buffers: Vec<wgpu::Buffer>,
    sun_dir: Vec3,
}

impl ScenePipeline {
    /// Sets the point lights the next frames are shaded with (the first
    /// [`MAX_POINT_LIGHTS`] are used; pass the nearest ones).
    /// Fades everything between `start_m` and `end_m` (horizontal metres from
    /// the camera) into the sky's horizon colour, so terrain that is not drawn
    /// past `end_m` does not pop in at the edge. `end_m <= start_m` turns it off.
    pub fn set_fog_range(&self, start_m: f32, end_m: f32) {
        *self.fog_range.lock().unwrap_or_else(|e| e.into_inner()) = [start_m, end_m];
    }

    /// Pulls the fog colour toward a bright, desaturated white (`0` leaves the sky's
    /// horizon colour): the blizzard whiteout.
    pub fn set_fog_whiteout(&self, amount: f32) {
        *self.fog_whiteout.lock().unwrap_or_else(|e| e.into_inner()) = amount.clamp(0.0, 1.0);
    }

    pub fn set_point_lights(&self, lights: &[PointLight]) {
        let mut slot = self.point_lights.lock().unwrap_or_else(|e| e.into_inner());
        slot.clear();
        slot.extend(lights.iter().take(MAX_POINT_LIGHTS).copied());
    }

    pub fn new(device: &wgpu::Device) -> Self {
        Self::new_for_output(device, COLOR_FORMAT)
    }

    /// As [`Self::new`], with the tone-map pass targeting `output_format`
    /// (a swapchain format for a host that tone-maps straight to the window).
    /// The format must be an sRGB one: the tone map writes linear values and
    /// relies on the target's hardware encode, exactly once.
    pub fn new_for_output(device: &wgpu::Device, output_format: wgpu::TextureFormat) -> Self {
        assert!(
            output_format.is_srgb(),
            "the tone map needs an sRGB output format, got {output_format:?}"
        );
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
        let skybox_shader = shader(
            device,
            "spall-voxel-skybox",
            include_str!("shaders/skybox.wgsl"),
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
                // Water surface heights, read with `textureLoad` only (by the
                // grid surface's vertex stage as well as every fragment stage).
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
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
        let skybox_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-voxel-skybox-layout"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT)],
        });

        crate::probe::mark(
            "ScenePipeline::new: IndirectPipeline::new (3x vkCreateComputePipelines)",
        );
        let indirect = IndirectPipeline::new(device);
        let sky = SkyPipeline::new(device);
        let (sky_disabled, sky_disabled_buffers) = sky.disabled(device);
        crate::probe::mark("ScenePipeline::new: vkCreateGraphicsPipelines(opaque)");
        let opaque = create_opaque_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
            "vs_main",
            &[Some(GpuVertex::LAYOUT)],
        );
        // Terrain meshes: the mesh layout with the static-terrain vertex entry.
        let opaque_terrain = create_opaque_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
            "vs_terrain",
            &[Some(GpuVertex::LAYOUT)],
        );
        let opaque_cube = create_opaque_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
            "vs_cube",
            &[Some(CubeVertex::LAYOUT), Some(CubeInstance::LAYOUT)],
        );
        let transparent_cube = create_transparent_cube_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
        );
        let water_surface = create_water_surface_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
        );
        let water_grid = create_water_grid_pipeline(
            device,
            &opaque_shader,
            &scene_layout,
            indirect.display_layout(),
            sky.display_layout(),
        );
        crate::probe::mark("ScenePipeline::new: vkCreateGraphicsPipelines(shadow)");
        let shadow = create_shadow_pipeline(
            device,
            &shadow_shader,
            &shadow_layout,
            "vs_main",
            &[Some(GpuVertex::LAYOUT)],
        );
        let shadow_cube = create_shadow_pipeline(
            device,
            &shadow_shader,
            &shadow_layout,
            "vs_cube",
            &[Some(CubeVertex::LAYOUT), Some(CubeInstance::LAYOUT)],
        );
        crate::probe::mark("ScenePipeline::new: vkCreateGraphicsPipelines(tone_map)");
        let tone_map = create_tone_pipeline(device, &tone_shader, &tone_layout, output_format);
        let skybox = create_skybox_pipeline(device, &skybox_shader, &skybox_layout);
        crate::probe::mark("ScenePipeline::new: all pipelines OK (3 compute + 9 graphics)");
        let globals_buffer = uniform_buffer::<Globals>(device, "spall-t12-globals");
        let shadow_globals_buffers = (0..CASCADE_COUNT)
            .map(|_| uniform_buffer::<ShadowGlobals>(device, "spall-shadow-globals"))
            .collect();
        let tone_globals_buffer = uniform_buffer::<ToneGlobals>(device, "spall-tone-globals");
        let skybox_buffer = uniform_buffer::<SkyboxGlobals>(device, "spall-voxel-skybox-globals");
        let skybox_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-voxel-skybox-bind"),
            layout: &skybox_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: skybox_buffer.as_entire_binding(),
            }],
        });

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
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let shadow_static_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-sun-shadow-static-cascades"),
            size: wgpu::Extent3d {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
                depth_or_array_layers: CASCADE_COUNT as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let shadow_merged_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-sun-shadow-merged-cascades"),
            size: wgpu::Extent3d {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
                depth_or_array_layers: CASCADE_COUNT as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let shadow_merged_layer_views = (0..CASCADE_COUNT)
            .map(|layer| {
                shadow_merged_texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("spall-shadow-merged-layer-view"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer as u32,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let shadow_static_layer_views = (0..CASCADE_COUNT)
            .map(|layer| {
                shadow_static_texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("spall-shadow-static-layer-view"),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    base_array_layer: layer as u32,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
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

        let water_none_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-water-none"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let water_none_view = water_none_texture.create_view(&Default::default());

        Self {
            skybox,
            skybox_buffer,
            skybox_bind,
            opaque,
            shadow,
            opaque_cube,
            opaque_terrain,
            transparent_cube,
            water_surface,
            water_grid,
            shadow_cube,
            tone_map,
            scene_layout,
            water: std::sync::Mutex::new(WaterState::default()),
            _water_none_texture: water_none_texture,
            water_none_view,
            shadow_layout,
            tone_layout,
            globals_buffer,
            point_lights: std::sync::Mutex::new(Vec::new()),
            fog_range: std::sync::Mutex::new([0.0, 0.0]),
            fog_whiteout: std::sync::Mutex::new(0.0),
            shadow_globals_buffers,
            tone_globals_buffer,
            shadow_texture,
            shadow_static_texture,
            shadow_static_layer_views,
            shadow_merged_texture,
            shadow_merged_layer_views,
            shadow_sample_view,
            shadow_layer_views,
            shadow_sampler,
            linear_sampler,
            indirect,
            sky,
            sky_disabled,
            _sky_disabled_buffers: sky_disabled_buffers,
            sun_dir: default_sun_dir(),
        }
    }

    pub fn opaque(&self) -> &wgpu::RenderPipeline {
        &self.opaque
    }
    pub fn shadow(&self) -> &wgpu::RenderPipeline {
        &self.shadow
    }
    /// The bind group (`@group(2)`) that disables sky visibility.
    pub fn sky_disabled_bind(&self) -> &wgpu::BindGroup {
        &self.sky_disabled
    }
    pub(crate) fn sky_pipeline(&self) -> &SkyPipeline {
        &self.sky
    }
    /// A sky-visibility resource set for one camera-local cache.
    pub fn create_sky_visibility(
        &self,
        device: &wgpu::Device,
        materials: &wgpu::Buffer,
    ) -> SkyVisibility {
        self.sky.create(device, materials)
    }
    /// [`Self::opaque`] for static terrain meshes: per-voxel colour jitter on.
    pub fn opaque_terrain(&self) -> &wgpu::RenderPipeline {
        &self.opaque_terrain
    }
    /// Instanced-cube variant of [`Self::opaque`] (same bind groups).
    pub fn opaque_cube(&self) -> &wgpu::RenderPipeline {
        &self.opaque_cube
    }
    /// Alpha-blended cube variant. Draw back-to-front after opaque geometry.
    pub fn transparent_cube(&self) -> &wgpu::RenderPipeline {
        &self.transparent_cube
    }
    /// Instanced-cube variant of [`Self::shadow`] (same bind group).
    pub fn shadow_cube(&self) -> &wgpu::RenderPipeline {
        &self.shadow_cube
    }
    pub fn tone_map(&self) -> &wgpu::RenderPipeline {
        &self.tone_map
    }

    pub(crate) fn skybox_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.skybox
    }

    pub(crate) fn skybox_bind_group(
        &self,
        queue: &wgpu::Queue,
        camera: &Camera,
        environment: &Environment,
    ) -> &wgpu::BindGroup {
        let [hr, hg, hb, _] = environment.background_linear().map(|v| v as f32);
        let globals = SkyboxGlobals {
            right: camera.right().extend(0.0).to_array(),
            up: camera.up().extend(0.0).to_array(),
            forward: camera.forward().extend(0.0).to_array(),
            // Environment sun_dir points toward the scene; the visible disc is
            // in the opposite direction.
            sun_direction: (-environment.sun_dir).extend(0.0).to_array(),
            moon_direction: (-environment.moon_dir).extend(0.0).to_array(),
            horizon: [hr, hg, hb, 1.0],
            zenith: [hr * 0.72, hg * 0.9, hb * 1.05, 1.0],
            cloud: {
                let brightness = if environment.sun_intensity < 1.0 {
                    0.16
                } else if environment.sun_intensity < 3.0 {
                    0.68
                } else {
                    0.9
                };
                [brightness, brightness * 1.02, brightness * 1.06, 1.0]
            },
            params: [
                (camera.fov_y * 0.5).tan(),
                camera.aspect,
                environment.sun_angular_diameter_deg.to_radians() * 0.5,
                environment.sun_visibility,
            ],
            moon_color: [
                environment.moon_color[0],
                environment.moon_color[1],
                environment.moon_color[2],
                1.0,
            ],
            moon_params: [
                0.52_f32.to_radians() * 0.5,
                environment.moon_phase,
                environment.moon_visibility,
                environment.moon_intensity,
            ],
        };
        queue.write_buffer(&self.skybox_buffer, 0, bytemuck::bytes_of(&globals));
        &self.skybox_bind
    }
    pub fn shadow_layer(&self, index: usize) -> &wgpu::TextureView {
        &self.shadow_layer_views[index]
    }

    /// The retained terrain-only depth layer of a cascade.
    pub fn shadow_static_layer(&self, index: usize) -> &wgpu::TextureView {
        &self.shadow_static_layer_views[index]
    }

    /// The retained terrain-plus-foliage depth layer of a cascade.
    pub fn shadow_merged_layer(&self, index: usize) -> &wgpu::TextureView {
        &self.shadow_merged_layer_views[index]
    }

    fn copy_depth_layer(
        encoder: &mut wgpu::CommandEncoder,
        from: &wgpu::Texture,
        to: &wgpu::Texture,
        cascade: usize,
    ) {
        let layer = |texture| wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: cascade as u32,
            },
            aspect: wgpu::TextureAspect::DepthOnly,
        };
        encoder.copy_texture_to_texture(
            layer(from),
            layer(to),
            wgpu::Extent3d {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Copies a cascade's retained terrain depth into its merged layer, ready
    /// for the foliage to be drawn over it.
    pub fn copy_static_to_merged(&self, encoder: &mut wgpu::CommandEncoder, cascade: usize) {
        Self::copy_depth_layer(
            encoder,
            &self.shadow_static_texture,
            &self.shadow_merged_texture,
            cascade,
        );
    }

    /// Copies a cascade's retained terrain and foliage depth into the layer the
    /// scene samples, ready for the moving casters to be drawn over it.
    pub fn copy_static_shadow(&self, encoder: &mut wgpu::CommandEncoder, cascade: usize) {
        Self::copy_depth_layer(
            encoder,
            &self.shadow_merged_texture,
            &self.shadow_texture,
            cascade,
        );
    }

    pub(crate) fn indirect_resources(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        volume: Option<&crate::indirect::LightingVolume>,
        materials: &wgpu::Buffer,
    ) -> IndirectResources {
        self.indirect.resources(device, queue, volume, materials)
    }

    #[cfg_attr(not(feature = "capture"), allow(dead_code))]
    pub(crate) fn dispatch_indirect<'a>(
        &'a self,
        encoder: &'a mut wgpu::CommandEncoder,
        resources: &'a IndirectResources,
        trace_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
        denoise_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
    ) {
        self.indirect
            .dispatch(encoder, resources, trace_timestamps, denoise_timestamps);
    }

    #[cfg_attr(not(feature = "capture"), allow(dead_code))]
    pub(crate) fn dispatch_indirect_temporal<'a>(
        &'a self,
        encoder: &'a mut wgpu::CommandEncoder,
        resources: &'a IndirectResources,
        timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
    ) {
        self.indirect
            .dispatch_temporal(encoder, resources, timestamps);
    }

    pub fn material_buffer(&self, device: &wgpu::Device, materials: &[Material]) -> wgpu::Buffer {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-materials"),
            contents: bytemuck::cast_slice(&gpu_materials(materials)),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        })
    }

    /// Overwrites a buffer from [`Self::material_buffer`] in place. `materials`
    /// must have the length the buffer was created with.
    pub fn write_materials(
        &self,
        queue: &wgpu::Queue,
        buffer: &wgpu::Buffer,
        materials: &[Material],
    ) {
        queue.write_buffer(buffer, 0, bytemuck::cast_slice(&gpu_materials(materials)));
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
        let environment = Environment {
            sun_dir: self.sun_dir,
            exposure,
            ..Environment::default()
        };
        self.scene_bind_group_lit(device, queue, camera, view, &environment, materials)
    }

    /// [`Self::scene_bind_group`] lit by `environment` (its sun direction,
    /// tints and ambient) instead of the fixed default. The caller must use
    /// the same `environment.sun_dir` for the shadow cascades.
    pub fn scene_bind_group_lit(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        camera: &Camera,
        view: DebugView,
        environment: &Environment,
        materials: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let (light_view_proj, cascade_splits) = cascade_data(camera, environment.sun_dir);
        let [sr, sg, sb] = environment.sun_color;
        let [kr, kg, kb] = environment.sky;
        let [gr, gg, gb] = environment.ground;
        let mut point_lights = [[0.0; 4]; MAX_POINT_LIGHTS];
        let mut point_colors = [[0.0; 4]; MAX_POINT_LIGHTS];
        let lights = self.point_lights.lock().unwrap_or_else(|e| e.into_inner());
        for (i, light) in lights.iter().enumerate() {
            point_lights[i] = [
                light.position[0],
                light.position[1],
                light.position[2],
                light.radius,
            ];
            point_colors[i] = [light.color[0], light.color[1], light.color[2], 0.0];
        }
        let point_count = [lights.len() as f32, 0.0, 0.0, 0.0];
        drop(lights);
        let water = self.water.lock().unwrap_or_else(|e| e.into_inner());
        let (water_cam, water_field) = match (&water.field, &water.gpu) {
            (Some(field), Some(_)) => {
                let eye = camera.position.to_array();
                let surface = field.submerged_surface(eye);
                (
                    [
                        f32::from(u8::from(surface.is_some())),
                        surface.unwrap_or(0.0),
                        water.time_s,
                        1.0,
                    ],
                    [
                        field.origin_xz[0],
                        field.origin_xz[1],
                        field.cell_m,
                        field.dim as f32,
                    ],
                )
            }
            _ => ([0.0; 4], [0.0; 4]),
        };
        let globals = Globals {
            view_proj: camera.view_projection().to_cols_array_2d(),
            view: camera.view().to_cols_array_2d(),
            light_view_proj: light_view_proj.map(|matrix| matrix.to_cols_array_2d()),
            camera_pos: camera.position.extend(0.0).to_array(),
            // `w`: tan of the sun's half angular diameter, the PCSS light size.
            sun_dir: environment
                .sun_dir
                .extend((environment.sun_angular_diameter_deg.to_radians() * 0.5).tan())
                .to_array(),
            cascade_splits,
            params: [
                view.code(),
                camera.z_near,
                camera.z_far,
                environment.exposure,
            ],
            sun: [sr, sg, sb, environment.sun_intensity],
            sky_color: [kr, kg, kb, 0.0],
            ground_color: [gr, gg, gb, 0.0],
            moon_dir: environment.moon_dir.extend(0.0).to_array(),
            moon: [
                environment.moon_color[0],
                environment.moon_color[1],
                environment.moon_color[2],
                environment.moon_intensity,
            ],
            moon_phase: [environment.moon_phase, 0.0, 0.0, 0.0],
            point_lights,
            point_colors,
            point_count,
            water_cam,
            water_field,
            fog_color: {
                let [r, g, b, _] = environment.background_linear().map(|v| v as f32);
                // What the sky shader draws at the horizon.
                let horizon = [r * 0.96, g * 0.96, b * 0.96];
                let whiteout = *self.fog_whiteout.lock().unwrap_or_else(|e| e.into_inner());
                let luma = horizon[0] * 0.2126 + horizon[1] * 0.7152 + horizon[2] * 0.0722;
                let [r, g, b] = horizon.map(|c| {
                    let white = (c * 0.2 + luma * 0.8) * 1.25;
                    c + (white - c) * whiteout
                });
                [r, g, b, 0.0]
            },
            fog_range: {
                let [start, end] = *self.fog_range.lock().unwrap_or_else(|e| e.into_inner());
                [start, end, 0.0, 0.0]
            },
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
        let water_view = water
            .gpu
            .as_ref()
            .map_or(&self.water_none_view, |(_, view)| view);
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
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(water_view),
                },
            ],
        })
    }

    /// Supplies (or clears, with `None`) the water surface heights the opaque
    /// shader uses for underwater fog, tint and caustics. The field is
    /// presentation-only client data; it never feeds back into simulation.
    pub fn set_water_field(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        field: Option<WaterField>,
    ) {
        let mut state = self.water.lock().unwrap_or_else(|e| e.into_inner());
        let Some(field) = field else {
            state.field = None;
            state.gpu = None;
            return;
        };
        let size = wgpu::Extent3d {
            width: field.dim,
            height: field.dim,
            depth_or_array_layers: 1,
        };
        let reusable = state
            .gpu
            .as_ref()
            .is_some_and(|(texture, _)| texture.width() == field.dim);
        if !reusable {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("spall-water-field"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            state.gpu = Some((texture, view));
        }
        let bounds = field.texel_rect(0, 0, field.dim, field.dim);
        if let Some((texture, _)) = &state.gpu {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&bounds),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(field.dim * 16),
                    rows_per_image: Some(field.dim),
                },
                size,
            );
        }
        state.field = Some(field);
    }

    /// Replaces the installed water field with `field` without a stall: the new
    /// field is uploaded to a second texture a few rows a frame by
    /// [`Self::advance_water_upload`], and the old one keeps drawing until the
    /// last row is in, when they swap. Rectangle updates in the meantime are for
    /// the new field. With no field installed yet, or one of another size, the
    /// replacement is immediate (as [`Self::set_water_field`]).
    pub fn replace_water_field(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        field: WaterField,
    ) {
        let immediate = {
            let state = self.water.lock().unwrap_or_else(|e| e.into_inner());
            state.gpu.is_none() || state.field.as_ref().is_none_or(|f| f.dim != field.dim)
        };
        if immediate {
            self.set_water_field(device, queue, Some(field));
            return;
        }
        let mut state = self.water.lock().unwrap_or_else(|e| e.into_inner());
        // A staging still under way is dropped: its texture is reused.
        let reuse = state
            .staged
            .take()
            .map(|s| (s.texture, s.view))
            .or_else(|| state.spare.take())
            .filter(|(texture, _)| texture.width() == field.dim);
        let (texture, view) = reuse.unwrap_or_else(|| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("spall-water-field-staged"),
                size: wgpu::Extent3d {
                    width: field.dim,
                    height: field.dim,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            (texture, view)
        });
        state.staged = Some(StagedWaterField {
            field,
            texture,
            view,
            next_row: 0,
        });
    }

    /// Uploads up to `rows` more rows of a field being replaced (see
    /// [`Self::replace_water_field`]) and swaps it in when it is complete.
    /// Returns whether a replacement is still under way.
    pub fn advance_water_upload(&self, queue: &wgpu::Queue, rows: u32) -> bool {
        let mut guard = self.water.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        let Some(staged) = state.staged.as_mut() else {
            return false;
        };
        let dim = staged.field.dim;
        let count = rows.min(dim - staged.next_row);
        if count > 0 {
            let texels = staged.field.texel_rect(0, staged.next_row, dim, count);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &staged.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: staged.next_row,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&texels),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(dim * 16),
                    rows_per_image: Some(count),
                },
                wgpu::Extent3d {
                    width: dim,
                    height: count,
                    depth_or_array_layers: 1,
                },
            );
            staged.next_row += count;
        }
        if staged.next_row < dim {
            return true;
        }
        let done = state.staged.take().expect("checked above");
        state.spare = state.gpu.take();
        state.gpu = Some((done.texture, done.view));
        state.field = Some(done.field);
        false
    }

    /// Replaces a rectangle of the water field on the GPU and in the
    /// renderer's copy (which answers "is the camera under water"). `texels`
    /// are `(top, bottom, smooth, agitation + flow)` row by row (see
    /// [`crate::water::pack_agitation_flow`]), `w * h` of them. A
    /// rectangle outside the installed field is ignored.
    pub fn update_water_field_rect(
        &self,
        queue: &wgpu::Queue,
        x0: u32,
        z0: u32,
        w: u32,
        h: u32,
        texels: &[[f32; 4]],
    ) {
        let mut guard = self.water.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        // While a replacement is being uploaded the rectangles are for it: its
        // rows not yet uploaded read this copy, the rest are written now.
        let (field, texture) = if let Some(staged) = state.staged.as_mut() {
            (&mut staged.field, &staged.texture)
        } else if let (Some(field), Some((texture, _))) = (state.field.as_mut(), state.gpu.as_ref())
        {
            (field, texture)
        } else {
            return;
        };
        if w == 0
            || h == 0
            || texels.len() != w as usize * h as usize
            || x0.checked_add(w).is_none_or(|e| e > field.dim)
            || z0.checked_add(h).is_none_or(|e| e > field.dim)
        {
            return;
        }
        let dim = field.dim as usize;
        for (row, line) in texels.chunks_exact(w as usize).enumerate() {
            for (col, t) in line.iter().enumerate() {
                let i = (x0 as usize + col) + dim * (z0 as usize + row);
                field.heights[i] = t[0];
                field.bottoms[i] = t[1];
                field.smooth[i] = t[2];
                (field.agitation[i], field.flow[i]) = crate::water::unpack_agitation_flow(t[3]);
            }
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: x0, y: z0, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(texels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 16),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// World origin (x, z, metres) of the installed water field, if any.
    pub fn water_field_origin(&self) -> Option<[f32; 2]> {
        self.water
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .field
            .as_ref()
            .map(|f| f.origin_xz)
    }

    /// Cells per side of the installed water field, 0 if none.
    pub fn water_field_dim(&self) -> u32 {
        self.water
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .field
            .as_ref()
            .map_or(0, |f| f.dim)
    }

    /// Alpha-blended water grid surface (no vertex buffers: the vertex stage
    /// builds the grid from the water field).
    pub fn water_grid(&self) -> &wgpu::RenderPipeline {
        &self.water_grid
    }

    /// Animation clock for caustics, in seconds.
    pub fn set_water_time(&self, seconds: f32) {
        self.water.lock().unwrap_or_else(|e| e.into_inner()).time_s = seconds;
    }

    /// Alpha-blended water surface sheet (mesh vertex layout, no culling).
    pub fn water_surface(&self) -> &wgpu::RenderPipeline {
        &self.water_surface
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
    indirect_layout: &wgpu::BindGroupLayout,
    sky_layout: &wgpu::BindGroupLayout,
    vertex_entry: &str,
    vertex_buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-t12-opaque-layout"),
        bind_group_layouts: &[Some(layout), Some(indirect_layout), Some(sky_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-t12-opaque-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vertex_entry),
            buffers: vertex_buffers,
            compilation_options: Default::default(),
        },
        primitive: opaque_primitive(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
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
        multiview_mask: None,
        cache: None,
    })
}

fn create_transparent_cube_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    indirect_layout: &wgpu::BindGroupLayout,
    sky_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-debug-transparent-cube-layout"),
        bind_group_layouts: &[Some(layout), Some(indirect_layout), Some(sky_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-debug-transparent-cube-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_cube"),
            buffers: &[Some(CubeVertex::LAYOUT), Some(CubeInstance::LAYOUT)],
            compilation_options: Default::default(),
        },
        primitive: opaque_primitive(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: HDR_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_water_surface_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    indirect_layout: &wgpu::BindGroupLayout,
    sky_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-water-surface-layout"),
        bind_group_layouts: &[Some(layout), Some(indirect_layout), Some(sky_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-water-surface-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[Some(GpuVertex::LAYOUT)],
            compilation_options: Default::default(),
        },
        // Seen from above and from below (the Snell's-window ceiling).
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            ..opaque_primitive()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_water"),
            targets: &[Some(wgpu::ColorTargetState {
                format: HDR_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_water_grid_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    indirect_layout: &wgpu::BindGroupLayout,
    sky_layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-water-grid-layout"),
        bind_group_layouts: &[Some(layout), Some(indirect_layout), Some(sky_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-water-grid-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_water"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        // Seen from above and from below (the Snell's-window ceiling).
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            ..opaque_primitive()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_water_grid"),
            targets: &[Some(wgpu::ColorTargetState {
                format: HDR_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_shadow_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    vertex_entry: &str,
    vertex_buffers: &[Option<wgpu::VertexBufferLayout<'_>>],
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-shadow-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-shadow-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vertex_entry),
            buffers: vertex_buffers,
            compilation_options: Default::default(),
        },
        primitive: opaque_primitive(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: wgpu::DepthBiasState {
                constant: 2,
                slope_scale: 2.0,
                clamp: 0.0,
            },
        }),
        multisample: Default::default(),
        fragment: None,
        multiview_mask: None,
        cache: None,
    })
}

fn create_tone_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    output_format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-tone-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
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
                format: output_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_skybox_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("spall-voxel-skybox-pipeline-layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("spall-voxel-skybox-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
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
        multiview_mask: None,
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

/// The four sun cascades and their split distances.
///
/// Each cascade is a box in light space around a *sphere centred on the
/// camera*, big enough to hold everything the cascade's slice of the view
/// frustum can show whichever way the camera looks. The box depends on the
/// camera's position and the sun alone: turning the camera changes nothing,
/// and moving it changes the box only when the centre crosses a step of
/// about 8% of the cascade's reach (a multiple of its texel, so shadow edges
/// never swim). A cascade whose box is unchanged and whose terrain is
/// unchanged keeps its terrain depth from the last frame
/// ([`crate::game_renderer`]), which is most frames for the far cascades.
/// The angular step the shadow maps follow the sun in, degrees. The day clock
/// moves the sun a tenth of a degree a second; framing the maps afresh for
/// each of those makes the texel grid slide across the terrain and the
/// stair-stepped edges of its shadows shimmer on a slope. Held for a few
/// seconds at a time they stay still (and the cached terrain depth stays
/// valid), and the shift when they do move is under a texel near the camera.
const SHADOW_SUN_STEP_DEG: f32 = 0.5;

/// `sun_dir` rounded to a grid of [`SHADOW_SUN_STEP_DEG`] in azimuth and
/// elevation, normalised.
fn quantized_sun(sun_dir: Vec3) -> Vec3 {
    let sun = sun_dir.normalize();
    let step = SHADOW_SUN_STEP_DEG.to_radians();
    let elevation = sun.y.clamp(-1.0, 1.0).asin();
    let azimuth = sun.z.atan2(sun.x);
    let elevation = (elevation / step).round() * step;
    let azimuth = (azimuth / step).round() * step;
    Vec3::new(
        elevation.cos() * azimuth.cos(),
        elevation.sin(),
        elevation.cos() * azimuth.sin(),
    )
    .normalize()
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

    // Farthest a point at view depth `d` can be from the camera: `d` times
    // the length of the frustum's corner ray.
    let tan_h = (camera.fov_y * 0.5).tan();
    let tan_w = tan_h * camera.aspect;
    let corner = (1.0 + tan_h * tan_h + tan_w * tan_w).sqrt();

    let sun = quantized_sun(sun_dir);
    let up = if sun.dot(Vec3::Y).abs() > 0.95 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    // The light view's own axes, so the snapping below is exactly in them.
    let right = sun.cross(up).normalize();
    let light_up = right.cross(sun);

    let mut matrices = [Mat4::IDENTITY; CASCADE_COUNT];
    for i in 0..CASCADE_COUNT {
        let reach = (splits[i] * corner).max(1.0);
        let step_wanted = reach * 0.08;
        let radius = reach + step_wanted * 3.0;
        let texel = (radius * 2.0) / SHADOW_MAP_SIZE as f32;
        let step = (step_wanted / texel).ceil() * texel;
        let snap = |v: f32| (v / step).round() * step;
        let p = camera.position;
        let centre =
            right * snap(p.dot(right)) + light_up * snap(p.dot(light_up)) + sun * snap(p.dot(sun));
        let light_eye = centre - sun * radius * 2.5;
        let light_view = rh::view::look_at_mat4(light_eye, centre, up);
        let light_proj =
            rh::proj::directx::orthographic(-radius, radius, -radius, radius, 0.0, radius * 5.0);
        matrices[i] = light_proj * light_view;
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
    fn the_shadow_sun_holds_still_between_steps_and_stays_close() {
        let at = |deg: f32| {
            let r = deg.to_radians();
            Vec3::new(r.cos(), -r.sin(), 0.3).normalize()
        };
        // A tenth of a degree, the day clock's pace for a second, moves nothing
        // except when it crosses a step...
        let a = quantized_sun(at(40.0));
        assert_eq!(a, quantized_sun(at(40.05)));
        // ...a change of a step does move it, and it is never far from the sun.
        assert_ne!(a, quantized_sun(at(41.0)));
        for deg in [3.0, 17.3, 40.0, 61.7, 88.0] {
            let sun = at(deg).normalize();
            let angle = quantized_sun(sun)
                .dot(sun)
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees();
            assert!(angle < SHADOW_SUN_STEP_DEG, "{deg}: {angle}");
        }
    }

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
    fn cascade_matrices_are_finite_and_stable_for_small_camera_motion() {
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
    fn turning_the_camera_does_not_change_any_cascade() {
        let camera = Camera {
            position: Vec3::new(30.0, 12.0, -8.0),
            z_far: 120.0,
            ..Default::default()
        };
        let (a, splits_a) = cascade_data(&camera, default_sun_dir());
        for yaw in [0.3_f32, 1.7, -2.4] {
            let (b, splits_b) = cascade_data(
                &Camera {
                    yaw,
                    pitch: 0.4,
                    ..camera
                },
                default_sun_dir(),
            );
            assert_eq!(a, b, "yaw {yaw}");
            assert_eq!(splits_a, splits_b);
        }
    }

    #[test]
    fn a_cascade_changes_only_after_the_camera_moves_a_step_and_far_ones_change_less() {
        let camera = Camera {
            position: Vec3::new(100.0, 20.0, 100.0),
            z_far: 120.0,
            ..Default::default()
        };
        let (base, _) = cascade_data(&camera, default_sun_dir());
        // Walk along x in 5 cm steps for 20 m and count how often each
        // cascade's matrix differs from the previous one.
        let mut changes = [0usize; CASCADE_COUNT];
        let mut last = base;
        for n in 1..=400 {
            let moved = Camera {
                position: camera.position + Vec3::new(0.05 * n as f32, 0.0, 0.0),
                ..camera
            };
            let (m, _) = cascade_data(&moved, default_sun_dir());
            for i in 0..CASCADE_COUNT {
                if m[i] != last[i] {
                    changes[i] += 1;
                }
            }
            last = m;
        }
        assert!(
            changes[0] > changes[1] && changes[1] > changes[2] && changes[2] > changes[3],
            "{changes:?}"
        );
        // Over 20 m the farthest cascade is rebuilt a handful of times, not 400.
        assert!(changes[3] <= 6, "{changes:?}");
        assert!(changes[0] >= 5, "{changes:?}");
    }

    #[test]
    fn every_point_a_cascade_serves_lies_inside_its_box() {
        let camera = Camera {
            position: Vec3::new(-14.3, 31.7, 9.1),
            yaw: 0.9,
            pitch: -0.3,
            z_far: 120.0,
            ..Default::default()
        };
        let (matrices, splits) = cascade_data(&camera, default_sun_dir());
        let forward = camera.forward();
        let right = forward.cross(Vec3::Y).normalize();
        let up = right.cross(forward);
        let (tan_h, tan_w) = (
            (camera.fov_y * 0.5).tan(),
            (camera.fov_y * 0.5).tan() * camera.aspect,
        );
        let mut slice_near = camera.z_near;
        for (i, matrix) in matrices.iter().enumerate() {
            // The corners and the middle of the slice's far face and its near face.
            for depth in [slice_near.max(0.01), splits[i]] {
                for (sx, sy) in [
                    (-1.0, -1.0),
                    (1.0, -1.0),
                    (-1.0, 1.0),
                    (1.0, 1.0),
                    (0.0, 0.0),
                ] {
                    let point = camera.position
                        + (forward + right * (sx * tan_w) + up * (sy * tan_h)) * depth;
                    let clip = *matrix * point.extend(1.0);
                    let ndc = clip.truncate() / clip.w;
                    assert!(
                        ndc.x.abs() <= 1.0 && ndc.y.abs() <= 1.0 && (0.0..=1.0).contains(&ndc.z),
                        "cascade {i} depth {depth} corner ({sx}, {sy}) at {ndc:?}"
                    );
                }
            }
            slice_near = splits[i];
        }
    }

    #[test]
    fn material_gpu_layout_is_storage_buffer_safe() {
        assert_eq!(std::mem::size_of::<GpuMaterial>(), 32);
    }
}
