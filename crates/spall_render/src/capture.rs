//! Bounded offscreen capture for the explicit T12 shadow/HDR/tone-map passes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use image::{ImageBuffer, Rgba};

use crate::camera::Camera;
use crate::context::{RenderContext, RenderError};
use crate::indirect::{LIGHT_VOLUME_DIM, LightingUpdate};
use crate::pipeline::{CASCADE_COUNT, DebugView, PassTiming, ScenePipeline, default_sun_dir};
use crate::scene::Scene;
use crate::target::OffscreenTarget;
use crate::upload::{GpuMesh, UploadBudget};
use crate::vertex::to_gpu;

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub width: u32,
    pub height: u32,
    pub views: Vec<DebugView>,
    pub budget: UploadBudget,
    /// Fixed linear exposure used by every acceptance capture.
    pub exposure: f32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            views: vec![
                DebugView::Shaded,
                DebugView::Albedo,
                DebugView::Normals,
                DebugView::Depth,
                DebugView::ShadowCascades,
                DebugView::Roughness,
            ],
            budget: UploadBudget::default(),
            exposure: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureImage {
    pub view: DebugView,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureTiming {
    /// CPU wall time to allocate and upload the lighting cache.
    pub cpu_lighting_upload_millis: f64,
    pub cpu_total_millis: f64,
    pub cpu_readback_millis: f64,
    pub cpu_encode_millis: f64,
    /// Total measured device time for all shadow, opaque, and tone-map passes.
    pub gpu_render_millis: Option<f64>,
    /// Device timing split by explicit pass family.
    pub gpu_passes: Option<PassTiming>,
}

#[derive(Debug, Clone)]
pub struct CaptureReport {
    pub images: Vec<CaptureImage>,
    pub width: u32,
    pub height: u32,
    pub items_total: usize,
    pub items_drawn: usize,
    pub triangles: u64,
    pub vertex_bytes: u64,
    pub index_bytes: u64,
    pub adapter: String,
    pub backend: String,
    pub indirect_enabled: bool,
    pub indirect_cells: usize,
    pub timing: CaptureTiming,
}

struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    period_ns: f64,
    pairs: u32,
}

impl GpuTimer {
    fn new(ctx: &RenderContext, pairs: u32) -> Option<Self> {
        if !ctx.supports_gpu_timestamps() || pairs == 0 {
            return None;
        }
        let count = pairs.checked_mul(2)?;
        let bytes = u64::from(count) * std::mem::size_of::<u64>() as u64;
        let set = ctx.device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("spall-capture-timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count,
        });
        let resolve = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-capture-timestamp-resolve"),
            size: bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-capture-timestamp-readback"),
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Some(Self {
            set,
            resolve,
            readback,
            period_ns: f64::from(ctx.timestamp_period_ns()),
            pairs,
        })
    }

    fn writes(&self, index: u32) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(index * 2),
            end_of_pass_write_index: Some(index * 2 + 1),
        }
    }

    fn compute_writes(&self, index: u32) -> wgpu::ComputePassTimestampWrites<'_> {
        wgpu::ComputePassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(index * 2),
            end_of_pass_write_index: Some(index * 2 + 1),
        }
    }

    fn millis(self, ctx: &RenderContext) -> Option<Vec<f64>> {
        let count = self.pairs * 2;
        let bytes = u64::from(count) * std::mem::size_of::<u64>() as u64;
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-capture-timestamp-resolve-encoder"),
            });
        encoder.resolve_query_set(&self.set, 0..count, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &self.readback, 0, bytes);
        ctx.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.wait();
        rx.recv().ok()?.ok()?;
        let ticks: Vec<u64> = {
            let mapped = slice.get_mapped_range();
            bytemuck::cast_slice::<u8, u64>(&mapped).to_vec()
        };
        self.readback.unmap();
        Some(
            ticks
                .chunks_exact(2)
                .map(|pair| pair[1].saturating_sub(pair[0]) as f64 * self.period_ns / 1.0e6)
                .collect(),
        )
    }
}

