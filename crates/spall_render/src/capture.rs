//! Offscreen capture: render a [`Scene`] to PNG images (shaded plus normal and
//! depth debug views) and return a report. The function renders, writes, and
//! returns — nothing keeps running.

use std::path::{Path, PathBuf};
use std::time::Instant;

use image::{ImageBuffer, Rgba};

use crate::context::{RenderContext, RenderError};
use crate::pipeline::{DebugView, ScenePipeline};
use crate::scene::Scene;
use crate::target::OffscreenTarget;
use crate::upload::{GpuMesh, UploadBudget};
use crate::vertex::to_gpu;

/// Capture settings.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub width: u32,
    pub height: u32,
    /// Which images to write. Defaults to all three.
    pub views: Vec<DebugView>,
    pub budget: UploadBudget,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            views: vec![DebugView::Shaded, DebugView::Normals, DebugView::Depth],
            budget: UploadBudget::default(),
        }
    }
}

/// One written image.
#[derive(Debug, Clone)]
pub struct CaptureImage {
    pub view: DebugView,
    pub path: PathBuf,
}

/// Timings for one [`capture_scene`] call.
///
/// GPU and CPU costs are reported separately and must never be conflated. In
/// particular `cpu_*` figures include the synchronous readback map wait and the
/// PNG compression, neither of which is GPU work; `gpu_render_millis` is a real
/// device measurement from timestamp queries or [`None`] when the adapter does
/// not support them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureTiming {
    /// Wall-clock time for the whole render → readback → PNG-encode loop,
    /// measured on the CPU. This is *not* a GPU timing.
    pub cpu_total_millis: f64,
    /// CPU wall-clock spent inside `map_async` readback + row unpadding, summed
    /// over every captured view.
    pub cpu_readback_millis: f64,
    /// CPU wall-clock spent in PNG compression and the file write, summed over
    /// every captured view.
    pub cpu_encode_millis: f64,
    /// GPU time spent inside the render passes, measured with timestamp queries
    /// and summed over every captured view. [`None`] when the adapter/driver
    /// does not support render-pass timestamp queries — callers must surface it
    /// as "GPU timing unavailable" and never substitute a CPU figure.
    pub gpu_render_millis: Option<f64>,
}

/// Result of [`capture_scene`].
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
    /// Separated CPU/GPU timings. See [`CaptureTiming`].
    pub timing: CaptureTiming,
}

/// Render-pass timestamp queries for the capture loop. One begin/end pair per
/// captured view; resolved and read back once at the end.
struct GpuTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    period_ns: f64,
    pairs: u32,
}

impl GpuTimer {
    /// Allocate a timer for `pairs` render passes, or `None` when the device has
    /// no timestamp-query support.
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

    /// Timestamp writes for render pass number `index` (`0`-based).
    fn writes(&self, index: u32) -> wgpu::RenderPassTimestampWrites<'_> {
        wgpu::RenderPassTimestampWrites {
            query_set: &self.set,
            beginning_of_pass_write_index: Some(index * 2),
            end_of_pass_write_index: Some(index * 2 + 1),
        }
    }

    /// Resolve and read the queries. Returns the summed GPU pass time in
    /// milliseconds, or `None` if the driver produced no usable delta.
    fn total_millis(self, ctx: &RenderContext) -> Option<f64> {
        let count = self.pairs * 2;
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-capture-timestamp-resolve-encoder"),
            });
        encoder.resolve_query_set(&self.set, 0..count, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.resolve,
            0,
            &self.readback,
            0,
            u64::from(count) * std::mem::size_of::<u64>() as u64,
        );
        ctx.queue.submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        ctx.wait();
        rx.recv().ok()?.ok()?;

        let ticks: Vec<u64> = {
            let mapped = slice.get_mapped_range();
            bytemuck::cast_slice::<u8, u64>(&mapped).to_vec()
        };
        self.readback.unmap();

        let mut total_ns = 0.0f64;
        let mut usable = false;
        for pair in ticks.chunks_exact(2) {
            let delta = pair[1].saturating_sub(pair[0]);
            if delta > 0 {
                usable = true;
                total_ns += delta as f64 * self.period_ns;
            }
        }
        usable.then_some(total_ns / 1.0e6)
    }
}

/// Render `scene` to `<out_dir>/<view>.png` for each requested view.
pub fn capture_scene(
    ctx: &RenderContext,
    scene: &Scene,
    out_dir: &Path,
    opts: &CaptureOptions,
) -> Result<CaptureReport, RenderError> {
    std::fs::create_dir_all(out_dir).map_err(|e| RenderError::Image {
        path: out_dir.display().to_string(),
        source: image::ImageError::IoError(e),
    })?;

    let pipeline = ScenePipeline::new(&ctx.device);
    let palette = pipeline.palette_buffer(&ctx.device, &scene.palette);
    let target = OffscreenTarget::new(&ctx.device, opts.width, opts.height);

    // Frustum-cull, then upload every visible item once.
    let frustum = scene.camera.frustum();
    let mut draws: Vec<GpuMesh> = Vec::new();
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

    let clear = wgpu::Color {
        r: scene.clear[0],
        g: scene.clear[1],
        b: scene.clear[2],
        a: scene.clear[3],
    };

    let mut gpu_timer = GpuTimer::new(ctx, opts.views.len() as u32);

    let loop_start = Instant::now();
    let mut readback_millis = 0.0f64;
    let mut encode_millis = 0.0f64;
    let mut images = Vec::new();
    for (index, &view) in opts.views.iter().enumerate() {
        let bind_group =
            pipeline.frame_bind_group(&ctx.device, &ctx.queue, &scene.camera, view, &palette);

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-capture-encoder"),
            });
        {
            let timestamp_writes = gpu_timer.as_ref().map(|t| t.writes(index as u32));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-capture-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target.color_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
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
            pass.set_pipeline(pipeline.raw());
            pass.set_bind_group(0, &bind_group, &[]);
            for gpu in &draws {
                pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                pass.set_index_buffer(gpu.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..gpu.index_count, 0, 0..1);
            }
        }
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);

        let readback_start = Instant::now();
        let rgba = target.read_rgba(ctx)?;
        let image: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_raw(target.width, target.height, rgba)
                .expect("readback buffer is exactly width*height*4");
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

    let gpu_render_millis = gpu_timer.take().and_then(|t| t.total_millis(ctx));

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
        timing: CaptureTiming {
            cpu_total_millis,
            cpu_readback_millis: readback_millis,
            cpu_encode_millis: encode_millis,
            gpu_render_millis,
        },
    })
}
