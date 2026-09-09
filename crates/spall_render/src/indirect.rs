//! Fixed-size camera-local voxel lighting prototype for T13.
//!
//! The cache is deliberately a derived render resource. It is not a world
//! format and it never feeds collision, support, or authoritative geometry.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::{IVec3, Vec3};
use wgpu::util::DeviceExt;

use crate::scene::Material;

pub const LIGHT_VOLUME_DIM: u32 = 128;
pub const LIGHT_CELL_SIZE_METRES: f32 = 0.5;
const TRACE_WORKGROUP: u32 = 4;

/// One solid axis-aligned fill applied during a [`LightingUpdate`], in world
/// metres. `material` `0` clears the span to air.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightingRegion {
    pub min_m: Vec3,
    pub max_m: Vec3,
    pub material: u32,
}

/// A plain, engine-agnostic description of what changed in the world since the
/// last lighting update, consumed by [`LightingVolume::apply_update`].
///
/// `spall_render` has no dependency on the simulation: the sandbox example
/// translates committed edits and moving-body poses into this DTO. Every dirty
/// bound is cleared to air first, then every region is filled in order — so a
/// caller passes the union of an edit's old and new occupancy (and a moving
/// body's old and new world bounds) as `dirty_world_bounds`, and the full solid
/// state left inside those bounds as `regions`. Reconstructing occupancy for the
/// whole dirty span this way is how a moving body vacates cells without a
/// separate clear step erasing geometry that overlaps its old bounds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LightingUpdate {
    pub dirty_world_bounds: Vec<(Vec3, Vec3)>,
    pub regions: Vec<LightingRegion>,
}

impl LightingUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a world-space AABB dirty. Its cells are cleared to air, then
    /// recomputed from whatever [`regions`](Self::regions) cover them.
    pub fn dirty_bound(mut self, min_m: Vec3, max_m: Vec3) -> Self {
        self.dirty_world_bounds.push((min_m, max_m));
        self
    }

    /// Add a solid fill, applied after every dirty bound has been cleared.
    pub fn region(mut self, min_m: Vec3, max_m: Vec3, material: u32) -> Self {
        self.regions.push(LightingRegion {
            min_m,
            max_m,
            material,
        });
        self
    }

    /// Add an existing [`LightingRegion`] verbatim.
    pub fn with_region(mut self, region: LightingRegion) -> Self {
        self.regions.push(region);
        self
    }

    /// Build the update for a box-shaped body moving from world AABB `prev` to
    /// `next`: both boxes are dirty, then `keep` (the static geometry that
    /// overlaps either box) is re-asserted so the move does not erase it, and
    /// finally the body is filled at `next` with `material`.
    ///
    /// Callers pass every static region that intersects `prev` or `next`.
    /// Nothing outside the two dirty boxes is touched, so unrelated geometry
    /// and other bodies are unaffected.
    pub fn moving_box(
        prev: (Vec3, Vec3),
        next: (Vec3, Vec3),
        material: u32,
        keep: &[LightingRegion],
    ) -> Self {
        let mut update = Self::new()
            .dirty_bound(prev.0, prev.1)
            .dirty_bound(next.0, next.1);
        update.regions.extend_from_slice(keep);
        update.regions.push(LightingRegion {
            min_m: next.0,
            max_m: next.1,
            material,
        });
        update
    }
}

/// CPU staging image for the 128^3 occupancy/material clipmap. Each cell is a
/// material id; zero is air.
#[derive(Debug, Clone)]
pub struct LightingVolume {
    origin: Vec3,
    cells: Vec<u32>,
    /// Flat indices whose material changed since the last [`take_dirty`], i.e.
    /// the minimal set the GPU occupancy buffer must re-upload. Construction-
    /// time fills ([`set`], [`fill_box`]) do not touch this; only
    /// [`apply_update`] does.
    ///
    /// [`take_dirty`]: Self::take_dirty
    /// [`set`]: Self::set
    /// [`fill_box`]: Self::fill_box
    /// [`apply_update`]: Self::apply_update
    dirty: BTreeSet<u32>,
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
            dirty: BTreeSet::new(),
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