pub fn capture_scene(
    ctx: &RenderContext,
    scene: &Scene,
    out_dir: &Path,
    opts: &CaptureOptions,
) -> Result<CaptureReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;

    let pipeline = ScenePipeline::new(&ctx.device);
    let materials = pipeline.material_buffer(&ctx.device, &scene.materials);
    let indirect =
        pipeline.indirect_resources(&ctx.device, &ctx.queue, scene.lighting.as_ref(), &materials);
    let target = OffscreenTarget::new(&ctx.device, opts.width, opts.height);
    let frustum = scene.camera.frustum();
    let mut draws = Vec::new();
    let mut triangles = 0u64;
    let (mut vertex_bytes, mut index_bytes) = (0u64, 0u64);
    for item in &scene.items {
        if !frustum.intersects_aabb(item.world_bounds()) {
            continue;
        }
        let (vertices, indices) = to_gpu(&item.mesh, item.model);
        if indices.is_empty() {
            continue;
        }
        let gpu = GpuMesh::create(&ctx.device, &vertices, &indices, opts.budget)?;
        triangles += u64::from(gpu.index_count) / 3;
        vertex_bytes += gpu.vertex_bytes;
        index_bytes += gpu.index_bytes;
        draws.push(gpu);
    }

    let pair_count = 2 + CASCADE_COUNT as u32 + opts.views.len() as u32 * 2;
    let mut gpu_timer = GpuTimer::new(ctx, pair_count);
    let loop_start = Instant::now();

    // Lighting and shadows are view-independent, so compute/render them once.
    let (light_matrices, _) = ScenePipeline::cascade_data(&scene.camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-shadow-encoder"),
        });
    let trace_timestamps = gpu_timer.as_ref().map(|timer| timer.compute_writes(0));
    let denoise_timestamps = gpu_timer.as_ref().map(|timer| timer.compute_writes(1));
    pipeline.dispatch_indirect(
        &mut shadow_encoder,
        &indirect,
        trace_timestamps,
        denoise_timestamps,
    );
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let timestamp_writes = gpu_timer
            .as_ref()
            .map(|timer| timer.writes(2 + cascade as u32));
        let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-shadow-pass"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: pipeline.shadow_layer(cascade),
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline.shadow());
        pass.set_bind_group(0, &bind, &[]);
        draw_meshes(&mut pass, &draws);
    }
    ctx.queue.submit([shadow_encoder.finish()]);

    let mut readback_millis = 0.0;
    let mut encode_millis = 0.0;
    let mut images = Vec::new();
    for (view_index, &view) in opts.views.iter().enumerate() {
        let scene_bind = pipeline.scene_bind_group(
            &ctx.device,
            &ctx.queue,
            &scene.camera,
            view,
            opts.exposure,
            &materials,
        );
        let tone_bind = pipeline.tone_bind_group(
            &ctx.device,
            &ctx.queue,
            target.hdr_view(),
            opts.exposure,
            view != DebugView::Shaded,
        );
        let base_query = 2 + CASCADE_COUNT as u32 + view_index as u32 * 2;
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-t12-capture-encoder"),
            });
        {
            let timestamp_writes = gpu_timer.as_ref().map(|timer| timer.writes(base_query));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-hdr-opaque-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: scene.clear[0],
                            g: scene.clear[1],
                            b: scene.clear[2],
                            a: scene.clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: target.depth_view(),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline.opaque());
            pass.set_bind_group(0, &scene_bind, &[]);
            pass.set_bind_group(1, &indirect.display_bind, &[]);
            draw_meshes(&mut pass, &draws);
        }
        {
            let timestamp_writes = gpu_timer.as_ref().map(|timer| timer.writes(base_query + 1));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-tone-map-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.color_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline.tone_map());
            pass.set_bind_group(0, &tone_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);

        let readback_start = Instant::now();
        let rgba = target.read_rgba(ctx)?;
        let image: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_raw(target.width, target.height, rgba)
                .expect("readback has width*height*4 bytes");
        readback_millis += readback_start.elapsed().as_secs_f64() * 1000.0;

        let encode_start = Instant::now();
        let path = out_dir.join(format!("{}.png", view.stem()));
        image.save(&path).map_err(|source| RenderError::Image {
            path: path.display().to_string(),
            source,
        })?;
        encode_millis += encode_start.elapsed().as_secs_f64() * 1000.0;
        images.push(CaptureImage { view, path });
    }
    ctx.wait();
    let cpu_total_millis = loop_start.elapsed().as_secs_f64() * 1000.0;

    let gpu_passes = gpu_timer
        .take()
        .and_then(|timer| timer.millis(ctx))
        .map(|times| {
            let shadow_start = 2;
            let raster_start = shadow_start + CASCADE_COUNT;
            let shadow_millis = times[shadow_start..raster_start].iter().sum();
            let opaque_millis = times[raster_start..].iter().step_by(2).sum();
            let tone_map_millis = times[raster_start + 1..].iter().step_by(2).sum();
            PassTiming {
                indirect_trace_millis: times[0],
                indirect_denoise_millis: times[1],
                shadow_millis,
                opaque_millis,
                tone_map_millis,
            }
        });
    let gpu_render_millis = gpu_passes.map(PassTiming::total);

    Ok(CaptureReport {
        images,
        width: target.width,
        height: target.height,
        items_total: scene.items.len(),
        items_drawn: draws.len(),
        triangles,
        vertex_bytes,
        index_bytes,
        adapter: ctx.adapter_name().to_string(),
        backend: format!("{:?}", ctx.backend()),
        indirect_enabled: indirect.enabled,
        indirect_cells: indirect.cells,
        timing: CaptureTiming {
            cpu_lighting_upload_millis: indirect.upload_millis,
            cpu_total_millis,
            cpu_readback_millis: readback_millis,
            cpu_encode_millis: encode_millis,
            gpu_render_millis,
            gpu_passes,
        },
    })
}

