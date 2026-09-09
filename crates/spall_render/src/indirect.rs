//! Fixed-size camera-local voxel lighting prototype for T13.
//!
//! The cache is deliberately a derived render resource. It is not a world
//! format and it never feeds collision, support, or authoritative geometry.

use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::{IVec3, Vec3};
use wgpu::util::DeviceExt;

use crate::scene::Material;

pub const LIGHT_VOLUME_DIM: u32 = 128;
pub const LIGHT_CELL_SIZE_METRES: f32 = 0.5;
const TRACE_WORKGROUP: u32 = 4;

/// CPU staging image for the 128^3 occupancy/material clipmap. Each cell is a
/// material id; zero is air.
#[derive(Debug, Clone)]
pub struct LightingVolume {
    origin: Vec3,
    cells: Vec<u32>,
}

impl LightingVolume {
    pub fn empty(origin: Vec3) -> Self {
        Self {
            origin,
            cells: vec![
                0;
                LIGHT_VOLUME_DIM as usize
                    * LIGHT_VOLUME_DIM as usize
                    * LIGHT_VOLUME_DIM as usize
            ],
        }
    }

    pub fn origin(&self) -> Vec3 {
        self.origin
    }

    pub fn cells(&self) -> &[u32] {
        &self.cells
    }

    pub fn set(&mut self, cell: IVec3, material: u32) -> bool {
        let Some(index) = cell_index(cell) else {
            return false;
        };
        self.cells[index] = material;
        true
    }

    /// Fill a half-open clipmap-cell box. Bounds are clipped to the cache.
    pub fn fill_box(&mut self, min: IVec3, max: IVec3, material: u32) {
        let lo = min.max(IVec3::ZERO);
        let hi = max.min(IVec3::splat(LIGHT_VOLUME_DIM as i32));
        for z in lo.z..hi.z {
            for y in lo.y..hi.y {
                for x in lo.x..hi.x {
                    let _ = self.set(IVec3::new(x, y, z), material);
                }
            }
        }
    }

    pub fn world_to_cell(&self, world: Vec3) -> IVec3 {
        ((world - self.origin) / LIGHT_CELL_SIZE_METRES)
            .floor()
            .as_ivec3()
    }

    /// CPU copy of the prototype's unfiltered trace, used for deterministic
    /// fixture probes and leakage quantification without requiring a GPU.
    pub fn probe_radiance(&self, world: Vec3, materials: &[Material]) -> Vec3 {
        trace_air_cell(self, self.world_to_cell(world), materials)
    }
}

fn cell_index(cell: IVec3) -> Option<usize> {
    let d = LIGHT_VOLUME_DIM as i32;
    if cell.cmplt(IVec3::ZERO).any() || cell.cmpge(IVec3::splat(d)).any() {
        return None;
    }
    Some((cell.x + d * (cell.y + d * cell.z)) as usize)
}

const TRACE_DIRECTIONS: [Vec3; 12] = [
    Vec3::new(1.0, 0.0, 0.0),
    Vec3::new(-1.0, 0.0, 0.0),
    Vec3::new(0.0, 1.0, 0.0),
    Vec3::new(0.0, -1.0, 0.0),
    Vec3::new(0.0, 0.0, 1.0),
    Vec3::new(0.0, 0.0, -1.0),
    Vec3::new(0.70710677, 0.70710677, 0.0),
    Vec3::new(-0.70710677, 0.70710677, 0.0),
    Vec3::new(0.0, 0.70710677, 0.70710677),
    Vec3::new(0.0, 0.70710677, -0.70710677),
    Vec3::new(0.57735026, 0.57735026, 0.57735026),
    Vec3::new(-0.57735026, 0.57735026, -0.57735026),
];

