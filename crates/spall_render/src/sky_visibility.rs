//! Sky visibility and one diffuse bounce over a camera-local occupancy grid.
//!
//! The occupancy is a [`LightingVolume`] (the frozen T13 layout: 128^3 cells of
//! 0.5 m, one `u32` per cell, `0` = air). It is *derived, client-local* data: it
//! never affects collision, topology, replication or saved geometry. This module
//! owns a GPU copy of the occupancy and two compute passes over it:
//!
//! 1. **Sky visibility** (`sky_visibility.wgsl`): what fraction of the sky each
//!    of six directions sees from every air cell near a surface, so skylight
//!    reaches open ground and stops at roofs and walls.
//! 2. **Bounce** (`bounce.wgsl`): one diffuse bounce whose sources are lit from
//!    the actual sun (shadow rays through the occupancy), the actual sky
//!    visibility, and material emission. It does not count skylight itself
//!    (the opaque pass applies that from sky visibility), so nothing is counted
//!    twice. Distant emitters are lit analytically from a small list of
//!    emissive bins gathered before each slice (`emitters.wgsl`), because a lamp
//!    is too small a target for a cell's rays to hit reliably.
//!
//! Both feed one bind group the opaque pass samples. A renderer with no
//! occupancy binds the disabled stub and the shader falls back to the legacy
//! hemispheric ambient with no bounce.
//!
//! Unknown space is explicit: an occupancy builder marks cells it cannot vouch
//! for (a brick that is not resident) with a non-zero value, so they block rays
//! and emit nothing instead of being treated as open air.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use glam::Vec3;

use crate::environment::Environment;
use crate::indirect::{LIGHT_CELL_SIZE_METRES, LIGHT_VOLUME_DIM, LightingVolume};
use crate::instances::CubeInstance;

/// Longest ray, in cells (24 m). A ray this long without a hit has reached the
/// sky; anything further is beyond the shading distance that matters.
pub const SKY_MAX_RAY_CELLS: u32 = 48;

/// Most emissive bins the bounce pass lights analytically. More than this and
/// the pass falls back to sampling emission with rays (see `emitters.wgsl`).
pub const EMITTER_CAPACITY: u32 = 340;
/// Cells per emitter bin along each axis (2 m bins).
const EMITTER_BIN_CELLS: u32 = 4;
/// `u32`s per emitter bin in the gather buffer, and bytes per emitter record.
const EMITTER_BIN_STRIDE: u64 = 16;
const EMITTER_RECORD_BYTES: u64 = 48;
/// Bytes of the emitter records buffer and its uniform copy: a 16-byte header
/// then one record per emitter.
const EMITTER_BLOCK_BYTES: u64 = 16 + EMITTER_CAPACITY as u64 * EMITTER_RECORD_BYTES;

/// Constants the emitter and bounce shaders share with this module.
fn shader_constants() -> String {
    format!(
        "const DIM: i32 = {LIGHT_VOLUME_DIM};\nconst MAX_EMITTERS: u32 = {EMITTER_CAPACITY}u;\n"
    )
}

/// Occupancy value for a cell the builder cannot vouch for. Any non-zero value
/// blocks; this one is distinguishable in debugging and counts.
pub const UNKNOWN_CELL: u32 = u32::MAX;

/// Where to put the camera-local cache so `center` (metres) sits inside it:
/// 32 m either side horizontally, 16 m below and 48 m above vertically,
/// snapped to the 0.5 m grid so world voxels of 0.25 m align with cells.
pub fn cache_origin_around(center: Vec3) -> Vec3 {
    let snap = |v: f32| (v / LIGHT_CELL_SIZE_METRES).floor() * LIGHT_CELL_SIZE_METRES;
    let half = LIGHT_VOLUME_DIM as f32 * LIGHT_CELL_SIZE_METRES * 0.5;
    Vec3::new(
        snap(center.x - half),
        snap(center.y - 16.0),
        snap(center.z - half),
    )
}

