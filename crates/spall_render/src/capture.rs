//! Bounded offscreen capture for the explicit T12 shadow/HDR/tone-map passes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use image::{ImageBuffer, Rgba};

use crate::camera::Camera;
use crate::context::{RenderContext, RenderError};
use crate::indirect::{LIGHT_VOLUME_DIM, LightingUpdate, LightingVolume};
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

// --- G2 / T15 frame-cost harness ------------------------------------------
//
// The G2 graphics gate (`docs/validation.md` "G2") asks for each render pass's
// timing and the total frame-cost percentiles at a fixed 1920x1080 with fixed
// exposure/sun/camera/material settings. [`capture_scene`] renders one settled
// frame and reports a single-frame [`PassTiming`]; this path renders the same
// scene for many consecutive frames and reduces the per-pass device timings to
// nearest-rank percentiles.

/// Nearest-rank percentile summary of a set of millisecond samples.
///
/// The percentile convention matches `spall_server::commit_latency`: sort
/// ascending, take the sample at rank `ceil(q * n)` clamped to `[1, n]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameStats {
    pub samples: usize,
    pub min_millis: f64,
    pub p50_millis: f64,
    pub p95_millis: f64,
    pub p99_millis: f64,
    pub max_millis: f64,
    pub mean_millis: f64,
}

impl FrameStats {
    /// Reduce raw millisecond samples to a percentile summary. `None` for an
    /// empty set.
    pub fn from_samples(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted.len();
        let rank = |q: f64| -> f64 {
            let r = ((q * n as f64).ceil() as usize).clamp(1, n);
            sorted[r - 1]
        };
        let sum: f64 = sorted.iter().sum();
        Some(Self {
            samples: n,
            min_millis: sorted[0],
            p50_millis: rank(0.50),
            p95_millis: rank(0.95),
            p99_millis: rank(0.99),
            max_millis: sorted[n - 1],
            mean_millis: sum / n as f64,
        })
    }
}

/// Settings for a [`capture_frame_series`] run.
#[derive(Debug, Clone, Copy)]
pub struct FrameSeriesOptions {
    pub width: u32,
    pub height: u32,
    /// Fixed linear exposure, held across every frame.
    pub exposure: f32,
    /// Frames rendered and discarded before measurement so shader/pipeline
    /// warm-up and first-use allocations stay out of the percentiles.
    pub warmup_frames: u32,
    /// Frames folded into the percentiles.
    pub measured_frames: u32,
}

impl Default for FrameSeriesOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            exposure: 1.0,
            warmup_frames: 10,
            measured_frames: 60,
        }
    }
}

/// Per-pass-family GPU percentile blocks for a [`capture_frame_series`] run.
/// Every field is `None` when the adapter has no timestamp queries.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameSeriesPasses {
    /// Sum of every measured pass family for the frame.
    pub gpu_total: Option<FrameStats>,
    pub indirect_trace: Option<FrameStats>,
    pub indirect_denoise: Option<FrameStats>,
    pub shadow: Option<FrameStats>,
    pub opaque: Option<FrameStats>,
    pub tone_map: Option<FrameStats>,
}

/// Result of a [`capture_frame_series`] run.
#[derive(Debug, Clone)]
pub struct FrameSeriesReport {
    pub adapter: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub warmup_frames: u32,
    pub measured_frames: u32,
    /// `true` iff the measured frames carry real device timings.
    pub gpu_timing_available: bool,
    pub passes: FrameSeriesPasses,
    pub indirect_enabled: bool,
    pub indirect_cells: usize,
    /// First measured frame, kept for the visual acceptance record.
    pub first_image: PathBuf,
    /// Last measured frame.
    pub last_image: PathBuf,
}

