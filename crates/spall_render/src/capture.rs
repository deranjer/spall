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
    pub gpu_millis: f64,
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

    let started = Instant::now();
    let mut images = Vec::new();
    for &view in &opts.views {
        let bind_group =
            pipeline.frame_bind_group(&ctx.device, &ctx.queue, &scene.camera, view, &palette);

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-capture-encoder"),
            });
        {
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
                timestamp_writes: None,
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

        let rgba = target.read_rgba(ctx)?;
        let image: ImageBuffer<Rgba<u8>, _> =
            ImageBuffer::from_raw(target.width, target.height, rgba)
                .expect("readback buffer is exactly width*height*4");
        let path = out_dir.join(format!("{}.png", view.stem()));
        image.save(&path).map_err(|source| RenderError::Image {
            path: path.display().to_string(),
            source,
        })?;
        images.push(CaptureImage { view, path });
    }
    ctx.wait();
    let gpu_millis = started.elapsed().as_secs_f64() * 1000.0;

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
        gpu_millis,
    })
}
