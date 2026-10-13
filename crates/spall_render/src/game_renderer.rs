//! The interactive game's frame renderer: the production T12 passes (cascaded
//! sun shadows, HDR opaque shading, tone map) over instanced-cube geometry,
//! written straight to a host-owned surface view.
//!
//! It shares [`ScenePipeline`], the material table semantics, [`Environment`]
//! and `fs_main` with the editor's [`crate::ViewportRenderer`]; only geometry
//! submission (instanced cubes here, greedy meshes there) and the output target
//! differ. The host keeps the device, queue, surface and any UI drawn after
//! [`GameRenderer::render`].
//!
//! Terrain and body instances are resident: [`GameRenderer::set_terrain`] is
//! called when a rebuild lands, [`GameRenderer::set_bodies`] when poses change,
//! and neither allocates while the data fits the existing buffer.

use std::collections::HashMap;
use wgpu::util::DeviceExt as _;

use crate::camera::{Aabb, Camera, Frustum};
use crate::environment::Environment;
use crate::indirect::{IndirectResources, LightingVolume};
use crate::instances::{CubeInstance, InstanceSet, unit_cube};
use crate::pipeline::{CASCADE_COUNT, DebugView, ScenePipeline};
use crate::scene::Material;
use crate::sky_visibility::SkyVisibility;
use crate::target::OffscreenTarget;
use crate::upload::{GpuMesh, UploadBudget};
use crate::vertex::GpuVertex;
use spall_core::BrickCoord;

/// GPU time of a recent frame's passes, from timestamp queries.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GamePassTimings {
    /// All shadow cascades together.
    pub shadow_ms: f64,
    pub opaque_ms: f64,
    pub tone_map_ms: f64,
    /// The most recent sky-visibility recompute (`None` until one has run).
    /// It runs when occupancy changes, not every frame, so it is not part of
    /// [`Self::total_ms`].
    pub sky_visibility_ms: Option<f64>,
    /// The most recent bounce recompute, like `sky_visibility_ms`.
    pub bounce_ms: Option<f64>,
}

impl GamePassTimings {
    pub fn total_ms(&self) -> f64 {
        self.shadow_ms + self.opaque_ms + self.tone_map_ms
    }
}

const TIMESTAMPS: u32 = 10;
const TIMER_SLOTS: usize = 3;

struct TimerSlot {
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    pending: Option<std::sync::mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>>,
    /// Whether this slot's frame ran the sky-visibility pass (timestamps 6, 7).
    has_sky: bool,
    /// Whether it also ran the bounce pass (timestamps 8, 9).
    has_bounce: bool,
}

/// Non-blocking timestamp timing: each frame resolves into one of a few
/// readback slots and a later frame collects it, so timing never stalls
/// presentation.
struct PassTimer {
    /// GPU milliseconds (sky plus bounce) of each sweep slice timed since the
    /// renderer last collected them.
    slice_costs: Vec<f64>,
    queries: wgpu::QuerySet,
    slots: [TimerSlot; TIMER_SLOTS],
    period_ns: f64,
    frame: usize,
    latest: Option<GamePassTimings>,
}