/// Render `scene` for `warmup_frames + measured_frames` consecutive frames at a
/// fixed size and exposure — only the `Shaded` view, i.e. the passes the G2
/// frame target is about: indirect trace + denoise, the shadow cascades, one
/// opaque pass and one tone-map pass — and report nearest-rank percentiles of
/// each pass family's device time over the measured frames.
///
/// This is the G2 / T15 GPU frame-cost path: it measures the device-time
/// distribution of a settled frame. It does **not** model a persistent-resource
/// client frame loop or the CPU frame budget — each frame here rebuilds the
/// pipeline and re-uploads the scene, so the CPU-side numbers from the inner
/// [`capture_scene`] calls are not representative and are not reported. A
/// representative client-frame loop and the CPU p95 remain later T15 increments.
pub fn capture_frame_series(
    ctx: &RenderContext,
    scene: &Scene,
    out_dir: &Path,
    opts: &FrameSeriesOptions,
) -> Result<FrameSeriesReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;
    let scratch = out_dir.join("_frame");
    let capture_opts = CaptureOptions {
        width: opts.width,
        height: opts.height,
        views: vec![DebugView::Shaded],
        budget: UploadBudget::default(),
        exposure: opts.exposure,
    };

    let total = opts.warmup_frames.saturating_add(opts.measured_frames);
    let mut total_ms = Vec::new();
    let mut trace_ms = Vec::new();
    let mut denoise_ms = Vec::new();
    let mut shadow_ms = Vec::new();
    let mut opaque_ms = Vec::new();
    let mut tone_ms = Vec::new();
    let (mut adapter, mut backend) = (String::new(), String::new());
    let (mut indirect_enabled, mut indirect_cells) = (false, 0usize);
    let first_image = out_dir.join("first.png");
    let last_image = out_dir.join("last.png");

    for frame in 0..total {
        let report = capture_scene(ctx, scene, &scratch, &capture_opts)?;
        adapter = report.adapter.clone();
        backend = report.backend.clone();
        indirect_enabled = report.indirect_enabled;
        indirect_cells = report.indirect_cells;

        let shaded = scratch.join("shaded.png");
        if frame == opts.warmup_frames {
            let _ = std::fs::copy(&shaded, &first_image);
        }
        if frame + 1 == total {
            let _ = std::fs::copy(&shaded, &last_image);
        }
        if frame < opts.warmup_frames {
            continue;
        }
        if let Some(passes) = report.timing.gpu_passes {
            total_ms.push(passes.total());
            trace_ms.push(passes.indirect_trace_millis);
            denoise_ms.push(passes.indirect_denoise_millis);
            shadow_ms.push(passes.shadow_millis);
            opaque_ms.push(passes.opaque_millis);
            tone_ms.push(passes.tone_map_millis);
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    let passes = FrameSeriesPasses {
        gpu_total: FrameStats::from_samples(&total_ms),
        indirect_trace: FrameStats::from_samples(&trace_ms),
        indirect_denoise: FrameStats::from_samples(&denoise_ms),
        shadow: FrameStats::from_samples(&shadow_ms),
        opaque: FrameStats::from_samples(&opaque_ms),
        tone_map: FrameStats::from_samples(&tone_ms),
    };
    Ok(FrameSeriesReport {
        gpu_timing_available: passes.gpu_total.is_some(),
        adapter,
        backend,
        width: capture_opts.width,
        height: capture_opts.height,
        warmup_frames: opts.warmup_frames,
        measured_frames: opts.measured_frames,
        passes,
        indirect_enabled,
        indirect_cells,
        first_image,
        last_image,
    })
}

// --- G2 / T15 persistent-resource frame loop (increment 2) -----------------
//
// Increment 1 (`capture_frame_series`) rebuilds the pipeline and re-uploads the
// scene every frame and always re-traces the whole 128^3 lighting cache, so its
// numbers are a *cold full-retrace* upper bound with per-frame-rebuild noise.
// This path builds every GPU resource once and then runs a lean per-frame loop
// — dispatch indirect (trace bounded to a small region or nothing, denoise +
// temporal full-cache), opaque, tone map — over a static scene and camera, so
// it measures the **settled-frame** device cost and the renderer's per-frame
// CPU encode work. Shadow cascades are rendered once (a settled frame does not
// re-cast them) and reported separately.

/// Centre-anchored cache-cell box `[lo, hi)` with the given edge length, for the
/// per-frame bounded re-trace. `edge == 0` yields a zero-volume box (nothing
/// re-traced).
fn centered_cell_box(edge: u32) -> (glam::UVec3, glam::UVec3) {
    let dim = LIGHT_VOLUME_DIM;
    if edge == 0 {
        return (glam::UVec3::ZERO, glam::UVec3::ZERO);
    }
    let edge = edge.min(dim);
    let lo = glam::UVec3::splat(dim / 2 - edge / 2);
    let hi = (lo + glam::UVec3::splat(edge)).min(glam::UVec3::splat(dim));
    (lo, hi)
}

/// Settings for a [`capture_frame_loop`] run.
#[derive(Debug, Clone, Copy)]
pub struct FrameLoopOptions {
    pub width: u32,
    pub height: u32,
    /// Fixed linear exposure, held across every frame.
    pub exposure: f32,
    /// Frames rendered and discarded (with a full re-trace) before measurement.
    pub warmup_frames: u32,
    /// Frames folded into the percentiles.
    pub measured_frames: u32,
    /// Edge length, in cache cells, of the box re-traced each measured frame —
    /// a stand-in for a live client's per-frame incremental lighting edit. `0`
    /// re-traces nothing: a settled frame with no lighting change.
    pub retrace_edge_cells: u32,
    /// Temporal accumulation weight for the measured frames. `1.0` disables it;
    /// a live client blends with a small weight (e.g. `0.1`).
    pub temporal_weight: f32,
}

impl Default for FrameLoopOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            exposure: 1.0,
            warmup_frames: 10,
            measured_frames: 120,
            retrace_edge_cells: 0,
            temporal_weight: 0.1,
        }
    }
}

/// Result of a [`capture_frame_loop`] run. Every `FrameStats` is `None` when the
/// adapter has no timestamp queries; `cpu_frame` is always measured.
#[derive(Debug, Clone)]
pub struct FrameLoopReport {
    pub adapter: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub warmup_frames: u32,
    pub measured_frames: u32,
    pub retrace_edge_cells: u32,
    pub temporal_weight: f32,
    pub gpu_timing_available: bool,
    pub indirect_enabled: bool,
    pub indirect_cells: usize,
    /// The one-off shadow-cascade render, measured after warm-up. A settled
    /// frame does not re-cast shadows, so this is not in `gpu_frame`.
    pub shadow_once_millis: Option<f64>,
    /// Per measured frame: indirect trace + denoise + temporal + opaque + tone
    /// map (no shadow — see `shadow_once_millis`).
    pub gpu_frame: Option<FrameStats>,
    pub gpu_indirect_trace: Option<FrameStats>,
    pub gpu_indirect_denoise: Option<FrameStats>,
    pub gpu_indirect_temporal: Option<FrameStats>,
    pub gpu_opaque: Option<FrameStats>,
    pub gpu_tone_map: Option<FrameStats>,
    /// CPU wall time to build the two command encoders and submit them for one
    /// frame — no GPU wait, no readback. The renderer's per-frame encode cost,
    /// not a full client CPU frame (no simulation / culling / input).
    pub cpu_frame: Option<FrameStats>,
    /// Per measured frame: that frame's own CPU encode cost **plus** that same
    /// frame's own GPU device total — the directly-paired serial
    /// submit-then-sync frame duration, percentiled over the measured frames.
    /// This is a true measured upper bound on a pipelined client (which overlaps
    /// the two); `max(cpu_frame.p95, gpu_frame.p95)` is the matching lower
    /// bound. It is **not** `cpu_frame.p95 + gpu_frame.p95` — a sum of marginal
    /// percentiles is neither measured nor a valid bound on the p95 of the sum.
    pub serial_frame: Option<FrameStats>,
    pub first_image: PathBuf,
    pub last_image: PathBuf,
}