fn trace_air_cell(volume: &LightingVolume, start: IVec3, materials: &[Material]) -> Vec3 {
    if cell_index(start).is_none() {
        return Vec3::ZERO;
    }
    let sky = Vec3::new(0.24, 0.31, 0.42);
    let mut sum = Vec3::ZERO;
    for direction in TRACE_DIRECTIONS {
        let mut position = start.as_vec3() + Vec3::splat(0.5);
        let mut sample = sky * (0.35 + 0.65 * direction.y.max(0.0));
        for _ in 0..48 {
            position += direction;
            let cell = position.floor().as_ivec3();
            let Some(index) = cell_index(cell) else {
                break;
            };
            let material_id = volume.cells[index] as usize;
            if material_id == 0 {
                continue;
            }
            let material = materials.get(material_id).copied().unwrap_or_default();
            let base = Vec3::from_array(material.base_color);
            sample = base * (material.emissive.max(0.0) + 0.12 * direction.y.max(0.0));
            break;
        }
        sum += sample;
    }
    sum / TRACE_DIRECTIONS.len() as f32
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct IndirectGlobals {
    origin_cell_size: [f32; 4],
    dimensions: [u32; 4],
    sky: [f32; 4],
}

pub(crate) struct IndirectResources {
    pub(crate) trace_bind: wgpu::BindGroup,
    pub(crate) denoise_bind: wgpu::BindGroup,
    pub(crate) display_bind: wgpu::BindGroup,
    pub(crate) upload_millis: f64,
    pub(crate) enabled: bool,
    pub(crate) cells: usize,
    _cells: wgpu::Buffer,
    _source_dummy: wgpu::Buffer,
    _trace: wgpu::Buffer,
    _denoised: wgpu::Buffer,
    _globals: wgpu::Buffer,
}

pub(crate) struct IndirectPipeline {
    trace: wgpu::ComputePipeline,
    denoise: wgpu::ComputePipeline,
    compute_layout: wgpu::BindGroupLayout,
    display_layout: wgpu::BindGroupLayout,
}

impl IndirectPipeline {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-t13-indirect-compute-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(1, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(2, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(3, false, wgpu::ShaderStages::COMPUTE),
                uniform_entry(4, wgpu::ShaderStages::COMPUTE),
            ],
        });
        let display_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-t13-indirect-display-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::FRAGMENT),
                uniform_entry(1, wgpu::ShaderStages::FRAGMENT),
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spall-t13-indirect-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/indirect.wgsl").into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spall-t13-indirect-pipeline-layout"),
            bind_group_layouts: &[&compute_layout],
            push_constant_ranges: &[],
        });
        let make = |label, entry| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        Self {
            trace: make("spall-t13-trace-pipeline", "trace_main"),
            denoise: make("spall-t13-denoise-pipeline", "denoise_main"),
            compute_layout,
            display_layout,
        }
    }

    pub(crate) fn display_layout(&self) -> &wgpu::BindGroupLayout {
        &self.display_layout
    }

    pub(crate) fn resources(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        volume: Option<&LightingVolume>,
        materials: &wgpu::Buffer,
    ) -> IndirectResources {
        let start = Instant::now();
        let fallback = [0u32];
        let (origin, cells, dim, enabled) = match volume {
            Some(volume) => (
                volume.origin,
                volume.cells.as_slice(),
                LIGHT_VOLUME_DIM,
                true,
            ),
            None => (Vec3::ZERO, &fallback[..], 1, false),
        };
        let cell_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-t13-occupancy-material-cells"),
            contents: bytemuck::cast_slice(cells),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let output_size = cells.len() as u64 * std::mem::size_of::<[f32; 4]>() as u64;
        let output = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: output_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            })
        };
        let trace = output("spall-t13-traced-radiance");
        let denoised = output("spall-t13-denoised-radiance");
        let source_dummy = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-t13-unused-trace-source"),
            size: std::mem::size_of::<[f32; 4]>() as u64,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let globals = IndirectGlobals {
            origin_cell_size: [origin.x, origin.y, origin.z, LIGHT_CELL_SIZE_METRES],
            dimensions: [dim, 48, u32::from(enabled), 0],
            sky: [0.24, 0.31, 0.42, 0.0],
        };
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-t13-indirect-globals"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let compute_bind = |label, source: &wgpu::Buffer, target: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.compute_layout,
                entries: &[
                    entry(0, &cell_buffer),
                    entry(1, materials),
                    entry(2, source),
                    entry(3, target),
                    entry(4, &globals),
                ],
            })
        };
        // The trace pass ignores binding 2. Keep a distinct dummy buffer there:
        // a storage buffer cannot be read-only and read-write in one dispatch.
        let trace_bind = compute_bind("spall-t13-trace-bind", &source_dummy, &trace);
        let denoise_bind = compute_bind("spall-t13-denoise-bind", &trace, &denoised);
        let display_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-t13-display-bind"),
            layout: &self.display_layout,
            entries: &[entry(0, &denoised), entry(1, &globals)],
        });
        // Include command submission overhead for the initial occupancy upload;
        // device pass timings remain separate timestamp measurements.
        queue.submit(std::iter::empty());
        IndirectResources {
            trace_bind,
            denoise_bind,
            display_bind,
            upload_millis: start.elapsed().as_secs_f64() * 1000.0,
            enabled,
            cells: cells.len(),
            _cells: cell_buffer,
            _source_dummy: source_dummy,
            _trace: trace,
            _denoised: denoised,
            _globals: globals,
        }
    }

    pub(crate) fn dispatch<'a>(
        &'a self,
        encoder: &'a mut wgpu::CommandEncoder,
        resources: &'a IndirectResources,
        trace_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
        denoise_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
    ) {
        let groups = if resources.enabled {
            LIGHT_VOLUME_DIM.div_ceil(TRACE_WORKGROUP)
        } else {
            1
        };
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("spall-t13-voxel-trace-pass"),
                timestamp_writes: trace_timestamps,
            });
            pass.set_pipeline(&self.trace);
            pass.set_bind_group(0, &resources.trace_bind, &[]);
            pass.dispatch_workgroups(groups, groups, groups);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("spall-t13-denoise-pass"),
                timestamp_writes: denoise_timestamps,
            });
            pass.set_pipeline(&self.denoise);
            pass.set_bind_group(0, &resources.denoise_bind, &[]);
            pass.dispatch_workgroups(groups, groups, groups);
        }
    }
}

