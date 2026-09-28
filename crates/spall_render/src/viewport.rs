//! A persistent, host-driven viewport renderer.
//!
//! [`capture_scene`](crate::capture::capture_scene) rebuilds every pipeline and
//! buffer per call, which suits acceptance captures but not an interactive
//! tool. [`ViewportRenderer`] keeps the T12 shadow / HDR opaque / tone-map
//! pipeline, the material table and the resident mesh buffers alive between
//! frames, and renders into an [`OffscreenTarget`] whose colour texture a host
//! UI can sample. The host owns presentation; nothing here depends on a window.

use crate::camera::Camera;
use crate::context::{RenderContext, RenderError};
use crate::environment::Environment;
use crate::indirect::{IndirectResources, LightingVolume};
use crate::pipeline::{CASCADE_COUNT, DebugView, ScenePipeline};
use crate::scene::Material;
use crate::sky_visibility::SkyVisibility;
use crate::target::OffscreenTarget;
use crate::upload::{GpuMesh, UploadBudget};
use crate::vertex::GpuVertex;

/// Per-chunk upload ceiling. Chunks keep every buffer below the adapter's
/// maximum buffer size while letting a large scene stay resident.
const CHUNK_BUDGET: UploadBudget = UploadBudget {
    max_vertex_bytes: 128 << 20,
    max_index_bytes: 64 << 20,
};

/// One resident batch of triangles, already in world space.
#[derive(Debug, Clone, Copy)]
pub struct MeshChunk<'a> {
    pub vertices: &'a [GpuVertex],
    pub indices: &'a [u32],
}

/// Per-frame inputs that change without touching resident geometry.
#[derive(Debug, Clone, Copy)]
pub struct ViewportFrame {
    pub camera: Camera,
    /// Lighting, exposure and background; the clear colour is derived from it
    /// so the displayed background is exact after tone mapping.
    pub environment: Environment,
}

pub struct ViewportRenderer {
    pipeline: ScenePipeline,
    materials: wgpu::Buffer,
    indirect: IndirectResources,
    sky: Option<SkyVisibility>,
    target: OffscreenTarget,
    target_generation: u64,
    chunks: Vec<GpuMesh>,
    max_dimension: u32,
}

impl ViewportRenderer {
    pub fn new(ctx: &RenderContext, materials: &[Material]) -> Self {
        let pipeline = ScenePipeline::new(&ctx.device);
        let materials = pipeline.material_buffer(&ctx.device, materials);
        let indirect = pipeline.indirect_resources(&ctx.device, &ctx.queue, None, &materials);
        Self {
            pipeline,
            materials,
            indirect,
            sky: None,
            target: OffscreenTarget::new(&ctx.device, 16, 16),
            target_generation: 0,
            chunks: Vec::new(),
            max_dimension: ctx.device.limits().max_texture_dimension_2d,
        }
    }

    /// Replace the material table. Vertices index it by `GpuVertex::material`.
    pub fn set_materials(&mut self, ctx: &RenderContext, materials: &[Material]) {
        self.materials = self.pipeline.material_buffer(&ctx.device, materials);
        self.indirect =
            self.pipeline
                .indirect_resources(&ctx.device, &ctx.queue, None, &self.materials);
        if let Some(sky) = &self.sky {
            sky.rebind_materials(&ctx.device, self.pipeline.sky_pipeline(), &self.materials);
        }
    }

    /// Supply (or clear) the camera-local occupancy that makes skylight
    /// visibility-aware; see [`crate::GameRenderer::set_sky_occupancy`]. The
    /// visibility recompute is recorded by the next [`Self::render`].
    pub fn set_sky_occupancy(&mut self, ctx: &RenderContext, occupancy: Option<&LightingVolume>) {
        match occupancy {
            Some(volume) => {
                let sky = self.sky.get_or_insert_with(|| {
                    self.pipeline
                        .create_sky_visibility(&ctx.device, &self.materials)
                });
                sky.set_occupancy(&ctx.queue, volume);
            }
            None => self.sky = None,
        }
    }