/// Mark every cache cell overlapped by the world box `[min_m, max_m)` with
/// `marker` (a material id, or [`UNKNOWN_CELL`]). Conservative: a cell is
/// occupied if *any* part of it is, so a wall thinner than a cell still blocks
/// (over-occlusion, never leakage). Cells outside the cache are ignored.
pub fn mark_world_box(volume: &mut LightingVolume, min_m: Vec3, max_m: Vec3, marker: u32) {
    let lo = volume.world_to_cell(min_m);
    let hi = volume.world_to_cell(max_m - Vec3::splat(1.0e-4));
    volume.fill_box(lo, hi + glam::IVec3::ONE, marker);
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SkyGlobals {
    origin_cell_size: [f32; 4],
    /// `x` cells per axis, `y` enabled, `z` max ray cells, `w` bounce enabled.
    dimensions: [u32; 4],
    /// `x..y` half-open z range a dispatch may write.
    region: [u32; 4],
}

/// Byte offsets of `region` in [`SkyGlobals`] and [`BounceGlobals`].
const SKY_REGION_OFFSET: u64 = 32;
const BOUNCE_REGION_OFFSET: u64 = 96;

/// Cells (z) per time slice, so a sliced sweep takes `128 / 16 = 8` frames.
pub const SLICE_DEPTH: u32 = 16;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BounceGlobals {
    origin_cell_size: [f32; 4],
    /// `x` cells per axis, `y` max ray cells.
    dimensions: [u32; 4],
    sun_dir: [f32; 4],
    sun_radiance: [f32; 4],
    sky_color: [f32; 4],
    ground_color: [f32; 4],
    /// `x..y` half-open z range a dispatch may write.
    region: [u32; 4],
}

pub(crate) struct SkyPipeline {
    compute: wgpu::ComputePipeline,
    compute_layout: wgpu::BindGroupLayout,
    bounce: wgpu::ComputePipeline,
    bounce_layout: wgpu::BindGroupLayout,
    emit_gather: wgpu::ComputePipeline,
    emit_compact: wgpu::ComputePipeline,
    emit_finish: wgpu::ComputePipeline,
    emit_layout: wgpu::BindGroupLayout,
    display_layout: wgpu::BindGroupLayout,
}

/// The emissive-bin gather buffers shared by the emitter and bounce passes.
struct EmitterBuffers {
    /// Per-bin accumulators, cleared before every gather.
    bins: wgpu::Buffer,
    /// Header plus one record per populated bin, written by the gather.
    records: wgpu::Buffer,
    /// The same block as a uniform, copied from `records` for the bounce pass.
    uniform: wgpu::Buffer,
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

impl SkyPipeline {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let compute_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-sky-compute-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(1, false, wgpu::ShaderStages::COMPUTE),
                uniform_entry(2, wgpu::ShaderStages::COMPUTE),
            ],
        });
        let bounce_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-bounce-compute-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(1, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(2, true, wgpu::ShaderStages::COMPUTE),
                storage_entry(3, false, wgpu::ShaderStages::COMPUTE),
                uniform_entry(4, wgpu::ShaderStages::COMPUTE),
                uniform_entry(5, wgpu::ShaderStages::COMPUTE), // emitter block
            ],
        });
        let emit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-emitter-compute-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::COMPUTE), // cells
                storage_entry(1, true, wgpu::ShaderStages::COMPUTE), // materials
                storage_entry(2, false, wgpu::ShaderStages::COMPUTE), // bins
                storage_entry(3, false, wgpu::ShaderStages::COMPUTE), // records
            ],
        });
        let display_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-sky-display-layout"),
            entries: &[
                storage_entry(0, true, wgpu::ShaderStages::FRAGMENT), // visibility
                uniform_entry(1, wgpu::ShaderStages::FRAGMENT),       // globals
                storage_entry(2, true, wgpu::ShaderStages::FRAGMENT), // bounce radiance
            ],
        });
        let make =
            |label: &str, source: String, layout: &wgpu::BindGroupLayout, entry_point: &str| {
                let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(label),
                    source: wgpu::ShaderSource::Wgsl(source.into()),
                });
                let pipeline_layout =
                    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some(label),
                        bind_group_layouts: &[Some(layout)],
                        immediate_size: 0,
                    });
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
                    module: &shader,
                    entry_point: Some(entry_point),
                    compilation_options: Default::default(),
                    cache: None,
                })
            };
        let dda = include_str!("shaders/voxel_dda.wgsl");
        let compute = make(
            "spall-sky-visibility-pipeline",
            [dda, include_str!("shaders/sky_visibility.wgsl")].concat(),
            &compute_layout,
            "sky_visibility_main",
        );
        let constants = shader_constants();
        let bounce = make(
            "spall-bounce-pipeline",
            [constants.as_str(), dda, include_str!("shaders/bounce.wgsl")].concat(),
            &bounce_layout,
            "bounce_main",
        );
        let emitter_source = [constants.as_str(), include_str!("shaders/emitters.wgsl")].concat();
        let emit_gather = make(
            "spall-emitter-gather-pipeline",
            emitter_source.clone(),
            &emit_layout,
            "gather_main",
        );
        let emit_compact = make(
            "spall-emitter-compact-pipeline",
            emitter_source.clone(),
            &emit_layout,
            "compact_main",
        );
        let emit_finish = make(
            "spall-emitter-finish-pipeline",
            emitter_source,
            &emit_layout,
            "finish_main",
        );
        Self {
            compute,
            compute_layout,
            bounce,
            bounce_layout,
            emit_gather,
            emit_compact,
            emit_finish,
            emit_layout,
            display_layout,
        }
    }

    pub(crate) fn display_layout(&self) -> &wgpu::BindGroupLayout {
        &self.display_layout
    }

    /// A one-cell disabled stub: the shader sees `enabled == 0` and uses the
    /// legacy hemispheric ambient. Returned buffers must outlive the bind group.
    pub(crate) fn disabled(&self, device: &wgpu::Device) -> (wgpu::BindGroup, Vec<wgpu::Buffer>) {
        let stub = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: 16,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let cells = stub("spall-sky-disabled-cells");
        let visibility = stub("spall-sky-disabled-visibility");
        let radiance = stub("spall-sky-disabled-radiance");
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-sky-disabled-globals"),
            size: std::mem::size_of::<SkyGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        // Zero-initialised: `dimensions.y == 0` means disabled.
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-sky-disabled-bind"),
            layout: &self.display_layout,
            entries: &[
                entry(0, &visibility),
                entry(1, &globals),
                entry(2, &radiance),
            ],
        });
        (bind, vec![cells, visibility, radiance, globals])
    }

    #[allow(clippy::too_many_arguments)]
    fn bounce_bind(
        &self,
        device: &wgpu::Device,
        cells: &wgpu::Buffer,
        visibility: &wgpu::Buffer,
        materials: &wgpu::Buffer,
        radiance: &wgpu::Buffer,
        globals: &wgpu::Buffer,
        emitters: &EmitterBuffers,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-bounce-compute-bind"),
            layout: &self.bounce_layout,
            entries: &[
                entry(0, cells),
                entry(1, visibility),
                entry(2, materials),
                entry(3, radiance),
                entry(4, globals),
                entry(5, &emitters.uniform),
            ],
        })
    }

    fn emit_bind(
        &self,
        device: &wgpu::Device,
        cells: &wgpu::Buffer,
        materials: &wgpu::Buffer,
        emitters: &EmitterBuffers,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-emitter-compute-bind"),
            layout: &self.emit_layout,
            entries: &[
                entry(0, cells),
                entry(1, materials),
                entry(2, &emitters.bins),
                entry(3, &emitters.records),
            ],
        })
    }

    pub(crate) fn create(&self, device: &wgpu::Device, materials: &wgpu::Buffer) -> SkyVisibility {
        let cell_count = u64::from(LIGHT_VOLUME_DIM).pow(3);
        let buffer = |label, size, usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let cells = buffer(
            "spall-sky-occupancy",
            cell_count * 4,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        );
        let globals = buffer(
            "spall-sky-globals",
            std::mem::size_of::<SkyGlobals>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        // Shading reads its own copy: during a re-centre the compute globals
        // already hold the new origin while shading still samples the old cache.
        let display_globals = buffer(
            "spall-sky-display-globals",
            std::mem::size_of::<SkyGlobals>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let bounce_globals = buffer(
            "spall-bounce-globals",
            std::mem::size_of::<BounceGlobals>() as u64,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let bin_count = u64::from(LIGHT_VOLUME_DIM / EMITTER_BIN_CELLS).pow(3);
        let emitters = EmitterBuffers {
            bins: buffer(
                "spall-emitter-bins",
                (bin_count * EMITTER_BIN_STRIDE + 1) * 4,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            ),
            records: buffer(
                "spall-emitter-records",
                EMITTER_BLOCK_BYTES,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            ),
            uniform: buffer(
                "spall-emitter-block",
                EMITTER_BLOCK_BYTES,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ),
        };
        let emit_bind = self.emit_bind(device, &cells, materials, &emitters);
        // Two result sets. Recomputes in place (bodies, terrain edits) write the
        // front set the opaque pass is reading; re-centring the cache writes the
        // back set and swaps when it is complete, so shading never sees a
        // half-moved cache.
        let make_set = || {
            // Six unorm8 visibilities per cell in two u32 words.
            let visibility = buffer(
                "spall-sky-visibility",
                cell_count * 8,
                wgpu::BufferUsages::STORAGE,
            );
            // Six shared-exponent RGB (rgb9e5, 4 bytes) face radiances per cell.
            let radiance = buffer(
                "spall-bounce-radiance",
                cell_count * 24,
                wgpu::BufferUsages::STORAGE,
            );
            let compute_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("spall-sky-compute-bind"),
                layout: &self.compute_layout,
                entries: &[entry(0, &cells), entry(1, &visibility), entry(2, &globals)],
            });
            let display_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("spall-sky-display-bind"),
                layout: &self.display_layout,
                entries: &[
                    entry(0, &visibility),
                    entry(1, &display_globals),
                    entry(2, &radiance),
                ],
            });
            let bounce_bind = self.bounce_bind(
                device,
                &cells,
                &visibility,
                materials,
                &radiance,
                &bounce_globals,
                &emitters,
            );
            (
                ResultSet {
                    _visibility: visibility,
                    _radiance: radiance,
                    compute_bind,
                    display_bind,
                },
                bounce_bind,
            )
        };
        let ((set0, bounce0), (set1, bounce1)) = (make_set(), make_set());
        SkyVisibility {
            cells,
            globals,
            display_globals,
            bounce_globals,
            sets: [set0, set1],
            bounce_binds: Mutex::new([bounce0, bounce1]),
            emitters,
            emit_bind: Mutex::new(emit_bind),
            front: AtomicUsize::new(0),
            state: Mutex::new(State {
                origin: Vec3::ZERO,
                display_origin: Vec3::ZERO,
                deferred: None,
                last_cubes: Vec::new(),
                bodies_moved: false,
                environment: None,
                bounce_enabled: true,
                populated: false,
                base: Vec::new(),
                bodies: HashMap::new(),
                pending: Pending::default(),
                running: None,
                sweeps_completed: 0,
                recentres_completed: 0,
                frames_in_sweep: 0,
                last_sweep_frames: 0,
                change_since: None,
                sweep_change_since: None,
                last_latency_ms: None,
            }),
        }
    }
}