fn storage_entry(
    binding: u32,
    read_only: bool,
    visibility: wgpu::ShaderStages,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
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

fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_exactly_128_cubed_and_bounds_are_checked() {
        let mut volume = LightingVolume::empty(Vec3::splat(-32.0));
        assert_eq!(volume.cells().len(), 128usize.pow(3));
        assert!(volume.set(IVec3::new(127, 127, 127), 1));
        assert!(!volume.set(IVec3::new(128, 0, 0), 1));
        assert!(!volume.set(IVec3::new(-1, 0, 0), 1));
    }

    #[test]
    fn a_closed_probe_is_darker_than_an_open_probe() {
        let materials = crate::scene::default_materials();
        let origin = Vec3::splat(-32.0);
        let mut closed = LightingVolume::empty(origin);
        let centre = closed.world_to_cell(Vec3::ZERO);
        closed.fill_box(centre - IVec3::splat(4), centre + IVec3::new(5, -3, 5), 5);
        closed.fill_box(
            centre - IVec3::new(4, 3, 4),
            centre + IVec3::new(-3, 4, 5),
            5,
        );
        closed.fill_box(
            centre + IVec3::new(4, -3, -4),
            centre + IVec3::new(5, 4, 5),
            6,
        );
        closed.fill_box(
            centre - IVec3::new(4, 3, 4),
            centre + IVec3::new(5, 4, 5),
            1,
        );
        let open = LightingVolume::empty(origin);
        let closed_luma = closed.probe_radiance(Vec3::ZERO, &materials).length();
        let open_luma = open.probe_radiance(Vec3::ZERO, &materials).length();
        assert!(
            closed_luma < open_luma,
            "closed={closed_luma}, open={open_luma}"
        );
    }
}