fn draw_meshes<'pass>(pass: &mut wgpu::RenderPass<'pass>, draws: &'pass [GpuMesh]) {
    for gpu in draws {
        pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
        pass.set_index_buffer(gpu.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..gpu.index_count, 0, 0..1);
    }
}

/// One step of a [`capture_lighting_sequence`] run: an incremental world change
/// and, optionally, a new camera pose to render it from (for moving-camera
/// fixtures). `update` may be empty when only the camera moves.
#[derive(Debug, Clone)]
pub struct LightingStep {
    pub label: String,
    pub update: LightingUpdate,
    pub camera: Option<Camera>,
}

impl LightingStep {
    /// A step that only applies a world change, rendered from the base camera.
    pub fn edit(label: impl Into<String>, update: LightingUpdate) -> Self {
        Self {
            label: label.into(),
            update,
            camera: None,
        }
    }

    /// A step that only moves the camera over an unchanged world.
    pub fn view(label: impl Into<String>, camera: Camera) -> Self {
        Self {
            label: label.into(),
            update: LightingUpdate::new(),
            camera: Some(camera),
        }
    }
}

/// The lighting state rendered at one step of a [`capture_lighting_sequence`]
/// run. Step 0 is the base scene; every later step is the result of applying
/// one [`LightingStep`] on top of the previous state.
#[derive(Debug, Clone)]
pub struct SequenceStep {
    pub label: String,
    /// Cache cells whose material changed at this step (`0` for the base).
    pub dirty_cells: usize,
    /// Cache cells the trace recomputed this step: the dirty AABB grown by the
    /// halo, or the whole cache for the base step.
    pub retraced_cells: u64,
    pub gpu_trace_millis: Option<f64>,
    pub gpu_denoise_millis: Option<f64>,
    pub gpu_temporal_millis: Option<f64>,
    /// Mean sRGB luminance of the measured image band in the `IndirectOnly`
    /// render for this step.
    pub band_luminance: f32,
    pub indirect_image: PathBuf,
}