    /// Replace the resident geometry. Empty chunks are skipped. On error the
    /// previous geometry stays resident.
    pub fn set_meshes(
        &mut self,
        ctx: &RenderContext,
        chunks: &[MeshChunk<'_>],
    ) -> Result<(), RenderError> {
        let mut next = Vec::with_capacity(chunks.len());
        for chunk in chunks.iter().filter(|chunk| !chunk.indices.is_empty()) {
            next.push(GpuMesh::create(
                &ctx.device,
                chunk.vertices,
                chunk.indices,
                CHUNK_BUDGET,
            )?);
        }
        self.chunks = next;
        Ok(())
    }

    /// Resize the render target to `width` x `height` pixels, clamped to the
    /// adapter limit. Returns true when the texture was recreated, in which
    /// case a host that registered [`Self::color_view`] must re-register it.
    pub fn resize(&mut self, ctx: &RenderContext, width: u32, height: u32) -> bool {
        let width = width.clamp(1, self.max_dimension);
        let height = height.clamp(1, self.max_dimension);
        if (width, height) == (self.target.width, self.target.height) {
            return false;
        }
        self.target = OffscreenTarget::new(&ctx.device, width, height);
        self.target_generation += 1;
        true
    }

    pub fn size(&self) -> (u32, u32) {
        (self.target.width, self.target.height)
    }

    /// Bumps every time [`Self::resize`] recreates the colour texture.
    pub fn target_generation(&self) -> u64 {
        self.target_generation
    }

    /// The tone-mapped `Rgba8UnormSrgb` result, sampleable by a host UI.
    pub fn color_view(&self) -> &wgpu::TextureView {
        self.target.color_view()
    }

    /// The offscreen target, for readback in tests and captures.
    pub fn target(&self) -> &OffscreenTarget {
        &self.target
    }

    /// The last rendered frame as tightly packed RGBA8 (blocks until the GPU
    /// has finished). For captures and tests; a host UI samples
    /// [`Self::color_view`] instead.
    pub fn read_pixels(&self, ctx: &RenderContext) -> Result<Vec<u8>, RenderError> {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-viewport-readback"),
            });
        self.target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        self.target.read_rgba(ctx)
    }

    pub fn triangle_count(&self) -> u64 {
        self.chunks
            .iter()
            .map(|chunk| u64::from(chunk.index_count) / 3)
            .sum()
    }

    /// Render the resident geometry: cascaded sun shadows, HDR opaque shading,
    /// then tone mapping into the colour target. Submits its own work.
    pub fn render(&self, ctx: &RenderContext, frame: &ViewportFrame) {
        let mut camera = frame.camera;
        camera.aspect = self.target.width as f32 / self.target.height as f32;
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-viewport-encoder"),
            });

        if !self.chunks.is_empty() {
            let (light_matrices, _) =
                ScenePipeline::cascade_data(&camera, frame.environment.sun_dir);
            debug_assert_eq!(light_matrices.len(), CASCADE_COUNT);
            for (cascade, matrix) in light_matrices.into_iter().enumerate() {
                let bind =
                    self.pipeline
                        .shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("spall-viewport-shadow-pass"),
                    color_attachments: &[],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: self.pipeline.shadow_layer(cascade),
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Clear(1.0),
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(self.pipeline.shadow());
                pass.set_bind_group(0, &bind, &[]);
                self.draw(&mut pass);
            }
        }

        if let Some(sky) = &self.sky {
            sky.set_environment(&ctx.queue, &frame.environment);
            sky.dispatch(
                self.pipeline.sky_pipeline(),
                &ctx.queue,
                &mut encoder,
                None,
                None,
            );
        }

        let scene_bind = self.pipeline.scene_bind_group_lit(
            &ctx.device,
            &ctx.queue,
            &camera,
            DebugView::Shaded,
            &frame.environment,
            &self.materials,
        );
        let clear = frame.environment.hdr_clear();
        let tone_bind = self.pipeline.tone_bind_group(
            &ctx.device,
            &ctx.queue,
            self.target.hdr_view(),
            frame.environment.exposure,
            false,
        );
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-viewport-opaque-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.target.hdr_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0],
                            g: clear[1],
                            b: clear[2],
                            a: clear[3],
                        }),
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
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
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
            self.draw(&mut pass);
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-viewport-tone-map-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.target.color_view(),
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(self.pipeline.tone_map());
            pass.set_bind_group(0, &tone_bind, &[]);
            pass.draw(0..3, 0..1);
        }
        ctx.queue.submit([encoder.finish()]);
    }

    fn draw<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        for chunk in &self.chunks {
            pass.set_vertex_buffer(0, chunk.vertex_buffer.slice(..));
            pass.set_index_buffer(chunk.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..chunk.index_count, 0, 0..1);
        }
    }
}
