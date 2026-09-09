//! Bounded offscreen capture for the explicit T12 shadow/HDR/tone-map passes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use image::{ImageBuffer, Rgba};

use crate::context::{RenderContext, RenderError};
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

    let pair_count = CASCADE_COUNT as u32 + opts.views.len() as u32 * 2;
    let mut gpu_timer = GpuTimer::new(ctx, pair_count);
    let loop_start = Instant::now();

    // Shadows are camera-dependent but view-independent, so render them once.
    let (light_matrices, _) = ScenePipeline::cascade_data(&scene.camera, default_sun_dir());
    let mut shadow_encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("spall-shadow-encoder"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let timestamp_writes = gpu_timer.as_ref().map(|timer| timer.writes(cascade as u32));
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
        let base_query = CASCADE_COUNT as u32 + view_index as u32 * 2;
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
            let shadow_millis = times[..CASCADE_COUNT].iter().sum();
            let opaque_millis = times[CASCADE_COUNT..].iter().step_by(2).sum();
            let tone_map_millis = times[CASCADE_COUNT + 1..].iter().step_by(2).sum();
            PassTiming {
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
        timing: CaptureTiming {
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
