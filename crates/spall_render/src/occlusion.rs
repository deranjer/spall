//! Occlusion culling of terrain chunks with GPU occlusion queries.
//!
//! About four in five of the chunks inside the camera's frustum have no visible
//! pixel (underground shells, the far side of hills, the inside of cliffs), and
//! drawing them costs the CPU a few thousand commands a frame. This module
//! decides which to skip, exactly: a chunk's bounding box is drawn against the
//! depth the visible chunks left, inside a GPU occlusion query, and a chunk
//! whose box has no sample passing is hidden.
//!
//! Query results arrive a few frames late, so the answer for a chunk is a
//! *verdict* that is kept and reused:
//!
//! - A chunk with no verdict (new, edited, or back in the frustum after being
//!   outside it) is drawn, and tested first.
//! - A chunk judged visible is drawn, and re-tested now and then so that one
//!   that becomes hidden stops being drawn.
//! - A chunk judged hidden is skipped, re-tested often (so one that becomes
//!   visible is drawn within a few frames), and drawn anyway once its verdict is
//!   old, as a safety net.
//! - A camera that jumps drops every verdict.
//!
//! Only the camera's pass uses this; the sun cascades keep every chunk (their
//! casters can be hidden from the camera and still shade it).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytemuck::{Pod, Zeroable};
use glam::Vec3;
use spall_core::BrickCoord;
use wgpu::util::DeviceExt;

use crate::camera::Aabb;
use crate::pipeline::DEPTH_FORMAT;

/// Chunks tested per frame.
pub const QUERY_BUDGET: usize = 192;
/// Query batches in flight at once; a result is read three frames or so after it
/// was issued.
const SLOTS: usize = 3;
/// A "hidden" verdict older than this many frames is not trusted: the chunk is
/// drawn and tested again.
const HIDDEN_MAX_AGE: u64 = 90;
/// A "visible" chunk is re-tested after this many frames.
const VISIBLE_RETEST: u64 = 16;
/// A camera that moves farther than this in one frame drops every verdict
/// (a teleport, a respawn).
const JUMP_METRES: f32 = 3.0;

const SLOT_IDLE: u8 = 0;
const SLOT_ENCODED: u8 = 1;
const SLOT_MAPPING: u8 = 2;
const SLOT_READY: u8 = 3;

#[derive(Debug, Clone, Copy)]
struct Verdict {
    visible: bool,
    /// The frame whose query gave it.
    tested: u64,
    /// The last frame the chunk was inside the frustum.
    seen: u64,
}