/// One set of results and the bind groups that read and write it.
struct ResultSet {
    _visibility: wgpu::Buffer,
    _radiance: wgpu::Buffer,
    compute_bind: wgpu::BindGroup,
    display_bind: wgpu::BindGroup,
}

/// What the next sweep must recompute.
#[derive(Default, Clone, Copy)]
struct Pending {
    /// A full-cache recompute is wanted in one dispatch (first population,
    /// environment or material changes).
    full: bool,
    /// Occupancy changed in place: recompute sky visibility and bounce.
    occupancy: bool,
    /// Only the bounce is stale (environment, materials, toggle).
    bounce_only: bool,
    /// The cache origin moved: rebuild everything into the back set and swap.
    recentre: bool,
}

/// A sweep in progress.
#[derive(Clone, Copy)]
struct Running {
    /// The next z slice to process (unused when `full`).
    next_slice: u32,
    full: bool,
    sky: bool,
    bounce: bool,
    /// Writes the back set and swaps when finished (a re-centre).
    recentre: bool,
}

struct State {
    /// The origin of the occupancy buffer (new terrain and bodies are laid out
    /// against it).
    origin: Vec3,
    /// The origin the opaque pass samples the front set with. Trails `origin`
    /// while a re-centre is being computed.
    display_origin: Vec3,
    /// The newest grid that arrived while a re-centre was pending or running;
    /// applied when that finishes.
    deferred: Option<LightingVolume>,
    /// The body cubes last supplied, to lay back over a re-centred cache.
    last_cubes: Vec<CubeInstance>,
    /// Bodies moved while a re-centre was running (so the swap must light them).
    bodies_moved: bool,
    /// The environment the bounce sources were last lit with.
    environment: Option<Environment>,
    bounce_enabled: bool,
    populated: bool,
    /// The terrain occupancy last supplied, so a body that leaves a cell
    /// restores what was underneath instead of erasing it.
    base: Vec<u32>,
    /// Cells currently overridden by bodies, `cell index -> material`.
    bodies: HashMap<u32, u32>,
    pending: Pending,
    running: Option<Running>,
    sweeps_completed: u64,
    recentres_completed: u64,
    frames_in_sweep: u32,
    last_sweep_frames: u32,
    /// When the oldest not-yet-lit change arrived (CPU clock).
    change_since: Option<Instant>,
    /// The change time the running sweep is lighting.
    sweep_change_since: Option<Instant>,
    /// Milliseconds from a change arriving to the sweep that lit it finishing
    /// being recorded (CPU clock; excludes the GPU finishing that frame and
    /// presentation).
    last_latency_ms: Option<f64>,
}