impl PassTimer {
    fn new(device: &wgpu::Device, period_ns: f32) -> Self {
        let bytes = u64::from(TIMESTAMPS) * 8;
        Self {
            slice_costs: Vec::new(),
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("spall-game-pass-timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: TIMESTAMPS,
            }),
            slots: std::array::from_fn(|_| TimerSlot {
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("spall-game-timestamp-resolve"),
                    size: bytes,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("spall-game-timestamp-readback"),
                    size: bytes,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                pending: None,
                has_sky: false,
                has_bounce: false,
            }),
            period_ns: f64::from(period_ns),
            frame: 0,
            latest: None,
        }
    }

    fn writes(&self, begin: u32, end: u32) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.queries,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(end),
        }
    }

    /// Collect any slot whose readback finished, then pick the slot this frame
    /// will resolve into. `None` while that slot is still in flight (this
    /// frame simply is not timed).
    fn begin_frame(&mut self) -> Option<usize> {
        for slot in &mut self.slots {
            let has_sky = slot.has_sky;
            let has_bounce = slot.has_bounce;
            let Some(receiver) = &slot.pending else {
                continue;
            };
            let Ok(result) = receiver.try_recv() else {
                continue;
            };
            slot.pending = None;
            if result.is_err() {
                continue;
            }
            let ticks: Vec<u64> = match slot.readback.slice(..).get_mapped_range() {
                Ok(bytes) => bytemuck::cast_slice::<u8, u64>(&bytes).to_vec(),
                Err(_) => Vec::new(),
            };
            slot.readback.unmap();
            if ticks.len() == TIMESTAMPS as usize {
                let ms = |a: usize, b: usize| {
                    ticks[b].saturating_sub(ticks[a]) as f64 * self.period_ns / 1.0e6
                };
                // Only bounce slices say what a layer costs.
                if has_bounce {
                    self.slice_costs.push(ms(8, 9));
                }
                let previous_sky = self.latest.and_then(|t| t.sky_visibility_ms);
                let previous_bounce = self.latest.and_then(|t| t.bounce_ms);
                self.latest = Some(GamePassTimings {
                    shadow_ms: ms(0, 1),
                    opaque_ms: ms(2, 3),
                    tone_map_ms: ms(4, 5),
                    sky_visibility_ms: if has_sky {
                        Some(ms(6, 7))
                    } else {
                        previous_sky
                    },
                    bounce_ms: if has_bounce {
                        Some(ms(8, 9))
                    } else {
                        previous_bounce
                    },
                });
            }
        }
        let index = self.frame % TIMER_SLOTS;
        self.frame += 1;
        self.slots[index].pending.is_none().then_some(index)
    }

    fn resolve(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        slot: usize,
        has_sky: bool,
        has_bounce: bool,
    ) {
        let bytes = u64::from(TIMESTAMPS) * 8;
        self.slots[slot].has_sky = has_sky;
        self.slots[slot].has_bounce = has_bounce;
        // Unwritten queries (the sky pair on a frame that did not run it) are
        // resolved too; the slot's `has_sky` says whether to read them.
        encoder.resolve_query_set(&self.queries, 0..TIMESTAMPS, &self.slots[slot].resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.slots[slot].resolve,
            0,
            &self.slots[slot].readback,
            0,
            bytes,
        );
    }

    /// Call after the encoder holding the resolve was submitted.
    fn map(&mut self, slot: usize) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.slots[slot]
            .readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        self.slots[slot].pending = Some(rx);
    }
}

/// What a cascade's retained terrain depth was drawn from: its light-space box,
/// the terrain meshes, and whether terrain was drawn at all.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ShadowKey {
    matrix: [f32; 16],
    terrain_generation: u64,
    terrain_visible: bool,
}

pub struct GameRenderer {
    pipeline: ScenePipeline,
    materials: wgpu::Buffer,
    indirect: IndirectResources,
    target: OffscreenTarget,
    cube_vertices: wgpu::Buffer,
    cube_indices: wgpu::Buffer,
    cube_index_count: u32,
    terrain: InstanceSet,
    /// Independently resident greedy terrain chunks. Replacing one brick never
    /// reallocates the buffers for its unchanged neighbours.
    terrain_meshes: HashMap<BrickCoord, GpuMesh>,
    terrain_bounds: HashMap<BrickCoord, Aabb>,
    terrain_generation: u64,
    terrain_meshes_visible: bool,
    /// Which chunks the camera pass skips because nothing of them shows.
    occlusion: crate::occlusion::OcclusionCuller,
    /// `SPALL_NO_OCCLUSION=1` draws every chunk in the frustum.
    occlusion_enabled: bool,
    /// What each cascade's retained terrain depth was drawn from.
    shadow_cache: [Option<ShadowKey>; CASCADE_COUNT],
    body_meshes: HashMap<u64, DynamicMesh>,
    bodies: InstanceSet,
    /// Drawn lit but never shadow-casting (debug overlays).
    overlay: InstanceSet,
    vegetation: InstanceSet,
    /// The foliage instances grouped by 16 m square, each with its bounds and
    /// its range in `vegetation`, so a pass draws only the groups it can see.
    vegetation_buckets: Vec<(Aabb, u32, u32)>,
    transparent: InstanceSet,
    /// The smoothed water surface sheet, drawn alpha-blended after the opaque
    /// scene.
    water_surface: DynamicMesh,
    /// Draw the water field as a smooth grid surface (see
    /// [`Self::set_water_grid_surface`]).
    water_grid: bool,
    /// Camera-local sky occupancy + visibility; `None` keeps the legacy
    /// unconditional hemispheric ambient.
    sky: Option<SkyVisibility>,
    timer: Option<PassTimer>,
    timed_slot: Option<usize>,
}

struct DynamicMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    vertex_capacity: u64,
    index_capacity: u64,
    index_count: u32,
}

impl DynamicMesh {
    fn new(device: &wgpu::Device) -> Self {
        Self {
            vertices: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-body-mesh-vertices"),
                size: 4,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            indices: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-body-mesh-indices"),
                size: 4,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            vertex_capacity: 4,
            index_capacity: 4,
            index_count: 0,
        }
    }