/// Result of rendering a base scene and then a sequence of incremental lighting
/// updates, re-uploading only the dirty cells and re-tracing only the dirty
/// region (plus a halo) at each step.
#[derive(Debug, Clone)]
pub struct SequenceReport {
    pub adapter: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub total_cells: u64,
    /// Fractional image-x band `[x0, x1)` the `band_luminance` figures measure.
    pub band: [f32; 2],
    pub halo_cells: u32,
    /// Weight of the current frame mixed into history outside the re-traced
    /// region: `1.0` disables temporal accumulation.
    pub temporal_weight: f32,
    pub steps: Vec<SequenceStep>,
}

/// History clamp slack for [`capture_lighting_sequence`], as a fraction of the
/// local neighbourhood spread.
const TEMPORAL_SLACK: f32 = 0.25;

/// Settings for a [`capture_lighting_sequence`] run.
#[derive(Debug, Clone, Copy)]
pub struct SequenceOptions {
    /// Fractional image-x band `[x0, x1)` whose mean luminance is measured.
    pub band: [f32; 2],
    /// Cells added on each side of a step's dirty AABB before re-tracing.
    pub halo_cells: u32,
    pub exposure: f32,
    /// Weight of the current frame mixed into the temporal history outside the
    /// re-traced region. `1.0` disables accumulation (each step is one settled
    /// frame); a small value (e.g. `0.1`) accumulates.
    pub temporal_weight: f32,
    /// Render size. Both values are clamped up to the nearest even number.
    pub width: u32,
    pub height: u32,
}

impl Default for SequenceOptions {
    fn default() -> Self {
        Self {
            band: [0.3, 0.7],
            halo_cells: 12,
            exposure: 1.0,
            temporal_weight: 1.0,
            width: 1280,
            height: 720,
        }
    }
}