impl State {
    fn note_change(&mut self) {
        if self.change_since.is_none() {
            self.change_since = Some(Instant::now());
        }
    }

    fn recentring(&self) -> bool {
        self.pending.recentre || self.running.is_some_and(|run| run.recentre)
    }
}

/// GPU occupancy, sky visibility and bounce radiance for one camera-local cache.
///
/// Terrain arrives with [`Self::set_occupancy`]; moving bodies are laid over it
/// each frame with [`Self::set_bodies`] as an exact cell diff (old and new
/// cells are both updated, and a vacated cell gets the terrain value back, so
/// overlapping objects are never erased). Any change starts a *sweep* that
/// recomputes sky visibility and bounce over every z slice, a slice per frame,
/// so a moving body costs 1/8 of a full recompute per frame instead of all of
/// it. A sweep recomputes the whole cache, which is a superset of the cells
/// whose rays could pass through the changed cells (rays reach 24 m), so no
/// lighting beyond the edited bounds is left stale.
///
/// Moving the cache origin (the camera walked away from its centre) is a
/// sweep into a second, back result set that is swapped in when complete, so
/// re-centring costs the same 1/8 per frame instead of one whole-cache hitch,
/// and shading keeps the old, consistent cache until the new one is ready.
pub struct SkyVisibility {
    cells: wgpu::Buffer,
    /// Compute-pass globals: the origin being computed for, and the slice region.
    globals: wgpu::Buffer,
    /// Shading globals: the origin of the front result set.
    display_globals: wgpu::Buffer,
    bounce_globals: wgpu::Buffer,
    sets: [ResultSet; 2],
    /// The bounce pass's bind group per result set; recreated when the material
    /// table changes.
    bounce_binds: Mutex<[wgpu::BindGroup; 2]>,
    /// Emissive bins gathered before each bounce slice.
    emitters: EmitterBuffers,
    /// The emitter gather's bind group; recreated with the material table.
    emit_bind: Mutex<wgpu::BindGroup>,
    /// Index of the result set the opaque pass reads.
    front: AtomicUsize,
    state: Mutex<State>,
}

/// The cache cells a body cube covers: its rotated bounding box, conservatively
/// (a cell is occupied if any part of the box touches it).
fn cube_cells(origin: Vec3, cube: &CubeInstance, mut visit: impl FnMut(u32)) {
    let q = glam::Quat::from_array(cube.rotation);
    let m = glam::Mat3::from_quat(q);
    let half = Vec3::from_array(cube.size) * 0.5;
    let extent = Vec3::new(
        m.x_axis.x.abs() * half.x + m.y_axis.x.abs() * half.y + m.z_axis.x.abs() * half.z,
        m.x_axis.y.abs() * half.x + m.y_axis.y.abs() * half.y + m.z_axis.y.abs() * half.z,
        m.x_axis.z.abs() * half.x + m.y_axis.z.abs() * half.y + m.z_axis.z.abs() * half.z,
    );
    let centre = Vec3::from_array(cube.offset);
    let to_cell = |p: Vec3| ((p - origin) / LIGHT_CELL_SIZE_METRES).floor().as_ivec3();
    let lo = to_cell(centre - extent).max(glam::IVec3::ZERO);
    let hi = to_cell(centre + extent - Vec3::splat(1.0e-4))
        .min(glam::IVec3::splat(LIGHT_VOLUME_DIM as i32 - 1));
    let d = LIGHT_VOLUME_DIM as i32;
    for z in lo.z..=hi.z {
        for y in lo.y..=hi.y {
            for x in lo.x..=hi.x {
                visit((x + d * (y + d * z)) as u32);
            }
        }
    }
}

/// Cells overridden by `cubes` (later cubes win where they overlap).
fn body_cell_map(origin: Vec3, cubes: &[CubeInstance]) -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    for cube in cubes {
        cube_cells(origin, cube, |index| {
            map.insert(index, cube.material);
        });
    }
    map
}

/// The cells whose final occupancy differs between two body maps laid over
/// `base`, as sorted `(index, new value)` pairs. A cell a body vacated returns
/// to its `base` value; a cell two bodies shared stays occupied when only one
/// leaves.
fn diff_body_cells(
    base: &[u32],
    old: &HashMap<u32, u32>,
    new: &HashMap<u32, u32>,
) -> Vec<(u32, u32)> {
    let value = |map: &HashMap<u32, u32>, index: u32| {
        map.get(&index)
            .copied()
            .unwrap_or_else(|| base.get(index as usize).copied().unwrap_or(0))
    };
    let mut changed: Vec<(u32, u32)> = old
        .keys()
        .chain(new.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|index| {
            let (before, after) = (value(old, index), value(new, index));
            (before != after).then_some((index, after))
        })
        .collect();
    changed.sort_unstable();
    changed
}