struct Slot {
    readback: wgpu::Buffer,
    state: Arc<AtomicU8>,
    /// What each query of the batch asked about: the chunk, its edit epoch
    /// then, and the frame.
    issued: Vec<(BrickCoord, u64, u64)>,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BoxInstance {
    min: [f32; 3],
    _pad0: f32,
    max: [f32; 3],
    _pad1: f32,
}

pub struct OcclusionCuller {
    pipeline: wgpu::RenderPipeline,
    bind: wgpu::BindGroup,
    globals: wgpu::Buffer,
    corners: wgpu::Buffer,
    indices: wgpu::Buffer,
    instances: wgpu::Buffer,
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    slots: Vec<Slot>,
    verdicts: HashMap<BrickCoord, Verdict>,
    /// Bumped when a chunk's mesh changes, so a result for the old mesh is dropped.
    epochs: HashMap<BrickCoord, u64>,
    frame: u64,
    last_eye: Option<Vec3>,
    /// Chunks drawn and tested last frame.
    pub drawn: usize,
    pub tested: usize,
}

impl OcclusionCuller {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spall-occlusion"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/occlusion.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spall-occlusion-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spall-occlusion-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let corner_layout = wgpu::VertexBufferLayout {
            array_stride: 12,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3],
        };
        let box_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<BoxInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            // `max` follows `min`'s padding word, at offset 16.
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 16,
                    shader_location: 2,
                },
            ],
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("spall-occlusion-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(corner_layout), Some(box_layout)],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // Both faces: a camera inside a box must still count it.
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-occlusion-globals"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spall-occlusion-bind"),
            layout: &layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let corner_data: Vec<[f32; 3]> = (0..8u32)
            .map(|i| [(i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32])
            .collect();
        let corners = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-occlusion-corners"),
            contents: bytemuck::cast_slice(&corner_data),
            usage: wgpu::BufferUsages::VERTEX,
        });
        // The 12 triangles of a cube over those corners.
        const CUBE: [u16; 36] = [
            0, 1, 3, 0, 3, 2, // -z
            4, 6, 7, 4, 7, 5, // +z
            0, 2, 6, 0, 6, 4, // -x
            1, 5, 7, 1, 7, 3, // +x
            0, 4, 5, 0, 5, 1, // -y
            2, 3, 7, 2, 7, 6, // +y
        ];
        let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("spall-occlusion-indices"),
            contents: bytemuck::cast_slice(&CUBE),
            usage: wgpu::BufferUsages::INDEX,
        });
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-occlusion-instances"),
            size: (QUERY_BUDGET * std::mem::size_of::<BoxInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("spall-occlusion-queries"),
            ty: wgpu::QueryType::Occlusion,
            count: QUERY_BUDGET as u32,
        });
        let result_bytes = (QUERY_BUDGET * 8) as u64;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-occlusion-resolve"),
            size: result_bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let slots = (0..SLOTS)
            .map(|_| Slot {
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("spall-occlusion-readback"),
                    size: result_bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(SLOT_IDLE)),
                issued: Vec::new(),
            })
            .collect();
        Self {
            pipeline,
            bind,
            globals,
            corners,
            indices,
            instances,
            queries,
            resolve,
            slots,
            verdicts: HashMap::new(),
            epochs: HashMap::new(),
            frame: 0,
            last_eye: None,
            drawn: 0,
            tested: 0,
        }
    }

    /// A chunk's mesh changed or went away: forget what is known about it.
    pub fn forget(&mut self, coord: BrickCoord) {
        self.verdicts.remove(&coord);
        *self.epochs.entry(coord).or_default() += 1;
    }

    /// Starts a frame: reads the query results that have arrived and drops the
    /// verdicts if the camera jumped.
    pub fn begin_frame(&mut self, device: &wgpu::Device, eye: Vec3) {
        self.frame += 1;
        if self
            .last_eye
            .is_some_and(|last| (last - eye).length() > JUMP_METRES)
        {
            self.verdicts.clear();
        }
        self.last_eye = Some(eye);
        let _ = device.poll(wgpu::PollType::Poll);
        for slot in &mut self.slots {
            if slot.state.load(Ordering::Acquire) != SLOT_READY {
                continue;
            }
            {
                let bytes = (slot.issued.len() * 8) as u64;
                let view = slot.readback.slice(0..bytes).get_mapped_range();
                if let Ok(view) = view {
                    for (i, (coord, epoch, frame)) in slot.issued.iter().enumerate() {
                        if self.epochs.get(coord).copied().unwrap_or(0) != *epoch {
                            continue;
                        }
                        let samples = u64::from_le_bytes(
                            view[i * 8..i * 8 + 8].try_into().expect("eight bytes"),
                        );
                        // The chunk is in the frustum now (or was a moment ago):
                        // a verdict for it starts fresh, not as stale as the
                        // frame whose query gave it.
                        self.verdicts.insert(
                            *coord,
                            Verdict {
                                visible: samples > 0,
                                tested: *frame,
                                seen: self.frame,
                            },
                        );
                    }
                }
            }
            slot.readback.unmap();
            slot.issued.clear();
            slot.state.store(SLOT_IDLE, Ordering::Release);
        }
    }

    /// Whether `coord` is drawn this frame. Call once per frame for each chunk
    /// inside the camera's frustum.
    pub fn should_draw(&mut self, coord: BrickCoord) -> bool {
        let frame = self.frame;
        match self.verdicts.get_mut(&coord) {
            None => true,
            Some(v) if v.seen + 1 < frame => {
                // It was outside the frustum: what was known may be stale.
                self.verdicts.remove(&coord);
                true
            }
            Some(v) => {
                v.seen = frame;
                v.visible || frame.saturating_sub(v.tested) > HIDDEN_MAX_AGE
            }
        }
    }

    /// Chunks to test this frame, most useful first: those with no verdict, then
    /// hidden ones (oldest first), then visible ones that have not been tested
    /// for a while.
    fn select(&mut self, candidates: &[(BrickCoord, Aabb)], eye: Vec3) -> Vec<(BrickCoord, Aabb)> {
        let frame = self.frame;
        let mut ranked: Vec<(u64, BrickCoord, Aabb)> = Vec::new();
        for (coord, bounds) in candidates {
            // A camera inside the box sees the chunk.
            let inside =
                (0..3).all(|a| eye[a] >= bounds.min[a] - 0.5 && eye[a] <= bounds.max[a] + 0.5);
            if inside {
                self.verdicts.insert(
                    *coord,
                    Verdict {
                        visible: true,
                        tested: frame,
                        seen: frame,
                    },
                );
                continue;
            }
            // Older verdicts first within a class: a bigger age is a smaller rank.
            let older = |tested: u64| 999_999 - frame.saturating_sub(tested).min(999_999);
            let rank = match self.verdicts.get(coord) {
                None => 0,
                Some(v) if !v.visible => 1_000_000 + older(v.tested),
                Some(v) if frame.saturating_sub(v.tested) >= VISIBLE_RETEST => {
                    2_000_000 + older(v.tested)
                }
                Some(_) => continue,
            };
            ranked.push((rank, *coord, *bounds));
        }
        ranked.sort_by_key(|r| r.0);
        ranked.truncate(QUERY_BUDGET);
        ranked.into_iter().map(|(_, c, b)| (c, b)).collect()
    }

    /// Records the occlusion pass: the boxes of the chosen chunks against `depth`
    /// (the opaque pass's depth, read only). Call after the opaque pass, before
    /// submitting `encoder`, then [`Self::after_submit`].
    pub fn encode(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        view_proj: glam::Mat4,
        candidates: &[(BrickCoord, Aabb)],
        eye: Vec3,
    ) {
        let Some(slot_index) = self
            .slots
            .iter()
            .position(|s| s.state.load(Ordering::Acquire) == SLOT_IDLE)
        else {
            return;
        };
        let chosen = self.select(candidates, eye);
        self.tested = chosen.len();
        if chosen.is_empty() {
            return;
        }
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&view_proj.to_cols_array_2d()),
        );
        let boxes: Vec<BoxInstance> = chosen
            .iter()
            .map(|(_, b)| BoxInstance {
                min: b.min.to_array(),
                _pad0: 0.0,
                max: b.max.to_array(),
                _pad1: 0.0,
            })
            .collect();
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&boxes));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-occlusion-pass"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
                    depth_ops: None,
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: Some(&self.queries),
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_vertex_buffer(0, self.corners.slice(..));
            pass.set_vertex_buffer(1, self.instances.slice(..));
            pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint16);
            for i in 0..chosen.len() as u32 {
                pass.begin_occlusion_query(i);
                pass.draw_indexed(0..36, 0, i..i + 1);
                pass.end_occlusion_query();
            }
        }
        let bytes = (chosen.len() * 8) as u64;
        encoder.resolve_query_set(&self.queries, 0..chosen.len() as u32, &self.resolve, 0);
        let slot = &mut self.slots[slot_index];
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &slot.readback, 0, bytes);
        slot.issued = chosen
            .iter()
            .map(|(c, _)| (*c, self.epochs.get(c).copied().unwrap_or(0), self.frame))
            .collect();
        slot.state.store(SLOT_ENCODED, Ordering::Release);
    }

    /// Starts reading back the batch [`Self::encode`] recorded. Call after the
    /// encoder was submitted.
    pub fn after_submit(&mut self) {
        for slot in &self.slots {
            if slot.state.load(Ordering::Acquire) != SLOT_ENCODED {
                continue;
            }
            slot.state.store(SLOT_MAPPING, Ordering::Release);
            let state = slot.state.clone();
            let bytes = (slot.issued.len() * 8) as u64;
            slot.readback
                .slice(0..bytes)
                .map_async(wgpu::MapMode::Read, move |result| {
                    state.store(
                        if result.is_ok() {
                            SLOT_READY
                        } else {
                            SLOT_IDLE
                        },
                        Ordering::Release,
                    );
                });
        }
    }
}