/// Build every GPU resource once, then render `scene` from a fixed camera for
/// `warmup_frames + measured_frames` consecutive frames and report nearest-rank
/// percentiles of each pass family's device time and of the per-frame CPU
/// encode cost over the measured frames.
///
/// Warm-up frames run a full-cache re-trace; measured frames re-trace only a
/// `retrace_edge_cells` box (or nothing) with `temporal_weight` accumulation,
/// modelling a live client's settled / incremental-edit frame. Shadow cascades
/// are rendered and timed once. Only the first and last measured frames are
/// read back (for `first.png` / `last.png`).
///
/// This is the representative half of the G2 / T15 frame-cost evidence; it
/// still does not model a *pipelined* client (CPU frame N+1 overlapping GPU
/// frame N) — `cpu_frame` and `gpu_frame` are measured on a serial
/// submit-then-sync loop.
pub fn capture_frame_loop(
    ctx: &RenderContext,
    scene: &Scene,
    out_dir: &Path,
    opts: &FrameLoopOptions,
) -> Result<FrameLoopReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;

    let pipeline = ScenePipeline::new(&ctx.device);
    let materials = pipeline.material_buffer(&ctx.device, &scene.materials);
    let indirect =
        pipeline.indirect_resources(&ctx.device, &ctx.queue, scene.lighting.as_ref(), &materials);
    let (width, height) = (even(opts.width), even(opts.height));
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

    // Shadow cascades: rendered and timed once (static camera + sun).
    let shadow_timer = GpuTimer::new(ctx, CASCADE_COUNT as u32);
    let (light_matrices, _) = ScenePipeline::cascade_data(&scene.camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-g2-loop-shadow-encoder"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let timestamp_writes = shadow_timer
            .as_ref()
            .map(|timer| timer.writes(cascade as u32));
        let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-g2-loop-shadow-pass"),
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
    let shadow_once_millis = shadow_timer
        .and_then(|timer| timer.millis(ctx))
        .map(|times| times.iter().sum());

    // Static scene / camera / exposure → build the display bind groups once.
    let scene_bind = pipeline.scene_bind_group(
        &ctx.device,
        &ctx.queue,
        &scene.camera,
        DebugView::Shaded,
        opts.exposure,
        &materials,
    );
    let tone_bind = pipeline.tone_bind_group(
        &ctx.device,
        &ctx.queue,
        target.hdr_view(),
        opts.exposure,
        false,
    );

    indirect.set_full_trace_region(&ctx.queue);
    indirect.set_temporal(&ctx.queue, 1.0, TEMPORAL_SLACK);
    let (measured_lo, measured_hi) = centered_cell_box(opts.retrace_edge_cells);

    let total = opts
        .warmup_frames
        .saturating_add(opts.measured_frames)
        .max(1);
    let first_image = out_dir.join("first.png");
    let last_image = out_dir.join("last.png");
    let mut trace_ms = Vec::new();
    let mut denoise_ms = Vec::new();
    let mut temporal_ms = Vec::new();
    let mut opaque_ms = Vec::new();
    let mut tone_ms = Vec::new();
    let mut frame_ms = Vec::new();
    let mut cpu_ms = Vec::new();
    let mut serial_ms = Vec::new();

    for frame in 0..total {
        if frame == opts.warmup_frames {
            // Enter the measured window: bounded re-trace + the caller's weight.
            indirect.set_trace_region(&ctx.queue, measured_lo, measured_hi);
            indirect.set_temporal(&ctx.queue, opts.temporal_weight, TEMPORAL_SLACK);
        }
        let measured = frame >= opts.warmup_frames;
        let need_png = frame == opts.warmup_frames || frame + 1 == total;

        let timer = GpuTimer::new(ctx, 5);
        let cpu_start = Instant::now();

        let mut lighting_encoder =
            ctx.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("spall-g2-loop-lighting-encoder"),
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

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-g2-loop-frame-encoder"),
            });
        {
            let timestamp_writes = timer.as_ref().map(|timer| timer.writes(3));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-g2-loop-opaque-pass"),
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
            pass.set_bind_group(1, &indirect.history_display_bind, &[]);
            draw_meshes(&mut pass, &draws);
        }
        {
            let timestamp_writes = timer.as_ref().map(|timer| timer.writes(4));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-g2-loop-tone-pass"),
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
        if need_png {
            target.copy_to_readback(&mut encoder);
        }
        ctx.queue.submit([encoder.finish()]);
        let frame_cpu_millis = cpu_start.elapsed().as_secs_f64() * 1000.0;

        if need_png {
            let rgba = target.read_rgba(ctx)?;
            let image: ImageBuffer<Rgba<u8>, _> =
                ImageBuffer::from_raw(target.width, target.height, rgba)
                    .expect("readback has width*height*4 bytes");
            let path = if frame == opts.warmup_frames {
                &first_image
            } else {
                &last_image
            };
            image.save(path).map_err(|source| RenderError::Image {
                path: path.display().to_string(),
                source,
            })?;
        }

        let times = timer.and_then(|timer| timer.millis(ctx));
        if measured {
            cpu_ms.push(frame_cpu_millis);
            if let Some([trace, denoise, temporal, opaque, tone, ..]) = times.as_deref() {
                trace_ms.push(*trace);
                denoise_ms.push(*denoise);
                temporal_ms.push(*temporal);
                opaque_ms.push(*opaque);
                tone_ms.push(*tone);
                let gpu_total = trace + denoise + temporal + opaque + tone;
                frame_ms.push(gpu_total);
                serial_ms.push(frame_cpu_millis + gpu_total);
            }
        }
    }
    // Leave the cache re-traceable in full for any later reuse.
    indirect.set_full_trace_region(&ctx.queue);

    let gpu_frame = FrameStats::from_samples(&frame_ms);
    Ok(FrameLoopReport {
        gpu_timing_available: gpu_frame.is_some(),
        adapter: ctx.adapter_name().to_string(),
        backend: format!("{:?}", ctx.backend()),
        width: target.width,
        height: target.height,
        warmup_frames: opts.warmup_frames,
        measured_frames: opts.measured_frames,
        retrace_edge_cells: opts.retrace_edge_cells,
        temporal_weight: opts.temporal_weight,
        indirect_enabled: indirect.enabled,
        indirect_cells: indirect.cells,
        shadow_once_millis,
        gpu_frame,
        gpu_indirect_trace: FrameStats::from_samples(&trace_ms),
        gpu_indirect_denoise: FrameStats::from_samples(&denoise_ms),
        gpu_indirect_temporal: FrameStats::from_samples(&temporal_ms),
        gpu_opaque: FrameStats::from_samples(&opaque_ms),
        gpu_tone_map: FrameStats::from_samples(&tone_ms),
        cpu_frame: FrameStats::from_samples(&cpu_ms),
        serial_frame: FrameStats::from_samples(&serial_ms),
        first_image,
        last_image,
    })
}