    fn update(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[GpuVertex],
        indices: &[u32],
    ) {
        let vb = std::mem::size_of_val(vertices) as u64;
        let ib = std::mem::size_of_val(indices) as u64;
        if vb > self.vertex_capacity {
            self.vertex_capacity = vb.next_power_of_two();
            self.vertices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-body-mesh-vertices"),
                size: self.vertex_capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if ib > self.index_capacity {
            self.index_capacity = ib.next_power_of_two();
            self.indices = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("spall-body-mesh-indices"),
                size: self.index_capacity,
                usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if vb > 0 {
            queue.write_buffer(&self.vertices, 0, bytemuck::cast_slice(vertices));
        }
        if ib > 0 {
            queue.write_buffer(&self.indices, 0, bytemuck::cast_slice(indices));
        }
        self.index_count = indices.len() as u32;
    }
}

impl GameRenderer {
    /// `output_format` is the sRGB surface format the tone map writes to.
    /// `timestamp_period_ns` is `Some` when the device was created with
    /// timestamp queries (`queue.get_timestamp_period()`); `None` reports GPU
    /// timing as unavailable.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        output_format: wgpu::TextureFormat,
        materials: &[Material],
        size: (u32, u32),
        timestamp_period_ns: Option<f32>,
    ) -> Self {
        let pipeline = ScenePipeline::new_for_output(device, output_format);
        let materials = pipeline.material_buffer(device, materials);
        let indirect = pipeline.indirect_resources(device, queue, None, &materials);
        let (vertices, indices) = unit_cube();
        Self {
            pipeline,
            materials,
            indirect,
            target: OffscreenTarget::new(device, size.0, size.1),
            cube_vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("spall-game-cube-vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            cube_indices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("spall-game-cube-indices"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            }),
            cube_index_count: indices.len() as u32,
            terrain: InstanceSet::new(),
            terrain_meshes: HashMap::new(),
            terrain_bounds: HashMap::new(),
            terrain_generation: 0,
            terrain_meshes_visible: true,
            occlusion: crate::occlusion::OcclusionCuller::new(device),
            occlusion_enabled: std::env::var_os("SPALL_NO_OCCLUSION").is_none(),
            shadow_cache: [None; CASCADE_COUNT],
            body_meshes: HashMap::new(),
            bodies: InstanceSet::new(),
            overlay: InstanceSet::new(),
            vegetation: InstanceSet::new(),
            vegetation_buckets: Vec::new(),
            transparent: InstanceSet::new(),
            water_surface: DynamicMesh::new(device),
            water_grid: false,
            sky: None,
            timer: timestamp_period_ns.map(|period| PassTimer::new(device, period)),
            timed_slot: None,
        }
    }

    /// Recreate the HDR/depth targets for a resized surface. Returns whether
    /// anything changed.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) -> bool {
        if (width.max(1), height.max(1)) == (self.target.width, self.target.height) {
            return false;
        }
        self.target = OffscreenTarget::new(device, width, height);
        true
    }

    /// Replace the resident terrain instances. Returns bytes uploaded.
    pub fn set_terrain(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        self.terrain.set(device, queue, instances)
    }

    /// Install changed brick meshes and evict chunks that left render
    /// residency. A fresh GPU allocation is made only for a changed chunk.
    pub fn update_terrain_meshes(
        &mut self,
        device: &wgpu::Device,
        updates: &[(BrickCoord, Vec<GpuVertex>, Vec<u32>)],
        removed: &[BrickCoord],
    ) -> Result<(), crate::context::RenderError> {
        if !updates.is_empty() || !removed.is_empty() {
            self.terrain_generation = self.terrain_generation.wrapping_add(1);
        }
        for coord in removed {
            self.terrain_meshes.remove(coord);
            self.terrain_bounds.remove(coord);
            self.occlusion.forget(*coord);
        }
        const BUDGET: UploadBudget = UploadBudget {
            max_vertex_bytes: 128 << 20,
            max_index_bytes: 64 << 20,
        };
        for (coord, vertices, indices) in updates {
            self.occlusion.forget(*coord);
            let mesh = GpuMesh::create(device, vertices, indices, BUDGET)?;
            if mesh.index_count == 0 {
                self.terrain_meshes.remove(coord);
                self.terrain_bounds.remove(coord);
            } else {
                let mut min = glam::Vec3::splat(f32::INFINITY);
                let mut max = glam::Vec3::splat(f32::NEG_INFINITY);
                for vertex in vertices {
                    let p = glam::Vec3::from_array(vertex.position);
                    min = min.min(p);
                    max = max.max(p);
                }
                self.terrain_bounds.insert(*coord, Aabb::new(min, max));
                self.terrain_meshes.insert(*coord, mesh);
            }
        }
        Ok(())
    }

    /// Fades the scene into the sky's horizon colour between `start_m` and
    /// `end_m` metres from the camera; see [`ScenePipeline::set_fog_range`].
    pub fn set_fog_range(&self, start_m: f32, end_m: f32) {
        self.pipeline.set_fog_range(start_m, end_m);
    }

    pub fn set_terrain_meshes_visible(&mut self, visible: bool) {
        self.terrain_meshes_visible = visible;
    }

    /// How many terrain chunks the camera pass drew last frame (the chunks in
    /// the frustum that were not known to be hidden).
    pub fn terrain_chunks_drawn(&self) -> usize {
        self.occlusion.drawn
    }

    pub fn terrain_geometry_generation(&self) -> u64 {
        self.terrain_generation
    }
    pub fn terrain_mesh_coords(&self) -> impl Iterator<Item = BrickCoord> + '_ {
        self.terrain_meshes.keys().copied()
    }