impl SkyVisibility {
    fn sky_globals(state: &State, origin: Vec3) -> SkyGlobals {
        SkyGlobals {
            origin_cell_size: [origin.x, origin.y, origin.z, LIGHT_CELL_SIZE_METRES],
            dimensions: [
                LIGHT_VOLUME_DIM,
                1,
                SKY_MAX_RAY_CELLS,
                u32::from(state.bounce_enabled),
            ],
            region: [0, LIGHT_VOLUME_DIM, 0, 0],
        }
    }

    /// Point the compute passes at the occupancy buffer's origin.
    fn write_compute_globals(&self, queue: &wgpu::Queue, state: &State) {
        let globals = Self::sky_globals(state, state.origin);
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
    }

    /// Point shading at the front result set's origin.
    fn write_display_globals(&self, queue: &wgpu::Queue, state: &State) {
        let globals = Self::sky_globals(state, state.display_origin);
        queue.write_buffer(&self.display_globals, 0, bytemuck::bytes_of(&globals));
    }

    fn write_bounce_globals(&self, queue: &wgpu::Queue, state: &State, environment: &Environment) {
        let o = state.origin;
        let [sr, sg, sb] = environment.sun_color;
        let [kr, kg, kb] = environment.sky;
        let [gr, gg, gb] = environment.ground;
        let globals = BounceGlobals {
            origin_cell_size: [o.x, o.y, o.z, LIGHT_CELL_SIZE_METRES],
            dimensions: [LIGHT_VOLUME_DIM, SKY_MAX_RAY_CELLS, 0, 0],
            sun_dir: environment.sun_dir.extend(0.0).to_array(),
            sun_radiance: [
                sr * environment.sun_intensity,
                sg * environment.sun_intensity,
                sb * environment.sun_intensity,
                0.0,
            ],
            sky_color: [kr, kg, kb, 0.0],
            ground_color: [gr, gg, gb, 0.0],
            region: [0, LIGHT_VOLUME_DIM, 0, 0],
        };
        queue.write_buffer(&self.bounce_globals, 0, bytemuck::bytes_of(&globals));
    }

    /// Write cell values, coalescing consecutive indices into single uploads.
    fn write_cells(&self, queue: &wgpu::Queue, writes: &[(u32, u32)]) {
        let mut i = 0;
        while i < writes.len() {
            let start = writes[i].0;
            let mut run = vec![writes[i].1];
            while i + 1 < writes.len() && writes[i + 1].0 == start + run.len() as u32 {
                i += 1;
                run.push(writes[i].1);
            }
            queue.write_buffer(
                &self.cells,
                u64::from(start) * 4,
                bytemuck::cast_slice(&run),
            );
            i += 1;
        }
    }

    /// Make `volume` the occupancy buffer's content: upload it, remember it as
    /// the terrain base, and lay the current bodies (re-derived for its origin)
    /// back over it.
    fn apply_grid(&self, queue: &wgpu::Queue, state: &mut State, volume: &LightingVolume) {
        state.origin = volume.origin();
        self.write_compute_globals(queue, state);
        state.base = volume.cells().to_vec();
        queue.write_buffer(&self.cells, 0, bytemuck::cast_slice(volume.cells()));
        let bodies = body_cell_map(state.origin, &state.last_cubes);
        let mut overlay: Vec<(u32, u32)> = bodies.iter().map(|(i, m)| (*i, *m)).collect();
        overlay.sort_unstable();
        self.write_cells(queue, &overlay);
        state.bodies = bodies;
        if let Some(environment) = state.environment {
            self.write_bounce_globals(queue, state, &environment);
        }
    }