// --- G2 / T15 moving-frame sequence + quality metrics (increment 3) --------
//
// Increment 2 measured a *static* settled frame. The G2 gate also asks for "at
// least 120 consecutive moving frames" and for light leakage, ghosting, noise
// and shadow instability to be flagged for review. This path renders a scene
// through a caller-supplied per-frame camera + lighting-update path on the
// increment-2 persistent-resource loop, samples luminance in named probe bands
// every frame, and the caller reduces those traces to quality metrics.

/// One frame of a [`capture_motion_sequence`] run: where the camera is and what
/// changed in the world since the previous frame. An empty `update` re-renders
/// the same lighting from a new camera pose (a pan).
#[derive(Debug, Clone)]
pub struct MotionFrame {
    pub camera: Camera,
    pub update: LightingUpdate,
}

/// A fractional image-x band `[x0, x1)` (full height) whose mean sRGB luminance
/// is sampled every frame.
#[derive(Debug, Clone)]
pub struct ProbeBand {
    pub name: String,
    pub band: [f32; 2],
}

/// Per-band luminance trace over a [`capture_motion_sequence`] run.
#[derive(Debug, Clone)]
pub struct BandTrace {
    pub name: String,
    pub band: [f32; 2],
    /// One mean sRGB luminance per rendered frame.
    pub luminance: Vec<f32>,
}

/// A frame kept as a PNG during a [`capture_motion_sequence`] run.
#[derive(Debug, Clone)]
pub struct MotionImage {
    pub frame: usize,
    pub path: PathBuf,
}

/// Settings for a [`capture_motion_sequence`] run.
#[derive(Debug, Clone, Copy)]
pub struct MotionSequenceOptions {
    pub width: u32,
    pub height: u32,
    pub exposure: f32,
    /// Which view each frame renders. `Shaded` is the real client frame;
    /// `IndirectOnly` isolates transported indirect radiance, so a lighting
    /// change (a moving occluder's shadow) is not swamped by direct light —
    /// the sensitive view for ghosting / trail detection.
    pub view: DebugView,
    /// Temporal accumulation weight applied every frame (`1.0` disables it).
    pub temporal_weight: f32,
    /// Cells added on each side of a frame's dirty cell-AABB before re-tracing.
    pub halo_cells: u32,
    /// Save every `png_stride`-th frame as a PNG (plus the first and last).
    /// `0` keeps only the first and last.
    pub png_stride: usize,
}

impl Default for MotionSequenceOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            exposure: 1.0,
            view: DebugView::Shaded,
            temporal_weight: 0.1,
            halo_cells: 12,
            png_stride: 20,
        }
    }
}

/// Result of a [`capture_motion_sequence`] run.
#[derive(Debug, Clone)]
pub struct MotionSequenceReport {
    pub adapter: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    pub bands: Vec<BandTrace>,
    pub gpu_timing_available: bool,
    /// Per-frame GPU device time (trace + denoise + temporal + opaque + tone map).
    pub gpu_frame: Option<FrameStats>,
    /// Per-frame renderer CPU encode cost (no GPU wait; the readback is excluded).
    pub cpu_frame: Option<FrameStats>,
    pub indirect_cells: usize,
    pub images: Vec<MotionImage>,
}

/// Mean absolute frame-to-frame change of `samples`, as a fraction of their
/// mean level — a temporal-instability / flicker index. `0.0` for a perfectly
/// steady trace; a noisy or unstable band pushes it up.
pub fn flicker_index(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mean = samples.iter().copied().sum::<f32>() / samples.len() as f32;
    if mean.abs() < f32::EPSILON {
        return 0.0;
    }
    let step_sum: f32 = samples.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    (step_sum / (samples.len() - 1) as f32) / mean
}

/// Largest single-frame absolute change in `samples`, as a fraction of their
/// mean level.
pub fn max_step_fraction(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    let mean = samples.iter().copied().sum::<f32>() / samples.len() as f32;
    if mean.abs() < f32::EPSILON {
        return 0.0;
    }
    let max_step = samples
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0_f32, f32::max);
    max_step / mean
}

/// First index at or after `from` where `samples` is within `tol_frac` of
/// `target` (relative to `target`) and stays within it through the end of the
/// trace. `None` if it never settles.
pub fn settle_index(samples: &[f32], target: f32, tol_frac: f32, from: usize) -> Option<usize> {
    if target.abs() < f32::EPSILON {
        return None;
    }
    let tol = (target * tol_frac).abs();
    let within = |v: f32| (v - target).abs() <= tol;
    (from..samples.len()).find(|&i| samples[i..].iter().all(|&v| within(v)))
}