    /// Replace the moving bodies' transformed mesh buffers. The CPU templates
    /// remain cached by topology revision; only their per-frame transforms are
    /// applied before upload.
    pub fn set_body_meshes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        updates: &[(u64, Vec<GpuVertex>, Vec<u32>)],
        live: &[u64],
    ) -> Result<(), crate::context::RenderError> {
        const BUDGET: UploadBudget = UploadBudget {
            max_vertex_bytes: 128 << 20,
            max_index_bytes: 64 << 20,
        };
        let live: std::collections::HashSet<_> = live.iter().copied().collect();
        self.body_meshes.retain(|entity, _| live.contains(entity));
        for (entity, vertices, indices) in updates {
            let v_bytes = std::mem::size_of_val(vertices) as u64;
            let i_bytes = std::mem::size_of_val(indices) as u64;
            if v_bytes > BUDGET.max_vertex_bytes {
                return Err(crate::context::RenderError::UploadBudgetExceeded {
                    bytes: v_bytes,
                    budget: BUDGET.max_vertex_bytes,
                });
            }
            if i_bytes > BUDGET.max_index_bytes {
                return Err(crate::context::RenderError::UploadBudgetExceeded {
                    bytes: i_bytes,
                    budget: BUDGET.max_index_bytes,
                });
            }
            if vertices.is_empty() || indices.is_empty() {
                self.body_meshes.remove(entity);
                continue;
            }
            self.body_meshes
                .entry(*entity)
                .or_insert_with(|| DynamicMesh::new(device))
                .update(device, queue, vertices, indices);
        }
        Ok(())
    }

    /// Replace the body instances (re-posed as bodies move). Bodies also block
    /// skylight and act as bounce sources: when sky occupancy exists they are
    /// laid over it as an exact cell diff and a time-sliced lighting sweep
    /// starts (see [`SkyVisibility`]). Returns bytes uploaded.
    pub fn set_bodies(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        if let Some(sky) = &self.sky {
            sky.set_bodies(queue, instances);
        }
        self.bodies.set(device, queue, instances)
    }

    /// Resident chunk/body mesh counts, triangle totals and allocated bytes.
    pub fn mesh_stats(&self) -> (usize, u64, usize, u64, u64) {
        let terrain_triangles: u64 = self
            .terrain_meshes
            .values()
            .map(|m| u64::from(m.index_count / 3))
            .sum();
        let terrain_bytes: u64 = self
            .terrain_meshes
            .values()
            .map(|m| m.vertex_bytes + m.index_bytes)
            .sum();
        let body_triangles: u64 = self
            .body_meshes
            .values()
            .map(|m| u64::from(m.index_count / 3))
            .sum();
        let body_bytes: u64 = self
            .body_meshes
            .values()
            .map(|m| m.vertex_capacity + m.index_capacity)
            .sum();
        (
            self.terrain_meshes.len(),
            terrain_triangles,
            self.body_meshes.len(),
            body_triangles,
            terrain_bytes + body_bytes,
        )
    }

    /// Frames the last completed lighting sweep took (see
    /// [`SkyVisibility::last_sweep_frames`]); `None` without sky occupancy.
    pub fn lighting_sweep_frames(&self) -> Option<u32> {
        self.sky.as_ref().map(SkyVisibility::last_sweep_frames)
    }

    /// Milliseconds the last lit change waited for its lighting sweep
    /// (see [`SkyVisibility::last_latency_ms`]).
    pub fn lighting_latency_ms(&self) -> Option<f64> {
        self.sky.as_ref().and_then(SkyVisibility::last_latency_ms)
    }

    /// Whether a lighting sweep is running or queued.
    pub fn lighting_is_sweeping(&self) -> bool {
        self.sky.as_ref().is_some_and(SkyVisibility::is_sweeping)
    }

    /// Soft vegetation is nonphysical geometry, but participates in the same
    /// opaque lighting and shadow passes as wood and terrain.
    pub fn set_vegetation(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        // Sorted into 16 m squares so each pass can cull whole squares.
        const SQUARE_M: f32 = 16.0;
        let key = |i: &CubeInstance| {
            (
                (i.offset[0] / SQUARE_M).floor() as i64,
                (i.offset[2] / SQUARE_M).floor() as i64,
            )
        };
        let mut sorted: Vec<CubeInstance> = instances.to_vec();
        sorted.sort_by_key(|i| key(i));
        self.vegetation_buckets.clear();
        let mut start = 0;
        while start < sorted.len() {
            let square = key(&sorted[start]);
            let mut end = start;
            let mut lo = glam::Vec3::splat(f32::INFINITY);
            let mut hi = glam::Vec3::splat(f32::NEG_INFINITY);
            while end < sorted.len() && key(&sorted[end]) == square {
                let i = &sorted[end];
                let centre = glam::Vec3::from_array(i.offset);
                // A rotated box reaches half its diagonal from its centre.
                let reach = glam::Vec3::splat(glam::Vec3::from_array(i.size).length() * 0.5);
                lo = lo.min(centre - reach);
                hi = hi.max(centre + reach);
                end += 1;
            }
            self.vegetation_buckets
                .push((Aabb::new(lo, hi), start as u32, (end - start) as u32));
            start = end;
        }
        self.vegetation.set(device, queue, &sorted)
    }

    /// Replace the debug overlay instances (lit, not shadow-casting).
    pub fn set_overlay(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        self.overlay.set(device, queue, instances)
    }

    /// Replace alpha-blended debug cubes. The caller sorts these back-to-front
    /// for its current camera before upload.
    pub fn set_transparent_cubes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        instances: &[CubeInstance],
    ) -> u64 {
        self.transparent.set(device, queue, instances)
    }

    /// Replace the water surface sheet (empty slices clear it).
    pub fn set_water_surface(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[GpuVertex],
        indices: &[u32],
    ) {
        self.water_surface.update(device, queue, vertices, indices);
    }

    /// Supply (or clear, with `None`) the water surface heights used for
    /// underwater fog, tint and caustics.
    pub fn set_water_field(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        field: Option<crate::water::WaterField>,
    ) {
        self.pipeline.set_water_field(device, queue, field);
    }

    /// Turns the smooth grid surface on or off. When on, the installed water
    /// field is drawn as a displaced grid (nothing is uploaded per frame but
    /// changed field rectangles), and the mesh sheet should be empty.
    pub fn set_water_grid_surface(&mut self, on: bool) {
        self.water_grid = on;
    }

    /// Replaces a rectangle of the installed water field; see
    /// [`ScenePipeline::update_water_field_rect`].
    pub fn update_water_field_rect(
        &mut self,
        queue: &wgpu::Queue,
        x0: u32,
        z0: u32,
        w: u32,
        h: u32,
        texels: &[[f32; 4]],
    ) {
        self.pipeline
            .update_water_field_rect(queue, x0, z0, w, h, texels);
    }

    /// Water animation clock in seconds (caustics).
    pub fn set_water_time(&mut self, seconds: f32) {
        self.pipeline.set_water_time(seconds);
    }

    /// Supply (or clear, with `None`) the camera-local occupancy that makes
    /// skylight visibility-aware. The occupancy is derived, client-local data;
    /// it is uploaded now and the visibility recomputed on the next frame.
    /// Unknown space must be marked (`UNKNOWN_CELL`) by the builder, not left
    /// as air.
    pub fn set_sky_occupancy(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        occupancy: Option<&LightingVolume>,
    ) {
        match occupancy {
            Some(volume) => {
                let sky = self.sky.get_or_insert_with(|| {
                    self.pipeline.create_sky_visibility(device, &self.materials)
                });
                sky.set_occupancy(queue, volume);
            }
            None => self.sky = None,
        }
    }

    /// Switch the diffuse bounce on or off (a no-op until occupancy exists).
    /// Off skips the pass; sky visibility is unaffected.
    /// How much lighting to compute; see [`crate::sky_visibility::LightingQuality`].
    pub fn set_lighting_quality(
        &self,
        queue: &wgpu::Queue,
        quality: crate::sky_visibility::LightingQuality,
    ) {
        if let Some(sky) = self.sky.as_ref() {
            sky.set_quality(queue, quality);
        }
    }

    pub fn set_bounce_enabled(&self, queue: &wgpu::Queue, enabled: bool) {
        if let Some(sky) = &self.sky {
            sky.set_bounce_enabled(queue, enabled);
        }
    }

    /// GPU memory held by the sky-visibility cache (0 when disabled).
    pub fn sky_bytes(&self) -> u64 {
        self.sky.as_ref().map_or(0, SkyVisibility::allocated_bytes)
    }

    /// `(terrain, bodies, overlay)` instance counts.
    pub fn instance_counts(&self) -> (u32, u32, u32) {
        (
            self.terrain.count() + self.vegetation.count(),
            self.bodies.count(),
            self.overlay.count(),
        )
    }

    /// GPU memory held by instance buffers.
    pub fn instance_bytes(&self) -> u64 {
        self.terrain.allocated_bytes()
            + self.bodies.allocated_bytes()
            + self.overlay.allocated_bytes()
            + self.vegetation.allocated_bytes()
    }

    /// The freshest completed GPU pass timings, if timestamps are available.
    pub fn pass_timings(&self) -> Option<GamePassTimings> {
        self.timer.as_ref().and_then(|timer| timer.latest)
    }

    /// Sets the point lights the following frames are shaded with (the nearest
    /// [`crate::MAX_POINT_LIGHTS`]; see [`crate::PointLight`]).
    pub fn set_point_lights(&self, lights: &[crate::PointLight]) {
        self.pipeline.set_point_lights(lights);
    }

    /// Record shadows, HDR shading and the tone map into `encoder`, writing the
    /// tone-mapped frame to `output` (which must match the size given to
    /// [`Self::new`]/[`Self::resize`]). The host draws its UI afterwards.
    ///
    /// Call [`Self::finish_timing`] after submitting `encoder`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output: &wgpu::TextureView,
        camera: &Camera,
        environment: &Environment,
        view: DebugView,
    ) {
        let mut camera = *camera;
        camera.aspect = self.target.width as f32 / self.target.height as f32;
        self.timed_slot = self.timer.as_mut().and_then(PassTimer::begin_frame);
        if let (Some(timer), Some(sky)) = (self.timer.as_mut(), self.sky.as_ref()) {
            for cost in timer.slice_costs.drain(..) {
                sky.note_slice_cost(cost);
            }
        }
        let timer = self.timer.as_ref().filter(|_| self.timed_slot.is_some());

        let (light_matrices, _) = ScenePipeline::cascade_data(&camera, environment.sun_dir);
        for (cascade, matrix) in light_matrices.into_iter().enumerate() {
            let bind = self
                .pipeline
                .shadow_bind_group(device, queue, cascade, matrix);
            // The terrain's depth in this cascade is kept between frames and
            // redrawn only when the cascade's box moved or the terrain changed;
            // each frame it is copied into the sampled layer and the moving
            // casters are drawn over it. A cascade's box depends on the camera's
            // position alone, and the far cascades move only every few metres,
            // so most frames skip the thousands of chunk draws.
            let key = ShadowKey {
                matrix: matrix.to_cols_array(),
                terrain_generation: self.terrain_generation,
                terrain_visible: self.terrain_meshes_visible,
            };
            let redraw_terrain = self.shadow_cache[cascade].as_ref() != Some(&key);
            // One begin/end pair spans the whole cascade set.
            let timestamp_writes = |first: bool, last: bool| {
                timer.and_then(|timer| {
                    let (begin, end) = (
                        (cascade == 0 && first).then_some(0),
                        (cascade == CASCADE_COUNT - 1 && last).then_some(1),
                    );
                    (begin.is_some() || end.is_some()).then_some(wgpu::RenderPassTimestampWrites {
                        query_set: &timer.queries,
                        beginning_of_pass_write_index: begin,
                        end_of_pass_write_index: end,
                    })
                })
            };
            if redraw_terrain {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("spall-game-shadow-terrain-pass"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: self.pipeline.shadow_static_layer(cascade),
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: timestamp_writes(true, false),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_bind_group(0, &bind, &[]);
                if self.terrain_meshes_visible {
                    pass.set_pipeline(self.pipeline.shadow());
                    self.draw_terrain_meshes(&mut pass, &Frustum::from_view_projection(matrix));
                }
                drop(pass);
                self.shadow_cache[cascade] = Some(key);
            }
            self.pipeline.copy_static_shadow(encoder, cascade);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-game-shadow-pass"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: self.pipeline.shadow_layer(cascade),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: timestamp_writes(!redraw_terrain, true),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(self.pipeline.shadow_cube());
            pass.set_bind_group(0, &bind, &[]);
            if !self.body_meshes.is_empty() {
                pass.set_pipeline(self.pipeline.shadow());
                self.draw_body_meshes(&mut pass);
                pass.set_pipeline(self.pipeline.shadow_cube());
            }
            self.draw_cubes(&mut pass, &[&self.terrain, &self.bodies]);
            self.draw_vegetation(&mut pass, &Frustum::from_view_projection(matrix));
        }

        // Recompute sky visibility / bounce when the occupancy, environment or
        // materials changed since last frame.
        let (mut sky_ran, mut bounce_ran) = (false, false);
        if let Some(sky) = self.sky.as_ref() {
            sky.set_environment(queue, environment);
            sky.set_focus(camera.position);
            let writes = |begin, end| {
                timer.map(|timer| wgpu::ComputePassTimestampWrites {
                    query_set: &timer.queries,
                    beginning_of_pass_write_index: Some(begin),
                    end_of_pass_write_index: Some(end),
                })
            };
            (sky_ran, bounce_ran) = sky.dispatch(
                self.pipeline.sky_pipeline(),
                queue,
                encoder,
                writes(6, 7),
                writes(8, 9),
            );
        }

        let scene_bind = self.pipeline.scene_bind_group_lit(
            device,
            queue,
            &camera,
            view,
            environment,
            &self.materials,
        );
        let tone_bind = self.pipeline.tone_bind_group(
            device,
            queue,
            self.target.hdr_view(),
            environment.exposure,
            view != DebugView::Shaded,
        );
        let skybox_bind = (view == DebugView::Shaded)
            .then(|| self.pipeline.skybox_bind_group(queue, &camera, environment));
        if let Some(bind) = skybox_bind.as_ref() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-voxel-skybox-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: timer.map(|timer| wgpu::RenderPassTimestampWrites {
                    query_set: &timer.queries,
                    beginning_of_pass_write_index: Some(2),
                    end_of_pass_write_index: None,
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(self.pipeline.skybox_pipeline());
            pass.set_bind_group(0, *bind, &[]);
            pass.draw(0..3, 0..1);
        }
        // The chunks the camera pass draws: those in the frustum that are not
        // known to be hidden (see `occlusion`).
        let camera_chunks: Vec<(BrickCoord, Aabb)> = if self.terrain_meshes_visible {
            let frustum = camera.frustum();
            self.terrain_meshes
                .keys()
                .filter_map(|coord| {
                    let bounds = *self.terrain_bounds.get(coord)?;
                    frustum.intersects_aabb(bounds).then_some((*coord, bounds))
                })
                .collect()
        } else {
            Vec::new()
        };
        if self.occlusion_enabled {
            self.occlusion.begin_frame(device, camera.position);
        }
        let mut draw_chunks: Vec<BrickCoord> = camera_chunks
            .iter()
            .map(|(coord, _)| *coord)
            .filter(|coord| !self.occlusion_enabled || self.occlusion.should_draw(*coord))
            .collect();
        draw_chunks.sort_by_key(|coord| coord.sort_key());
        self.occlusion.drawn = draw_chunks.len();
        let clear = [0.0, 0.0, 0.0, 1.0];
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-game-opaque-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: if skybox_bind.is_some() {
                            wgpu::LoadOp::Load
                        } else {
                            wgpu::LoadOp::Clear(wgpu::Color {
                                r: clear[0],
                                g: clear[1],
                                b: clear[2],
                                a: clear[3],
                            })
                        },
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: self.target.depth_view(),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: timer.map(|timer| {
                    if skybox_bind.is_some() {
                        wgpu::RenderPassTimestampWrites {
                            query_set: &timer.queries,
                            beginning_of_pass_write_index: None,
                            end_of_pass_write_index: Some(3),
                        }
                    } else {
                        timer.writes(2, 3)
                    }
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.terrain_meshes_visible {
                pass.set_pipeline(self.pipeline.opaque_terrain());
                pass.set_bind_group(0, &scene_bind, &[]);
                pass.set_bind_group(1, &self.indirect.display_bind, &[]);
                pass.set_bind_group(
                    2,
                    self.sky.as_ref().map_or(
                        self.pipeline.sky_disabled_bind(),
                        SkyVisibility::display_bind,
                    ),
                    &[],
                );
                self.draw_terrain_list(&mut pass, &draw_chunks);
            }
            if !self.body_meshes.is_empty() {
                pass.set_pipeline(self.pipeline.opaque());
                pass.set_bind_group(0, &scene_bind, &[]);
                pass.set_bind_group(1, &self.indirect.display_bind, &[]);
                pass.set_bind_group(
                    2,
                    self.sky.as_ref().map_or(
                        self.pipeline.sky_disabled_bind(),
                        SkyVisibility::display_bind,
                    ),
                    &[],
                );
                self.draw_body_meshes(&mut pass);
            }
            pass.set_pipeline(self.pipeline.opaque_cube());
            pass.set_bind_group(0, &scene_bind, &[]);
            pass.set_bind_group(1, &self.indirect.display_bind, &[]);
            pass.set_bind_group(
                2,
                self.sky.as_ref().map_or(
                    self.pipeline.sky_disabled_bind(),
                    SkyVisibility::display_bind,
                ),
                &[],
            );
            self.draw_cubes(&mut pass, &[&self.terrain, &self.bodies, &self.overlay]);
            self.draw_vegetation(&mut pass, &camera.frustum());
        }
        let water_grid_cells = if self.water_grid {
            self.pipeline.water_field_dim()
        } else {
            0
        };
        if self.occlusion_enabled && self.terrain_meshes_visible {
            self.occlusion.encode(
                queue,
                encoder,
                self.target.depth_view(),
                camera.view_projection(),
                &camera_chunks,
                camera.position,
            );
        }
        if self.transparent.count() > 0
            || self.water_surface.index_count > 0
            || water_grid_cells > 0
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-game-transparent-cube-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: self.target.depth_view(),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(self.pipeline.transparent_cube());
            pass.set_bind_group(0, &scene_bind, &[]);
            pass.set_bind_group(1, &self.indirect.display_bind, &[]);
            pass.set_bind_group(
                2,
                self.sky.as_ref().map_or(
                    self.pipeline.sky_disabled_bind(),
                    SkyVisibility::display_bind,
                ),
                &[],
            );
            self.draw_cubes(&mut pass, &[&self.transparent]);
            if self.water_surface.index_count > 0 {
                // Same bind groups as the cubes above.
                pass.set_pipeline(self.pipeline.water_surface());
                pass.set_vertex_buffer(0, self.water_surface.vertices.slice(..));
                pass.set_index_buffer(
                    self.water_surface.indices.slice(..),
                    wgpu::IndexFormat::Uint32,
                );
                pass.draw_indexed(0..self.water_surface.index_count, 0, 0..1);
            }
            if water_grid_cells > 0 {
                pass.set_pipeline(self.pipeline.water_grid());
                pass.draw(0..water_grid_cells * water_grid_cells * 6, 0..1);
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-game-tone-map-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: timer.map(|timer| timer.writes(4, 5)),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(self.pipeline.tone_map());
            pass.set_bind_group(0, &tone_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        if let (Some(timer), Some(slot)) = (self.timer.as_mut(), self.timed_slot) {
            timer.resolve(encoder, slot, sky_ran, bounce_ran);
        }
    }

    /// Start the asynchronous timestamp readback for the frame just recorded.
    /// Call once after submitting the encoder passed to [`Self::render`].
    pub fn finish_timing(&mut self) {
        // The occlusion batch recorded with this frame is read back too.
        self.occlusion.after_submit();
        if let (Some(timer), Some(slot)) = (self.timer.as_mut(), self.timed_slot.take()) {
            timer.map(slot);
        }
    }

    fn draw_cubes<'pass>(
        &'pass self,
        pass: &mut wgpu::RenderPass<'pass>,
        sets: &[&'pass InstanceSet],
    ) {
        pass.set_vertex_buffer(0, self.cube_vertices.slice(..));
        pass.set_index_buffer(self.cube_indices.slice(..), wgpu::IndexFormat::Uint16);
        for set in sets {
            if let Some(buffer) = set.buffer() {
                pass.set_vertex_buffer(1, buffer.slice(..));
                pass.draw_indexed(0..self.cube_index_count, 0, 0..set.count());
            }
        }
    }

    fn draw_terrain_meshes<'pass>(
        &'pass self,
        pass: &mut wgpu::RenderPass<'pass>,
        frustum: &Frustum,
    ) {
        // Use each pass's own frustum: an off-camera caster can still be in
        // a sun cascade. No terrain residency or authoritative voxels change.
        let mut chunks: Vec<_> = self
            .terrain_meshes
            .iter()
            .filter(|(coord, _)| {
                self.terrain_bounds
                    .get(coord)
                    .is_none_or(|bounds| frustum.intersects_aabb(*bounds))
            })
            .collect();
        chunks.sort_by_key(|(coord, _)| coord.sort_key());
        for (_, mesh) in chunks {
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
            pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }

    /// Draws the foliage squares inside `frustum`.
    fn draw_vegetation<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>, frustum: &Frustum) {
        let Some(buffer) = self.vegetation.buffer() else {
            return;
        };
        pass.set_vertex_buffer(0, self.cube_vertices.slice(..));
        pass.set_index_buffer(self.cube_indices.slice(..), wgpu::IndexFormat::Uint16);
        pass.set_vertex_buffer(1, buffer.slice(..));
        for (bounds, first, count) in &self.vegetation_buckets {
            if frustum.intersects_aabb(*bounds) {
                pass.draw_indexed(0..self.cube_index_count, 0, *first..*first + *count);
            }
        }
    }

    fn draw_terrain_list<'pass>(
        &'pass self,
        pass: &mut wgpu::RenderPass<'pass>,
        chunks: &[BrickCoord],
    ) {
        for coord in chunks {
            if let Some(mesh) = self.terrain_meshes.get(coord) {
                pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
                pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
    }

    fn draw_body_meshes<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        let mut meshes: Vec<_> = self.body_meshes.iter().collect();
        meshes.sort_by_key(|(entity, _)| **entity);
        for (_, mesh) in meshes {
            pass.set_vertex_buffer(0, mesh.vertices.slice(..));
            pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }
}