    /// Apply an incremental world change: clear every dirty bound to air, then
    /// fill every region in order. A cell is recorded dirty only if its
    /// material actually changed, so re-filling a span with what was already
    /// there costs nothing downstream. Returns the number of newly dirtied
    /// cells.
    ///
    /// This is the T14 update path. Static fixture construction still uses the
    /// plain [`set`](Self::set) / [`fill_box`](Self::fill_box) helpers and does
    /// not dirty anything.
    pub fn apply_update(&mut self, update: &LightingUpdate) -> usize {
        let mut spans = Vec::new();
        for &(min_m, max_m) in &update.dirty_world_bounds {
            if let Some(span) = self.world_span(min_m, max_m) {
                spans.push(span);
            }
        }
        for region in &update.regions {
            if let Some(span) = self.world_span(region.min_m, region.max_m) {
                spans.push(span);
            }
        }

        // Snapshot the original material of every affected cell once, before
        // any mutation, so overlapping spans still compare against the true
        // pre-update value.
        let mut original: BTreeMap<u32, u32> = BTreeMap::new();
        for [lo, hi] in &spans {
            for z in lo.z..hi.z {
                for y in lo.y..hi.y {
                    for x in lo.x..hi.x {
                        let idx = cell_index(IVec3::new(x, y, z)).expect("span is clamped") as u32;
                        original.entry(idx).or_insert(self.cells[idx as usize]);
                    }
                }
            }
        }

        for &(min_m, max_m) in &update.dirty_world_bounds {
            self.write_span(min_m, max_m, 0);
        }
        for region in &update.regions {
            self.write_span(region.min_m, region.max_m, region.material);
        }

        let mut changed = 0;
        for (idx, old) in original {
            if self.cells[idx as usize] != old {
                self.dirty.insert(idx);
                changed += 1;
            }
        }
        changed
    }

    /// Drain the dirty set as `(flat cell index, current material)` pairs — the
    /// minimal re-upload set for the GPU occupancy buffer.
    pub fn take_dirty(&mut self) -> Vec<(u32, u32)> {
        let drained: Vec<(u32, u32)> = self
            .dirty
            .iter()
            .map(|&idx| (idx, self.cells[idx as usize]))
            .collect();
        self.dirty.clear();
        drained
    }

    /// Number of cells currently pending re-upload.
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    /// Clamped half-open cell box `[lo, hi)` for a world-space AABB, or `None`
    /// when the box misses the cache entirely.
    fn world_span(&self, min_m: Vec3, max_m: Vec3) -> Option<[IVec3; 2]> {
        let d = LIGHT_VOLUME_DIM as i32;
        let lo = self.world_to_cell(min_m).max(IVec3::ZERO);
        let hi =
            (self.world_to_cell(max_m - Vec3::splat(1.0e-4)) + IVec3::ONE).min(IVec3::splat(d));
        if lo.cmpge(hi).any() {
            return None;
        }
        Some([lo, hi])
    }