/// Render `scene` through `frames` on the persistent-resource loop, sampling
/// each [`ProbeBand`] every frame. `scene.lighting` is required when any frame
/// carries a non-empty `update`; a pan (empty updates) works without it too but
/// then no lighting cache is bound, so pass a lit scene.
///
/// Warm-up: one full-cache re-trace from `frames[0].camera` before the sequence
/// so the history is seeded. Each frame then re-uploads only its dirty cells and
/// re-traces only the dirty cell-AABB grown by `halo_cells` (nothing for an
/// empty update), exactly like a live client.
pub fn capture_motion_sequence(
    ctx: &RenderContext,
    scene: &Scene,
    frames: &[MotionFrame],
    probes: &[ProbeBand],
    out_dir: &Path,
    opts: &MotionSequenceOptions,
) -> Result<MotionSequenceReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;
    if frames.is_empty() {
        return Err(RenderError::Gpu(
            "capture_motion_sequence needs frames".into(),
        ));
    }

    let mut volume = scene.lighting.clone();
    let pipeline = ScenePipeline::new(&ctx.device);
    let materials = pipeline.material_buffer(&ctx.device, &scene.materials);
    let indirect =
        pipeline.indirect_resources(&ctx.device, &ctx.queue, volume.as_ref(), &materials);
    let (width, height) = (even(opts.width), even(opts.height));
    let target = OffscreenTarget::new(&ctx.device, width, height);

    // Union of every frame's camera frustum is hard to bound cheaply, so upload
    // every scene item (the fixtures are small) and let the passes cull.
    let mut draws = Vec::new();
    for item in &scene.items {
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

    // Shadow cascades once, framed on the first camera.
    let (light_matrices, _) = ScenePipeline::cascade_data(&frames[0].camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-g2-motion-shadow-encoder"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-g2-motion-shadow-pass"),
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

    let tone_bind = pipeline.tone_bind_group(
        &ctx.device,
        &ctx.queue,
        target.hdr_view(),
        opts.exposure,
        opts.view != DebugView::Shaded,
    );

    // Warm-up: one full-cache re-trace + history seed from the first camera.
    indirect.set_full_trace_region(&ctx.queue);
    indirect.set_temporal(&ctx.queue, 1.0, TEMPORAL_SLACK);
    render_motion_frame(
        ctx,
        &pipeline,
        &indirect,
        &target,
        &materials,
        &draws,
        scene,
        &frames[0].camera,
        opts.view,
        opts.exposure,
        &tone_bind,
        None,
    )?;

    indirect.set_temporal(&ctx.queue, opts.temporal_weight, TEMPORAL_SLACK);

    let mut traces: Vec<BandTrace> = probes
        .iter()
        .map(|p| BandTrace {
            name: p.name.clone(),
            band: p.band,
            luminance: Vec::with_capacity(frames.len()),
        })
        .collect();
    let mut gpu_ms = Vec::with_capacity(frames.len());
    let mut cpu_ms = Vec::with_capacity(frames.len());
    let mut images = Vec::new();

    for (i, frame) in frames.iter().enumerate() {
        if !frame.update.regions.is_empty() || !frame.update.dirty_world_bounds.is_empty() {
            if let Some(volume) = volume.as_mut() {
                volume.apply_update(&frame.update);
                let dirty = volume.take_dirty();
                indirect.upload_dirty(&ctx.queue, &dirty);
                let (lo, hi) = trace_region(&dirty, opts.halo_cells);
                indirect.set_trace_region(&ctx.queue, lo, hi);
            }
        } else {
            indirect.set_trace_region(&ctx.queue, glam::UVec3::ZERO, glam::UVec3::ZERO);
        }

        let timer = GpuTimer::new(ctx, 5);
        let cpu_start = Instant::now();
        render_motion_frame(
            ctx,
            &pipeline,
            &indirect,
            &target,
            &materials,
            &draws,
            scene,
            &frame.camera,
            opts.view,
            opts.exposure,
            &tone_bind,
            timer.as_ref(),
        )?;
        let frame_cpu_millis = cpu_start.elapsed().as_secs_f64() * 1000.0;

        let rgba = target.read_rgba(ctx)?;
        for (probe, trace) in probes.iter().zip(traces.iter_mut()) {
            trace.luminance.push(band_luminance(
                &rgba,
                target.width,
                target.height,
                probe.band,
            ));
        }

        let keep_png =
            i == 0 || i + 1 == frames.len() || (opts.png_stride > 0 && i % opts.png_stride == 0);
        if keep_png {
            let image: ImageBuffer<Rgba<u8>, _> =
                ImageBuffer::from_raw(target.width, target.height, rgba)
                    .expect("readback has width*height*4 bytes");
            let path = out_dir.join(format!("frame_{i:04}.png"));
            image.save(&path).map_err(|source| RenderError::Image {
                path: path.display().to_string(),
                source,
            })?;
            images.push(MotionImage { frame: i, path });
        }

        cpu_ms.push(frame_cpu_millis);
        if let Some([trace, denoise, temporal, opaque, tone, ..]) =
            timer.and_then(|timer| timer.millis(ctx)).as_deref()
        {
            gpu_ms.push(trace + denoise + temporal + opaque + tone);
        }
    }
    indirect.set_full_trace_region(&ctx.queue);

    let gpu_frame = FrameStats::from_samples(&gpu_ms);
    Ok(MotionSequenceReport {
        gpu_timing_available: gpu_frame.is_some(),
        adapter: ctx.adapter_name().to_string(),
        backend: format!("{:?}", ctx.backend()),
        width: target.width,
        height: target.height,
        frames: frames.len(),
        bands: traces,
        gpu_frame,
        cpu_frame: FrameStats::from_samples(&cpu_ms),
        indirect_cells: indirect.cells,
        images,
    })
}