/// Render `scene` under `IndirectOnly`, then apply each [`LightingStep`] to the
/// scene's lighting volume in turn — re-uploading only the changed cells and
/// re-tracing only the changed region grown by `halo_cells` — and render again.
///
/// This is the T14 partial-update path exercised end to end: it proves an edit
/// reaches the rendered indirect lighting in the next frame and reports how
/// much of the cache each step actually touched.
///
/// `temporal_weight` controls the temporal accumulation pass: `1.0` disables it
/// (each step is a single settled frame); a small value (e.g. `0.1`) blends
/// each frame into a persistent, neighbourhood-clamped history so the 12-ray
/// trace noise settles, while the re-traced region and the clamp keep edits and
/// moving occluders from leaving stale shadows or light trails.
pub fn capture_lighting_sequence(
    ctx: &RenderContext,
    scene: Scene,
    opts: SequenceOptions,
    steps: &[LightingStep],
    out_dir: &Path,
) -> Result<SequenceReport, RenderError> {
    let SequenceOptions {
        band,
        halo_cells,
        exposure,
        temporal_weight,
        width,
        height,
    } = opts;
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;

    let mut volume = scene.lighting.clone().ok_or_else(|| {
        RenderError::Gpu("capture_lighting_sequence needs a scene lighting volume".into())
    })?;
    let total_cells = (LIGHT_VOLUME_DIM as u64).pow(3);

    let pipeline = ScenePipeline::new(&ctx.device);
    let materials = pipeline.material_buffer(&ctx.device, &scene.materials);
    let indirect = pipeline.indirect_resources(&ctx.device, &ctx.queue, Some(&volume), &materials);
    let (width, height) = (even(width), even(height));
    let target = OffscreenTarget::new(&ctx.device, width, height);

    let frustum = scene.camera.frustum();
    let mut draws = Vec::new();
    for item in &scene.items {
        if !frustum.intersects_aabb(item.world_bounds()) {
            continue;
        }
        let (vertices, indices) = to_gpu(&item.mesh, item.model);
        if indices.is_empty() {
            continue;
        }
        draws.push(GpuMesh::create(
            &ctx.device,
            &vertices,
            &indices,
            UploadBudget::default(),
        )?);
    }

    // Initialise the shadow cascades once. `IndirectOnly` ignores the sun, but
    // sampling an uncleared depth texture is undefined.
    let (light_matrices, _) = ScenePipeline::cascade_data(&scene.camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-t14-seq-shadow-encoder"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-t14-seq-shadow-pass"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: pipeline.shadow_layer(cascade),
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline.shadow());
        pass.set_bind_group(0, &bind, &[]);
        draw_meshes(&mut pass, &draws);
    }
    ctx.queue.submit([shadow_encoder.finish()]);

    type FrameResult = (f32, Option<f64>, Option<f64>, Option<f64>, PathBuf);
    let render_indirect = |step_index: usize,
                           label: &str,
                           camera: &Camera|
     -> Result<FrameResult, RenderError> {
        let timer = GpuTimer::new(ctx, 3);
        let mut lighting_encoder =
            ctx.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("spall-t14-seq-lighting-encoder"),
                });
        pipeline.dispatch_indirect(
            &mut lighting_encoder,
            &indirect,
            timer.as_ref().map(|timer| timer.compute_writes(0)),
            timer.as_ref().map(|timer| timer.compute_writes(1)),
        );
        pipeline.dispatch_indirect_temporal(
            &mut lighting_encoder,
            &indirect,
            timer.as_ref().map(|timer| timer.compute_writes(2)),
        );
        ctx.queue.submit([lighting_encoder.finish()]);

        let scene_bind = pipeline.scene_bind_group(
            &ctx.device,
            &ctx.queue,
            camera,
            DebugView::IndirectOnly,
            exposure,
            &materials,
        );
        let tone_bind =
            pipeline.tone_bind_group(&ctx.device, &ctx.queue, target.hdr_view(), exposure, true);
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-t14-seq-frame-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-t14-seq-opaque-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: scene.clear[0],
                            g: scene.clear[1],
                            b: scene.clear[2],
                            a: scene.clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: target.depth_view(),
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline.opaque());
            pass.set_bind_group(0, &scene_bind, &[]);
            pass.set_bind_group(1, &indirect.history_display_bind, &[]);
            draw_meshes(&mut pass, &draws);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-t14-seq-tone-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.color_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline.tone_map());
            pass.set_bind_group(0, &tone_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        ctx.wait();

        let rgba = target.read_rgba(ctx)?;
        let luminance = band_luminance(&rgba, target.width, target.height, band);
        let image: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_raw(target.width, target.height, rgba)
                .expect("readback has width*height*4 bytes");
        let path = out_dir.join(format!("{step_index:02}-{label}-indirect_only.png"));
        image.save(&path).map_err(|source| RenderError::Image {
            path: path.display().to_string(),
            source,
        })?;

        let times = timer.and_then(|timer| timer.millis(ctx));
        let (trace_ms, denoise_ms, temporal_ms) = match times.as_deref() {
            Some([trace, denoise, temporal, ..]) => (Some(*trace), Some(*denoise), Some(*temporal)),
            _ => (None, None, None),
        };
        Ok((luminance, trace_ms, denoise_ms, temporal_ms, path))
    };

    let mut report_steps = Vec::with_capacity(steps.len() + 1);
    // Base step: full trace, full history seed (the shader forces weight 1.0
    // inside the re-traced region, which is the whole cache here).
    indirect.set_temporal(&ctx.queue, 1.0, TEMPORAL_SLACK);
    let (base_luma, base_trace, base_denoise, base_temporal, base_path) =
        render_indirect(0, "base", &scene.camera)?;
    report_steps.push(SequenceStep {
        label: "base".to_string(),
        dirty_cells: 0,
        retraced_cells: total_cells,
        gpu_trace_millis: base_trace,
        gpu_denoise_millis: base_denoise,
        gpu_temporal_millis: base_temporal,
        band_luminance: base_luma,
        indirect_image: base_path,
    });

    for (i, step) in steps.iter().enumerate() {
        volume.apply_update(&step.update);
        let dirty = volume.take_dirty();
        indirect.upload_dirty(&ctx.queue, &dirty);

        let (lo, hi) = trace_region(&dirty, halo_cells);
        indirect.set_trace_region(&ctx.queue, lo, hi);
        indirect.set_temporal(&ctx.queue, temporal_weight, TEMPORAL_SLACK);
        let retraced_cells = u64::from((hi.x - lo.x) * (hi.y - lo.y) * (hi.z - lo.z));

        let slug = slugify(&step.label);
        let camera = step.camera.unwrap_or(scene.camera);
        let (luma, trace_ms, denoise_ms, temporal_ms, path) =
            render_indirect(i + 1, &slug, &camera)?;
        report_steps.push(SequenceStep {
            label: step.label.clone(),
            dirty_cells: dirty.len(),
            retraced_cells,
            gpu_trace_millis: trace_ms,
            gpu_denoise_millis: denoise_ms,
            gpu_temporal_millis: temporal_ms,
            band_luminance: luma,
            indirect_image: path,
        });
    }

    // Leave the resources re-traceable in full for any later reuse.
    indirect.set_full_trace_region(&ctx.queue);

    Ok(SequenceReport {
        adapter: ctx.adapter_name().to_string(),
        backend: format!("{:?}", ctx.backend()),
        width: target.width,
        height: target.height,
        total_cells,
        band,
        halo_cells,
        temporal_weight,
        steps: report_steps,
    })
}