    /// Supply the terrain occupancy. The first population is recomputed in one
    /// dispatch. A grid at the same origin (a terrain edit) is a time-sliced
    /// sweep in place. A grid at a *different* origin (the camera walked away
    /// from the cache's centre) is a re-centre: it is applied when its turn comes
    /// and recomputed, a slice per frame, into the back result set, which is
    /// swapped in when complete; until then shading keeps using the old cache.
    /// Bodies already supplied are laid back over the new terrain.
    pub fn set_occupancy(&self, queue: &wgpu::Queue, volume: &LightingVolume) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.populated {
            state.display_origin = volume.origin();
            self.apply_grid(queue, &mut state, volume);
            self.write_display_globals(queue, &state);
            state.populated = true;
            state.pending.occupancy = true;
            state.pending.full = true;
            state.note_change();
            return;
        }
        if state.recentring() {
            // A re-centre owns the occupancy buffer until it finishes.
            state.deferred = Some(volume.clone());
            state.pending.recentre = true;
            state.note_change();
            return;
        }
        if volume.origin() != state.origin {
            state.deferred = Some(volume.clone());
            state.pending.recentre = true;
            state.note_change();
            return;
        }
        self.apply_grid(queue, &mut state, volume);
        state.pending.occupancy = true;
        state.note_change();
    }

    /// Lay `cubes` (moving bodies, detached debris) over the terrain occupancy
    /// as occluders and bounce sources. Call every frame with the current
    /// poses; only cells whose occupancy actually changed are uploaded, and a
    /// change starts a time-sliced sweep. Returns the number of cells changed.
    pub fn set_bodies(&self, queue: &wgpu::Queue, cubes: &[CubeInstance]) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.populated {
            return 0;
        }
        state.last_cubes = cubes.to_vec();
        let new = body_cell_map(state.origin, cubes);
        if state.recentring() {
            // The occupancy buffer is being rebuilt for a new origin; the
            // bodies are laid over it when that grid is applied. Remember
            // whether they moved since, so the swap can light them.
            if new != state.bodies {
                state.bodies_moved = true;
            }
            return 0;
        }
        let changed = diff_body_cells(&state.base, &state.bodies, &new);
        state.bodies = new;
        if changed.is_empty() {
            return 0;
        }
        self.write_cells(queue, &changed);
        state.pending.occupancy = true;
        state.note_change();
        changed.len()
    }

    /// Light the bounce sources with `environment`. Cheap when unchanged; a
    /// change (a different sun, sky or intensity) schedules a bounce recompute.
    pub fn set_environment(&self, queue: &wgpu::Queue, environment: &Environment) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.environment.as_ref() == Some(environment) {
            return;
        }
        state.environment = Some(*environment);
        self.write_bounce_globals(queue, &state, environment);
        state.pending.bounce_only = true;
        state.pending.full = true;
        state.note_change();
    }

    /// Switch the bounce on or off. Off skips the pass and the shader adds no
    /// bounce; sky visibility is unaffected.
    pub fn set_bounce_enabled(&self, queue: &wgpu::Queue, enabled: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.bounce_enabled == enabled {
            return;
        }
        state.bounce_enabled = enabled;
        self.write_compute_globals(queue, &state);
        self.write_display_globals(queue, &state);
        if enabled {
            state.pending.bounce_only = true;
            state.pending.full = true;
            state.note_change();
        }
    }

    /// Point the bounce pass at a new material table.
    pub(crate) fn rebind_materials(
        &self,
        device: &wgpu::Device,
        pipeline: &SkyPipeline,
        materials: &wgpu::Buffer,
    ) {
        let mut binds = self.bounce_binds.lock().unwrap_or_else(|e| e.into_inner());
        for (index, set) in self.sets.iter().enumerate() {
            binds[index] = pipeline.bounce_bind(
                device,
                &self.cells,
                &set._visibility,
                materials,
                &set._radiance,
                &self.bounce_globals,
                &self.emitters,
            );
        }
        drop(binds);
        *self.emit_bind.lock().unwrap_or_else(|e| e.into_inner()) =
            pipeline.emit_bind(device, &self.cells, materials, &self.emitters);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.bounce_only = true;
        state.pending.full = true;
        state.note_change();
    }

    /// True when occupancy has been supplied and shading should use it.
    pub fn is_populated(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .populated
    }

    /// Sweeps that have run to completion.
    pub fn sweeps_completed(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sweeps_completed
    }

    /// Re-centres (origin moves) that have completed and been swapped in.
    pub fn recentres_completed(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recentres_completed
    }

    /// Frames the most recent completed sweep took from start to finish (1 for
    /// a full-cache dispatch, `128 / SLICE_DEPTH` for a sliced one). Add the
    /// frames a change waited for a running sweep to finish for the worst case.
    pub fn last_sweep_frames(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_sweep_frames
    }

    /// Milliseconds from the last change reaching the cache to the recompute
    /// that lit it being recorded (CPU clock, so it excludes the GPU finishing
    /// that frame and presentation). `None` before the first sweep.
    pub fn last_latency_ms(&self) -> Option<f64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_latency_ms
    }

    /// Whether a sweep is running or queued.
    pub fn is_sweeping(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running.is_some()
            || state.pending.full
            || state.pending.occupancy
            || state.pending.bounce_only
            || state.pending.recentre
    }

    /// GPU memory held: occupancy plus two result sets (visibility and
    /// bounce radiance each).
    pub fn allocated_bytes(&self) -> u64 {
        let cells = u64::from(LIGHT_VOLUME_DIM).pow(3);
        let bins = u64::from(LIGHT_VOLUME_DIM / EMITTER_BIN_CELLS).pow(3);
        cells * 4
            + 2 * cells * (8 + 24)
            + (bins * EMITTER_BIN_STRIDE + 1) * 4
            + 2 * EMITTER_BLOCK_BYTES
            + 16
    }

    pub(crate) fn display_bind(&self) -> &wgpu::BindGroup {
        &self.sets[self.front.load(Ordering::Acquire)].display_bind
    }

    /// Record this frame's share of the recompute. A full request recomputes
    /// the whole cache now; otherwise a running (or newly started) sweep
    /// recomputes one z slice; a re-centre writes the back set and swaps when
    /// its last slice is recorded. Returns `(sky_ran, bounce_ran)`.
    pub(crate) fn dispatch<'a>(
        &self,
        pipeline: &'a SkyPipeline,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        sky_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
        bounce_timestamps: Option<wgpu::ComputePassTimestampWrites<'a>>,
    ) -> (bool, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.populated {
            return (false, false);
        }
        let can_bounce = state.bounce_enabled && state.environment.is_some();

        // Start a sweep from whatever is pending, unless one is running.
        if state.running.is_none() {
            if state.pending.recentre {
                // Apply the newest grid, then rebuild the back set from it.
                let Some(grid) = state.deferred.take() else {
                    state.pending.recentre = false;
                    return (false, false);
                };
                state.pending.recentre = false;
                self.apply_grid(queue, &mut state, &grid);
                state.running = Some(Running {
                    next_slice: 0,
                    full: false,
                    sky: true,
                    bounce: can_bounce,
                    recentre: true,
                });
                state.frames_in_sweep = 0;
                state.sweep_change_since = state.change_since.take();
            } else {
                let p = state.pending;
                if !(p.full || p.occupancy || p.bounce_only) {
                    return (false, false);
                }
                state.pending = Pending::default();
                let sky = p.occupancy;
                let bounce = can_bounce && (p.occupancy || p.bounce_only);
                if !sky && !bounce {
                    return (false, false);
                }
                state.running = Some(Running {
                    next_slice: 0,
                    full: p.full,
                    sky,
                    bounce,
                    recentre: false,
                });
                state.frames_in_sweep = 0;
                state.sweep_change_since = state.change_since.take();
            }
        }

        let Some(run) = state.running else {
            return (false, false);
        };
        let front = self.front.load(Ordering::Acquire);
        let target = if run.recentre { 1 - front } else { front };
        let (z0, z1, finished) = if run.full {
            (0, LIGHT_VOLUME_DIM, true)
        } else {
            let z0 = run.next_slice * SLICE_DEPTH;
            (
                z0,
                (z0 + SLICE_DEPTH).min(LIGHT_VOLUME_DIM),
                z0 + SLICE_DEPTH >= LIGHT_VOLUME_DIM,
            )
        };
        state.frames_in_sweep += 1;
        let region = [z0, z1, 0, 0];
        // The shader skips cells outside `region`, so the dispatch always
        // covers the cache; the skipped threads exit immediately.
        let groups = LIGHT_VOLUME_DIM / 4;
        let (mut sky_ran, mut bounce_ran) = (false, false);
        if run.sky {
            queue.write_buffer(
                &self.globals,
                SKY_REGION_OFFSET,
                bytemuck::cast_slice(&region),
            );
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("spall-sky-visibility-pass"),
                timestamp_writes: sky_timestamps,
            });
            pass.set_pipeline(&pipeline.compute);
            pass.set_bind_group(0, &self.sets[target].compute_bind, &[]);
            pass.dispatch_workgroups(groups, groups, groups);
            sky_ran = true;
        }
        if run.bounce && can_bounce {
            // Gather emissive cells into the emitter list this slice reads. It
            // is rebuilt for every slice (a few thousandths of a millisecond of
            // bin arithmetic) so it always matches the occupancy and material
            // table the slice sees, including edits and moving bodies.
            encoder.clear_buffer(&self.emitters.bins, 0, None);
            // The bounce timing spans both passes: the emitter gather takes the
            // begin stamp and the bounce pass the end stamp, so the reported
            // bounce cost includes the gather.
            let (emit_timestamps, bounce_timestamps) = match bounce_timestamps {
                Some(ts) => (
                    Some(wgpu::ComputePassTimestampWrites {
                        query_set: ts.query_set,
                        beginning_of_pass_write_index: ts.beginning_of_pass_write_index,
                        end_of_pass_write_index: None,
                    }),
                    Some(wgpu::ComputePassTimestampWrites {
                        query_set: ts.query_set,
                        beginning_of_pass_write_index: None,
                        end_of_pass_write_index: ts.end_of_pass_write_index,
                    }),
                ),
                None => (None, None),
            };
            {
                let emit_bind = self.emit_bind.lock().unwrap_or_else(|e| e.into_inner());
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("spall-emitter-pass"),
                    timestamp_writes: emit_timestamps,
                });
                pass.set_bind_group(0, &*emit_bind, &[]);
                pass.set_pipeline(&pipeline.emit_gather);
                pass.dispatch_workgroups(groups, groups, groups);
                pass.set_pipeline(&pipeline.emit_compact);
                let bin_count = (LIGHT_VOLUME_DIM / EMITTER_BIN_CELLS).pow(3);
                pass.dispatch_workgroups(bin_count.div_ceil(64), 1, 1);
                pass.set_pipeline(&pipeline.emit_finish);
                pass.dispatch_workgroups(1, 1, 1);
            }
            encoder.copy_buffer_to_buffer(
                &self.emitters.records,
                0,
                &self.emitters.uniform,
                0,
                EMITTER_BLOCK_BYTES,
            );
            queue.write_buffer(
                &self.bounce_globals,
                BOUNCE_REGION_OFFSET,
                bytemuck::cast_slice(&region),
            );
            let binds = self.bounce_binds.lock().unwrap_or_else(|e| e.into_inner());
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("spall-bounce-pass"),
                timestamp_writes: bounce_timestamps,
            });
            pass.set_pipeline(&pipeline.bounce);
            pass.set_bind_group(0, &binds[target], &[]);
            pass.dispatch_workgroups(groups, groups, groups);
            bounce_ran = true;
        }
        if finished {
            state.running = None;
            state.sweeps_completed += 1;
            state.last_sweep_frames = state.frames_in_sweep;
            state.last_latency_ms = state
                .sweep_change_since
                .take()
                .map(|since| since.elapsed().as_secs_f64() * 1000.0);
            if run.recentre {
                // Swap: shading now reads the freshly built set, sampled with
                // the new origin.
                self.front.store(target, Ordering::Release);
                state.display_origin = state.origin;
                state.recentres_completed += 1;
                self.write_display_globals(queue, &state);
                // A grid that arrived during the sweep is next: an origin move
                // is another re-centre; the same origin is an in-place edit.
                if let Some(next) = state.deferred.take() {
                    if next.origin() != state.origin {
                        state.deferred = Some(next);
                        state.pending.recentre = true;
                    } else {
                        self.apply_grid(queue, &mut state, &next);
                        state.pending.occupancy = true;
                    }
                    state.note_change();
                }
                if std::mem::take(&mut state.bodies_moved) {
                    // Bodies moved during the sweep were only recorded, not
                    // uploaded; lay them over the new cache and light them.
                    let current = std::mem::take(&mut state.last_cubes);
                    let new = body_cell_map(state.origin, &current);
                    let changed = diff_body_cells(&state.base, &state.bodies, &new);
                    state.bodies = new;
                    state.last_cubes = current;
                    self.write_cells(queue, &changed);
                    state.pending.occupancy = true;
                    state.note_change();
                }
            }
        } else if let Some(running) = state.running.as_mut() {
            running.next_slice += 1;
        }
        (sky_ran, bounce_ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_origin_is_grid_aligned_and_contains_the_centre() {
        let centre = Vec3::new(13.37, 2.9, -41.2);
        let origin = cache_origin_around(centre);
        for axis in 0..3 {
            let cells = origin[axis] / LIGHT_CELL_SIZE_METRES;
            assert!(
                (cells - cells.round()).abs() < 1e-4,
                "axis {axis}: {origin:?}"
            );
        }
        let volume = LightingVolume::empty(origin);
        let cell = volume.world_to_cell(centre);
        let d = LIGHT_VOLUME_DIM as i32;
        assert!(cell.cmpge(glam::IVec3::ZERO).all() && cell.cmplt(glam::IVec3::splat(d)).all());
    }

    #[test]
    fn a_wall_thinner_than_a_cell_still_occupies_it() {
        let mut volume = LightingVolume::empty(Vec3::ZERO);
        // One 0.25 m voxel: half a cache cell.
        mark_world_box(
            &mut volume,
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(1.25, 1.25, 1.25),
            7,
        );
        let cell = volume.world_to_cell(Vec3::new(1.1, 1.1, 1.1));
        let index = (cell.x + 128 * (cell.y + 128 * cell.z)) as usize;
        assert_eq!(volume.cells()[index], 7);
        assert_eq!(volume.cells().iter().filter(|c| **c != 0).count(), 1);
    }

    fn cube(center: [f32; 3], size: f32, material: u32) -> CubeInstance {
        CubeInstance::new(center, material, [size; 3], CubeInstance::IDENTITY_ROTATION)
    }

    #[test]
    fn a_body_covers_the_cells_its_box_touches_and_leaving_restores_the_terrain() {
        let origin = Vec3::ZERO;
        let mut base = vec![0u32; (LIGHT_VOLUME_DIM as usize).pow(3)];
        let d = LIGHT_VOLUME_DIM as usize;
        let idx = |x: usize, y: usize, z: usize| (x + d * (y + d * z)) as u32;
        // Terrain under the body's path.
        base[idx(2, 2, 2) as usize] = 9;

        let empty = HashMap::new();
        // A 1 m cube centred at (1.25, 1.25, 1.25) covers cells [0.75, 1.75): 0.5 m cells 1..=3
        // in each axis (any touch counts).
        let at = body_cell_map(origin, &[cube([1.25; 3], 1.0, 4)]);
        assert_eq!(at.len(), 27);
        // Entering: every covered cell changes, terrain cell overridden by the body.
        let entering = diff_body_cells(&base, &empty, &at);
        assert_eq!(entering.len(), 27);
        assert!(entering.contains(&(idx(2, 2, 2), 4)));
        // Leaving: the vacated terrain cell gets its terrain value back.
        let leaving = diff_body_cells(&base, &at, &empty);
        assert_eq!(leaving.len(), 27);
        assert!(
            leaving.contains(&(idx(2, 2, 2), 9)),
            "terrain must be restored, not erased"
        );
        assert!(leaving.contains(&(idx(1, 1, 1), 0)));
    }

    #[test]
    fn a_cell_shared_by_two_bodies_stays_occupied_when_one_leaves() {
        let origin = Vec3::ZERO;
        let base = vec![0u32; (LIGHT_VOLUME_DIM as usize).pow(3)];
        let a = cube([1.25; 3], 1.0, 4);
        let b = cube([1.75, 1.25, 1.25], 1.0, 5);
        let both = body_cell_map(origin, &[a, b]);
        let only_b = body_cell_map(origin, &[b]);
        let changed = diff_body_cells(&base, &both, &only_b);
        // Cells only `a` covered go back to air; cells `b` also covers do not change.
        let d = LIGHT_VOLUME_DIM as usize;
        let shared = (2 + d * (2 + d * 2)) as u32;
        assert!(
            !changed.iter().any(|(i, _)| *i == shared),
            "shared cell must not be touched"
        );
        assert!(
            changed.iter().all(|(_, v)| *v == 0),
            "only vacated cells change"
        );
        assert!(!changed.is_empty());
        // Nothing moved: nothing to upload.
        assert!(diff_body_cells(&base, &both, &both).is_empty());
    }

    #[test]
    fn a_rotated_body_covers_at_least_its_axis_aligned_cells() {
        let origin = Vec3::ZERO;
        let flat = cube([4.0; 3], 1.0, 1);
        let turned = CubeInstance::new(
            [4.0; 3],
            1,
            [1.0; 3],
            glam::Quat::from_rotation_y(std::f32::consts::FRAC_PI_4).to_array(),
        );
        let (a, b) = (
            body_cell_map(origin, &[flat]),
            body_cell_map(origin, &[turned]),
        );
        assert!(
            b.len() >= a.len(),
            "the rotated bounding box must not shrink: {} vs {}",
            b.len(),
            a.len()
        );
        // Bodies outside the cache contribute nothing.
        assert!(body_cell_map(origin, &[cube([900.0; 3], 1.0, 1)]).is_empty());
    }

    #[test]
    fn boxes_outside_the_cache_are_ignored() {
        let mut volume = LightingVolume::empty(Vec3::ZERO);
        mark_world_box(&mut volume, Vec3::splat(500.0), Vec3::splat(501.0), 3);
        mark_world_box(&mut volume, Vec3::splat(-9.0), Vec3::splat(-8.0), 3);
        assert!(volume.cells().iter().all(|c| *c == 0));
    }
}