    fn write_span(&mut self, min_m: Vec3, max_m: Vec3, material: u32) {
        let Some([lo, hi]) = self.world_span(min_m, max_m) else {
            return;
        };
        for z in lo.z..hi.z {
            for y in lo.y..hi.y {
                for x in lo.x..hi.x {
                    let idx = cell_index(IVec3::new(x, y, z)).expect("span is clamped");
                    self.cells[idx] = material;
                }
            }
        }
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
    /// T14 partial re-trace bounds, in cache cells; `w` unused. See
    /// [`IndirectResources::set_trace_region`].
    trace_region_min: [u32; 4],
    trace_region_max: [u32; 4],
    /// T14 temporal blend: `[history_weight_of_current_frame, clamp_slack, 0, 0]`.
    /// `1.0` weight means no accumulation. See
    /// [`IndirectResources::set_temporal`].
    temporal: [f32; 4],
}

/// Byte offset of `trace_region_min` within [`IndirectGlobals`] — three
/// preceding `vec4`s. `set_trace_region` writes 8 `u32` from here.
const TRACE_REGION_OFFSET: u64 = 3 * 16;
/// Byte offset of `temporal` within [`IndirectGlobals`] — five preceding `vec4`s.
const TEMPORAL_OFFSET: u64 = 5 * 16;

pub(crate) struct IndirectResources {
    pub(crate) trace_bind: wgpu::BindGroup,
    pub(crate) denoise_bind: wgpu::BindGroup,
    pub(crate) temporal_bind: wgpu::BindGroup,
    /// Samples the denoised buffer — the T13 path, no temporal accumulation.
    pub(crate) display_bind: wgpu::BindGroup,
    /// Samples the temporal history buffer — used after [`dispatch_temporal`].
    pub(crate) history_display_bind: wgpu::BindGroup,
    pub(crate) upload_millis: f64,
    pub(crate) enabled: bool,
    pub(crate) cells: usize,
    cell_buffer: wgpu::Buffer,
    globals_buffer: wgpu::Buffer,
    _source_dummy: wgpu::Buffer,
    _trace: wgpu::Buffer,
    _denoised: wgpu::Buffer,
    _history: wgpu::Buffer,
}

impl IndirectResources {
    /// Re-upload only the cells [`LightingVolume::take_dirty`] reported changed,
    /// coalescing the sorted flat indices into contiguous runs so a scattered
    /// edit is a few `write_buffer` calls, not one per cell. Returns the number
    /// of cells written.
    pub(crate) fn upload_dirty(&self, queue: &wgpu::Queue, dirty: &[(u32, u32)]) -> usize {
        let stride = std::mem::size_of::<u32>() as u64;
        let mut written = 0usize;
        let mut i = 0usize;
        while i < dirty.len() {
            let start = dirty[i].0;
            let mut run: Vec<u32> = Vec::new();
            while i < dirty.len() && dirty[i].0 == start + run.len() as u32 {
                run.push(dirty[i].1);
                i += 1;
            }
            queue.write_buffer(
                &self.cell_buffer,
                u64::from(start) * stride,
                bytemuck::cast_slice(&run),
            );
            written += run.len();
        }
        written
    }

    /// Bound the next trace dispatch to `[min, max)` in cache cells. Cells
    /// outside keep their previous traced radiance, so a small edit re-traces a
    /// small region. The denoise pass still runs over the whole cache.
    pub(crate) fn set_trace_region(&self, queue: &wgpu::Queue, min: glam::UVec3, max: glam::UVec3) {
        let payload: [u32; 8] = [min.x, min.y, min.z, 0, max.x, max.y, max.z, 0];
        queue.write_buffer(
            &self.globals_buffer,
            TRACE_REGION_OFFSET,
            bytemuck::cast_slice(&payload),
        );
    }

    /// Reset the trace region to the whole cache.
    pub(crate) fn set_full_trace_region(&self, queue: &wgpu::Queue) {
        self.set_trace_region(
            queue,
            glam::UVec3::ZERO,
            glam::UVec3::splat(LIGHT_VOLUME_DIM),
        );
    }