/// Round up to an even value of at least 2.
fn even(value: u32) -> u32 {
    let value = value.max(2);
    value + (value & 1)
}

/// Clamped cell AABB `[lo, hi)` covering every dirty cell, grown by `halo` on
/// each side. An empty dirty set yields a zero-volume region so the trace does
/// nothing.
fn trace_region(dirty: &[(u32, u32)], halo: u32) -> (glam::UVec3, glam::UVec3) {
    let dim = LIGHT_VOLUME_DIM;
    if dirty.is_empty() {
        return (glam::UVec3::ZERO, glam::UVec3::ZERO);
    }
    let mut lo = glam::UVec3::splat(dim);
    let mut hi = glam::UVec3::ZERO;
    for &(idx, _) in dirty {
        let cell = glam::UVec3::new(idx % dim, (idx / dim) % dim, idx / (dim * dim));
        lo = lo.min(cell);
        hi = hi.max(cell + glam::UVec3::ONE);
    }
    let lo = lo.saturating_sub(glam::UVec3::splat(halo));
    let hi = (hi + glam::UVec3::splat(halo)).min(glam::UVec3::splat(dim));
    (lo, hi)
}

fn band_luminance(rgba: &[u8], width: u32, height: u32, band: [f32; 2]) -> f32 {
    let x0 = (band[0] * width as f32).round() as u32;
    let x1 = ((band[1] * width as f32).round() as u32).min(width);
    let mut sum = 0.0f64;
    let mut count = 0u64;
    for y in 0..height {
        for x in x0..x1 {
            let p = ((y * width + x) * 4) as usize;
            sum += 0.2126 * f64::from(rgba[p])
                + 0.7152 * f64::from(rgba[p + 1])
                + 0.0722 * f64::from(rgba[p + 2]);
            count += 1;
        }
    }
    (sum / count.max(1) as f64) as f32
}

fn slugify(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}