/// One frame of a motion sequence: dispatch indirect + temporal, then opaque +
/// tone map (in `view`) into `target`, always copying the result to the
/// readback buffer. Returns nothing; the caller reads `target` back.
#[allow(clippy::too_many_arguments)]
fn render_motion_frame(
    ctx: &RenderContext,
    pipeline: &ScenePipeline,
    indirect: &crate::indirect::IndirectResources,
    target: &OffscreenTarget,
    materials: &wgpu::Buffer,
    draws: &[GpuMesh],
    scene: &Scene,
    camera: &Camera,
    view: DebugView,
    exposure: f32,
    tone_bind: &wgpu::BindGroup,
    timer: Option<&GpuTimer>,
) -> Result<(), RenderError> {
    let mut lighting_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-g2-motion-lighting-encoder"),
        });
    pipeline.dispatch_indirect(
        &mut lighting_encoder,
        indirect,
        timer.map(|t| t.compute_writes(0)),
        timer.map(|t| t.compute_writes(1)),
    );
    pipeline.dispatch_indirect_temporal(
        &mut lighting_encoder,
        indirect,
        timer.map(|t| t.compute_writes(2)),
    );
    ctx.queue.submit([lighting_encoder.finish()]);

    let scene_bind =
        pipeline.scene_bind_group(&ctx.device, &ctx.queue, camera, view, exposure, materials);
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-g2-motion-frame-encoder"),
        });
    {
        let timestamp_writes = timer.map(|t| t.writes(3));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-g2-motion-opaque-pass"),
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
        pass.set_bind_group(1, &indirect.history_display_bind, &[]);
        draw_meshes(&mut pass, draws);
    }
    {
        let timestamp_writes = timer.map(|t| t.writes(4));
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-g2-motion-tone-pass"),
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
        pass.set_bind_group(0, tone_bind, &[]);
        pass.draw(0..3, 0..1);
    }
    target.copy_to_readback(&mut encoder);
    ctx.queue.submit([encoder.finish()]);
    Ok(())
}

// --- G2 / T15 bounded per-tick destruction sequence (increment 6) ----------
//
// Increment 5 (`--scene g2-collapse`) lights each captured collapse tick with a
// *cold* full re-trace of a clipmap rebuilt from scratch — the increment-1 cost
// (~12 ms p50), explicitly not a client frame. This path runs the collapse on
// the increment-2 persistent-resource loop: GPU resources are built once, and
// each tick's committed cuts and moving-body poses arrive as a bounded
// [`LightingUpdate`] (translated by the caller from the authoritative sim), so
// only the changed cells re-upload and only the changed region (grown by a
// halo) re-traces. It reports the per-pass device timings, the directly-paired
// per-frame serial total, the bounded re-upload / re-trace sizes, and the
// per-probe luminance trace the caller checks for stale vacated shadows and
// against a full-refresh reference.

/// One tick of a [`capture_collapse_sequence`] run: the camera, the freshly
/// meshed world (terrain + every detached body at its current pose), and the
/// bounded lighting delta since the previous tick.
pub struct CollapseFrame {
    pub camera: Camera,
    /// Terrain + body meshes for this tick, re-uploaded before the frame.
    pub items: Vec<crate::scene::SceneItem>,
    /// Committed cuts + moving-body poses since the previous tick, as the T14
    /// partial-update DTO. Empty for a tick with no world change.
    pub update: LightingUpdate,
}

/// Settings for a [`capture_collapse_sequence`] run.
#[derive(Debug, Clone, Copy)]
pub struct CollapseSequenceOptions {
    pub width: u32,
    pub height: u32,
    pub exposure: f32,
    /// Temporal accumulation weight applied every tick (`1.0` disables it).
    pub temporal_weight: f32,
    /// Cells added on each side of a tick's dirty cell-AABB before re-tracing.
    pub halo_cells: u32,
    /// Save every `png_stride`-th frame as a PNG (plus the first and last).
    /// `0` keeps only the first and last.
    pub png_stride: usize,
}

impl Default for CollapseSequenceOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            exposure: 1.0,
            temporal_weight: 0.1,
            halo_cells: 12,
            png_stride: 20,
        }
    }
}

/// Result of a [`capture_collapse_sequence`] run.
#[derive(Debug, Clone)]
pub struct CollapseSequenceReport {
    pub adapter: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    pub bands: Vec<BandTrace>,
    /// Newly-dirtied clipmap cells per tick — the bounded GPU re-upload set.
    pub dirty_cells: Vec<usize>,
    /// Clipmap cells the trace recomputed per tick (dirty AABB + halo; `0` for a
    /// tick with no lighting change).
    pub retraced_cells: Vec<u64>,
    pub total_cells: u64,
    pub gpu_timing_available: bool,
    /// Per timed tick: total GPU device time (trace + denoise + temporal +
    /// opaque + tone map). Empty when the adapter has no timestamp queries;
    /// otherwise one entry per rendered tick — index-aligned with `dirty_cells`.
    pub gpu_frame_millis: Vec<f64>,
    /// Per timed tick: the lighting cost only (trace + denoise + temporal).
    pub gpu_lighting_millis: Vec<f64>,
    /// Per timed tick: that tick's CPU encode + its own GPU device total.
    pub serial_frame_millis: Vec<f64>,
    /// Per tick: indirect trace + denoise + temporal + opaque + tone map.
    pub gpu_frame: Option<FrameStats>,
    /// Per tick: the lighting cost only (trace + denoise + temporal).
    pub gpu_lighting: Option<FrameStats>,
    pub gpu_indirect_trace: Option<FrameStats>,
    pub gpu_indirect_denoise: Option<FrameStats>,
    pub gpu_indirect_temporal: Option<FrameStats>,
    /// Per-tick renderer CPU encode cost (no GPU wait, no readback).
    pub cpu_frame: Option<FrameStats>,
    /// Per tick: that tick's own CPU encode cost plus its own GPU device total —
    /// the directly-paired serial submit-then-sync frame duration, percentiled.
    /// A measured upper bound on a pipelined client; not a sum of marginal
    /// percentiles.
    pub serial_frame: Option<FrameStats>,
    pub indirect_cells: usize,
    /// The persistent clipmap after the last tick — the caller diffs it against
    /// a from-scratch resample of the final world to catch stale/erased cells.
    pub final_lighting: LightingVolume,
    pub images: Vec<MotionImage>,
}