    /// Configure the temporal pass: `current_weight` is how much of this frame's
    /// denoised estimate is mixed into the history for cells outside the
    /// re-traced region (`1.0` disables accumulation and reproduces the T13
    /// path); `slack` is the history clamp margin as a fraction of the local
    /// neighbourhood spread.
    pub(crate) fn set_temporal(&self, queue: &wgpu::Queue, current_weight: f32, slack: f32) {
        let payload: [f32; 4] = [current_weight.clamp(0.0, 1.0), slack.max(0.0), 0.0, 0.0];
        queue.write_buffer(
            &self.globals_buffer,
            TEMPORAL_OFFSET,
            bytemuck::cast_slice(&payload),
        );
    }
}

pub(crate) struct IndirectPipeline {
    trace: wgpu::ComputePipeline,
    denoise: wgpu::ComputePipeline,
    temporal: wgpu::ComputePipeline,
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
            temporal: make("spall-t14-temporal-pipeline", "temporal_main"),
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
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
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
        let history = output("spall-t14-history-radiance");
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
            trace_region_min: [0, 0, 0, 0],
            trace_region_max: [dim, dim, dim, 0],
            // Default: no accumulation, so `capture_scene` is unchanged.
            temporal: [1.0, 0.25, 0.0, 0.0],
        };
        let globals = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-t13-indirect-globals"),
            contents: bytemuck::bytes_of(&globals),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
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
        // Temporal reads this frame's denoised estimate, reads+writes history.
        let temporal_bind = compute_bind("spall-t14-temporal-bind", &denoised, &history);
        let display = |label, buffer: &wgpu::Buffer| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.display_layout,
                entries: &[entry(0, buffer), entry(1, &globals)],
            })
        };
        let display_bind = display("spall-t13-display-bind", &denoised);
        let history_display_bind = display("spall-t14-history-display-bind", &history);
        // Include command submission overhead for the initial occupancy upload;
        // device pass timings remain separate timestamp measurements.
        queue.submit(std::iter::empty());
        IndirectResources {
            trace_bind,
            denoise_bind,
            temporal_bind,
            display_bind,
            history_display_bind,
            upload_millis: start.elapsed().as_secs_f64() * 1000.0,
            enabled,
            cells: cells.len(),
            cell_buffer,
            globals_buffer: globals,
            _source_dummy: source_dummy,
            _trace: trace,
            _denoised: denoised,
            _history: history,
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

    /// Run the T14 temporal accumulation pass, blending this frame's denoised
    /// estimate into the persistent history. Sample `history_display_bind`
    /// afterwards.
    pub(crate) fn dispatch_temporal<'a>(
        &'a self,
        encoder: &'a mut wgpu::CommandEncoder,
        resources: &'a IndirectResources,
        timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
    ) {
        let groups = if resources.enabled {
            LIGHT_VOLUME_DIM.div_ceil(TRACE_WORKGROUP)
        } else {
            1
        };
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("spall-t14-temporal-pass"),
            timestamp_writes: timestamps,
        });
        pass.set_pipeline(&self.temporal);
        pass.set_bind_group(0, &resources.temporal_bind, &[]);
        pass.dispatch_workgroups(groups, groups, groups);
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

    /// Flat cache index of the cell containing a world point.
    fn cell_at(v: &LightingVolume, world: Vec3) -> usize {
        cell_index(v.world_to_cell(world)).expect("point inside cache")
    }

    #[test]
    fn apply_update_fills_and_dirties_only_the_changed_cells() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        let update = LightingUpdate::new()
            .dirty_bound(Vec3::ZERO, Vec3::ONE)
            .region(Vec3::ZERO, Vec3::ONE, 5);

        // A 1 m cube at the origin is 2x2x2 half-metre cells.
        assert_eq!(v.apply_update(&update), 8);
        assert_eq!(v.dirty_len(), 8);
        assert_eq!(v.cells()[cell_at(&v, Vec3::splat(0.25))], 5);

        let drained = v.take_dirty();
        assert_eq!(drained.len(), 8);
        assert!(drained.iter().all(|&(_, material)| material == 5));
        assert_eq!(v.dirty_len(), 0);
    }

    #[test]
    fn construction_fills_never_dirty_anything() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        v.set(IVec3::new(10, 10, 10), 7);
        v.fill_box(IVec3::splat(0), IVec3::splat(4), 3);
        assert_eq!(v.dirty_len(), 0);
        assert!(v.take_dirty().is_empty());
    }

    #[test]
    fn refilling_a_span_with_the_same_material_dirties_nothing() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        let fill = LightingUpdate::new()
            .dirty_bound(Vec3::ZERO, Vec3::splat(1.5))
            .region(Vec3::ZERO, Vec3::splat(1.5), 3);
        v.apply_update(&fill);
        v.take_dirty();

        assert_eq!(v.apply_update(&fill), 0);
        assert_eq!(v.dirty_len(), 0);
    }

    #[test]
    fn moving_a_body_dirties_vacated_and_entered_cells_but_preserves_the_overlap() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        let (a_min, a_max) = (Vec3::ZERO, Vec3::new(2.0, 1.0, 1.0));
        v.apply_update(
            &LightingUpdate::new()
                .dirty_bound(a_min, a_max)
                .region(a_min, a_max, 5),
        );
        v.take_dirty();

        // Slide the body +1 m along X. Old and new bounds are both dirty; the
        // caller re-asserts the body's solid geometry only at its new box.
        let (b_min, b_max) = (Vec3::new(1.0, 0.0, 0.0), Vec3::new(3.0, 1.0, 1.0));
        let changed = v.apply_update(
            &LightingUpdate::new()
                .dirty_bound(a_min, a_max)
                .dirty_bound(b_min, b_max)
                .region(b_min, b_max, 5),
        );
        // Vacated x in [0,1) and newly entered x in [2,3): 2x2x2 cells each.
        // The x in [1,2) overlap keeps material 5 and stays clean.
        assert_eq!(changed, 16);

        let drained = v.take_dirty();
        let overlap = cell_at(&v, Vec3::new(1.5, 0.5, 0.5));
        assert!(!drained.iter().any(|&(idx, _)| idx as usize == overlap));
        assert_eq!(
            v.cells()[overlap],
            5,
            "overlap geometry must survive the move"
        );
        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(0.5, 0.5, 0.5))],
            0,
            "vacated -> air"
        );
        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(2.5, 0.5, 0.5))],
            5,
            "entered -> solid"
        );
    }

    #[test]
    fn moving_box_tracks_the_body_without_erasing_static_geometry() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        // A low static slab the body stands on and slides along.
        let wall = LightingRegion {
            min_m: Vec3::new(-3.0, 0.0, -1.0),
            max_m: Vec3::new(3.0, 1.0, 1.0),
            material: 1,
        };
        let start = (Vec3::new(-0.5, 0.0, -1.0), Vec3::new(0.0, 2.0, 1.0));
        v.apply_update(
            &LightingUpdate::new()
                .with_region(wall)
                .dirty_bound(start.0, start.1)
                .region(start.0, start.1, 6),
        );
        v.take_dirty();

        // Slide the body +2.5 m along X, re-asserting the slab it overlaps.
        let moved = (Vec3::new(2.0, 0.0, -1.0), Vec3::new(2.5, 2.0, 1.0));
        v.apply_update(&LightingUpdate::moving_box(start, moved, 6, &[wall]));

        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(-2.5, 0.5, 0.0))],
            1,
            "slab outside the swept boxes is untouched"
        );
        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(-0.25, 1.5, 0.0))],
            0,
            "the body's start cells above the slab are vacated to air"
        );
        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(-0.25, 0.5, 0.0))],
            1,
            "slab under the vacated body is re-asserted, not erased"
        );
        assert_eq!(
            v.cells()[cell_at(&v, Vec3::new(2.25, 1.5, 0.0))],
            6,
            "the body now occupies its new box"
        );
    }

    #[test]
    fn apply_update_ignores_bounds_outside_the_cache() {
        let mut v = LightingVolume::empty(Vec3::splat(-32.0));
        let far = LightingUpdate::new()
            .dirty_bound(Vec3::new(100.0, 0.0, 0.0), Vec3::new(110.0, 1.0, 1.0))
            .region(Vec3::new(100.0, 0.0, 0.0), Vec3::new(110.0, 1.0, 1.0), 5);
        assert_eq!(v.apply_update(&far), 0);
        assert_eq!(v.dirty_len(), 0);
    }
}