fn upload_items(
    ctx: &RenderContext,
    items: &[crate::scene::SceneItem],
) -> Result<Vec<GpuMesh>, RenderError> {
    let mut draws = Vec::new();
    for item in items {
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
    Ok(draws)
}

/// Build every GPU resource once from `base` (its `lighting` volume is the seed
/// clipmap, its `clear`/`materials` are held constant), warm up with one
/// full-cache re-trace + history seed, then render each [`CollapseFrame`] in
/// turn: re-upload that tick's meshes, apply its bounded [`LightingUpdate`] to
/// the persistent clipmap (re-uploading only the changed cells, re-tracing only
/// the changed cell-AABB grown by `halo_cells`), render `Shaded`, and sample
/// every [`ProbeBand`].
///
/// This is the increment-6 half of the G2 / T15 destruction evidence — the
/// bounded per-tick cost of an active collapse on the persistent loop, as
/// opposed to increment 5's cold full re-trace per captured tick. It still runs
/// a serial submit-then-sync loop (no pipelined CPU/GPU overlap) and the
/// broader CPU frame budget (simulation, culling, entity update) is out of
/// scope — `cpu_frame` is renderer encode only.
pub fn capture_collapse_sequence(
    ctx: &RenderContext,
    base: &Scene,
    frames: Vec<CollapseFrame>,
    probes: &[ProbeBand],
    out_dir: &Path,
    opts: &CollapseSequenceOptions,
) -> Result<CollapseSequenceReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|error| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(error),
    })?;
    if frames.is_empty() {
        return Err(RenderError::Gpu(
            "capture_collapse_sequence needs frames".into(),
        ));
    }
    let mut volume = base.lighting.clone().ok_or_else(|| {
        RenderError::Gpu("capture_collapse_sequence needs a seed lighting volume".into())
    })?;
    let total_cells = (LIGHT_VOLUME_DIM as u64).pow(3);

    let pipeline = ScenePipeline::new(&ctx.device);
    let materials = pipeline.material_buffer(&ctx.device, &base.materials);
    let indirect = pipeline.indirect_resources(&ctx.device, &ctx.queue, Some(&volume), &materials);
    let (width, height) = (even(opts.width), even(opts.height));
    let target = OffscreenTarget::new(&ctx.device, width, height);

    // Shadow cascades once, framed on the first tick's camera (static sun).
    let warm_draws = upload_items(ctx, &frames[0].items)?;
    let (light_matrices, _) = ScenePipeline::cascade_data(&frames[0].camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-g2-collapse-shadow-encoder"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let mut pass = shadow_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("spall-g2-collapse-shadow-pass"),
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
        draw_meshes(&mut pass, &warm_draws);
    }
    ctx.queue.submit([shadow_encoder.finish()]);

    let tone_bind = pipeline.tone_bind_group(
        &ctx.device,
        &ctx.queue,
        target.hdr_view(),
        opts.exposure,
        false,
    );

    // Warm-up: full-cache re-trace + history seed from the first tick.
    indirect.set_full_trace_region(&ctx.queue);
    indirect.set_temporal(&ctx.queue, 1.0, TEMPORAL_SLACK);
    render_motion_frame(
        ctx,
        &pipeline,
        &indirect,
        &target,
        &materials,
        &warm_draws,
        base,
        &frames[0].camera,
        DebugView::Shaded,
        opts.exposure,
        &tone_bind,
        None,
    )?;
    let _ = target.read_rgba(ctx)?;
    indirect.set_temporal(&ctx.queue, opts.temporal_weight, TEMPORAL_SLACK);

    let mut traces: Vec<BandTrace> = probes
        .iter()
        .map(|p| BandTrace {
            name: p.name.clone(),
            band: p.band,
            luminance: Vec::with_capacity(frames.len()),
        })
        .collect();
    let mut dirty_cells = Vec::with_capacity(frames.len());
    let mut retraced_cells = Vec::with_capacity(frames.len());
    let mut trace_ms = Vec::new();
    let mut denoise_ms = Vec::new();
    let mut temporal_ms = Vec::new();
    let mut lighting_ms = Vec::new();
    let mut gpu_ms = Vec::new();
    let mut cpu_ms = Vec::new();
    let mut serial_ms = Vec::new();
    let mut images = Vec::new();
    let frame_count = frames.len();

    for (i, frame) in frames.into_iter().enumerate() {
        let changed = volume.apply_update(&frame.update);
        let dirty = volume.take_dirty();
        indirect.upload_dirty(&ctx.queue, &dirty);
        let (lo, hi) = trace_region(&dirty, opts.halo_cells);
        indirect.set_trace_region(&ctx.queue, lo, hi);
        let retraced = u64::from((hi.x - lo.x) * (hi.y - lo.y) * (hi.z - lo.z));

        let draws = upload_items(ctx, &frame.items)?;
        let timer = GpuTimer::new(ctx, 5);
        let cpu_start = Instant::now();
        render_motion_frame(
            ctx,
            &pipeline,
            &indirect,
            &target,
            &materials,
            &draws,
            base,
            &frame.camera,
            DebugView::Shaded,
            opts.exposure,
            &tone_bind,
            timer.as_ref(),
        )?;
        let frame_cpu_millis = cpu_start.elapsed().as_secs_f64() * 1000.0;

        let rgba = target.read_rgba(ctx)?;
        for (probe, trace) in probes.iter().zip(traces.iter_mut()) {
            trace.luminance.push(band_luminance(
                &rgba,
                target.width,
                target.height,
                probe.band,
            ));
        }

        let keep_png =
            i == 0 || i + 1 == frame_count || (opts.png_stride > 0 && i % opts.png_stride == 0);
        if keep_png {
            let image: ImageBuffer<Rgba<u8>, _> =
                ImageBuffer::from_raw(target.width, target.height, rgba)
                    .expect("readback has width*height*4 bytes");
            let path = out_dir.join(format!("tick_{i:04}.png"));
            image.save(&path).map_err(|source| RenderError::Image {
                path: path.display().to_string(),
                source,
            })?;
            images.push(MotionImage { frame: i, path });
        }

        dirty_cells.push(changed);
        retraced_cells.push(retraced);
        cpu_ms.push(frame_cpu_millis);
        if let Some([tr, dn, tp, op, tn, ..]) = timer.and_then(|timer| timer.millis(ctx)).as_deref()
        {
            trace_ms.push(*tr);
            denoise_ms.push(*dn);
            temporal_ms.push(*tp);
            let lighting = tr + dn + tp;
            let gpu_total = lighting + op + tn;
            lighting_ms.push(lighting);
            gpu_ms.push(gpu_total);
            serial_ms.push(frame_cpu_millis + gpu_total);
        }
    }
    indirect.set_full_trace_region(&ctx.queue);
    let gpu_frame_millis = gpu_ms.clone();
    let gpu_lighting_millis = lighting_ms.clone();
    let serial_frame_millis = serial_ms.clone();

    let gpu_frame = FrameStats::from_samples(&gpu_ms);
    Ok(CollapseSequenceReport {
        gpu_timing_available: gpu_frame.is_some(),
        adapter: ctx.adapter_name().to_string(),
        backend: format!("{:?}", ctx.backend()),
        width: target.width,
        height: target.height,
        frames: frame_count,
        bands: traces,
        dirty_cells,
        retraced_cells,
        total_cells,
        gpu_frame_millis,
        gpu_lighting_millis,
        serial_frame_millis,
        gpu_frame,
        gpu_lighting: FrameStats::from_samples(&lighting_ms),
        gpu_indirect_trace: FrameStats::from_samples(&trace_ms),
        gpu_indirect_denoise: FrameStats::from_samples(&denoise_ms),
        gpu_indirect_temporal: FrameStats::from_samples(&temporal_ms),
        cpu_frame: FrameStats::from_samples(&cpu_ms),
        serial_frame: FrameStats::from_samples(&serial_ms),
        indirect_cells: indirect.cells,
        final_lighting: volume,
        images,
    })
}

#[cfg(test)]
mod tests {
    use super::{FrameStats, centered_cell_box, flicker_index, max_step_fraction, settle_index};
    use crate::indirect::LIGHT_VOLUME_DIM;

    #[test]
    fn frame_stats_are_empty_for_no_samples() {
        assert!(FrameStats::from_samples(&[]).is_none());
    }

    #[test]
    fn frame_stats_use_nearest_rank_percentiles() {
        // 1..=100, deliberately shuffled.
        let mut samples: Vec<f64> = (1..=100).map(f64::from).collect();
        samples.swap(0, 99);
        samples.swap(10, 40);
        let stats = FrameStats::from_samples(&samples).expect("100 samples");

        assert_eq!(stats.samples, 100);
        assert_eq!(stats.min_millis, 1.0);
        assert_eq!(stats.max_millis, 100.0);
        // nearest-rank: ceil(0.50*100)=50, ceil(0.95*100)=95, ceil(0.99*100)=99
        assert_eq!(stats.p50_millis, 50.0);
        assert_eq!(stats.p95_millis, 95.0);
        assert_eq!(stats.p99_millis, 99.0);
        assert!((stats.mean_millis - 50.5).abs() < 1e-9);
    }

    #[test]
    fn frame_stats_single_sample_collapses_to_that_value() {
        let stats = FrameStats::from_samples(&[7.5]).expect("one sample");
        assert_eq!(stats.samples, 1);
        for v in [
            stats.min_millis,
            stats.p50_millis,
            stats.p95_millis,
            stats.p99_millis,
            stats.max_millis,
            stats.mean_millis,
        ] {
            assert_eq!(v, 7.5);
        }
    }

    #[test]
    fn a_zero_edge_retrace_box_has_no_volume() {
        let (lo, hi) = centered_cell_box(0);
        assert_eq!(lo, hi);
        assert_eq!((hi.x - lo.x) * (hi.y - lo.y) * (hi.z - lo.z), 0);
    }

    #[test]
    fn a_retrace_box_is_centred_and_clamped_to_the_cache() {
        let dim = LIGHT_VOLUME_DIM;
        let (lo, hi) = centered_cell_box(24);
        assert_eq!(hi - lo, glam::UVec3::splat(24));
        let centre = (lo + hi) / 2;
        assert_eq!(centre, glam::UVec3::splat(dim / 2));

        // An over-large edge is clamped to the cache bounds.
        let (lo, hi) = centered_cell_box(dim * 4);
        assert_eq!(lo, glam::UVec3::ZERO);
        assert_eq!(hi, glam::UVec3::splat(dim));
    }

    #[test]
    fn a_steady_trace_has_zero_flicker() {
        let flat = [12.0_f32; 40];
        assert_eq!(flicker_index(&flat), 0.0);
        assert_eq!(max_step_fraction(&flat), 0.0);
        assert_eq!(flicker_index(&[7.0]), 0.0);
    }

    #[test]
    fn flicker_index_is_mean_abs_step_over_mean_level() {
        // Alternates 10, 12, 10, 12, ... : every step is 2, mean level is 11.
        let saw: Vec<f32> = (0..20)
            .map(|i| if i % 2 == 0 { 10.0 } else { 12.0 })
            .collect();
        assert!((flicker_index(&saw) - 2.0 / 11.0).abs() < 1e-6);
        // One 5.0 spike dominates the max step (mean stays ~10.25).
        let mut spike = vec![10.0_f32; 20];
        spike[10] = 15.0;
        let mean = spike.iter().sum::<f32>() / 20.0;
        assert!((max_step_fraction(&spike) - 5.0 / mean).abs() < 1e-6);
    }

    #[test]
    fn settle_index_finds_the_first_lasting_return_to_target() {
        // Disturbed to ~28 for a while, then back to ~22 and stays there.
        let mut s = vec![22.0_f32; 60];
        for v in s.iter_mut().take(30).skip(10) {
            *v = 28.0;
        }
        // 5% of 22 is 1.1; the trace is within tolerance from frame 30 on.
        assert_eq!(settle_index(&s, 22.0, 0.05, 0), Some(30));
        // The final frame past tolerance means it never settles.
        s[59] = 25.0;
        assert_eq!(settle_index(&s, 22.0, 0.05, 0), None);
        // A zero target is undefined.
        assert_eq!(settle_index(&s, 0.0, 0.05, 0), None);
    }
}
