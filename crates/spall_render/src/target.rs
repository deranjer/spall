//! An offscreen colour + depth target and RGBA readback.

use crate::context::{RenderContext, RenderError};
use crate::pipeline::{COLOR_FORMAT, DEPTH_FORMAT, HDR_FORMAT};

const ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// A colour texture (with `COPY_SRC`) and a matching depth texture.
pub struct OffscreenTarget {
    pub width: u32,
    pub height: u32,
    color: wgpu::Texture,
    color_view: wgpu::TextureView,
    _hdr: wgpu::Texture,
    hdr_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    readback: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl OffscreenTarget {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-capture-color"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let hdr = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-capture-hdr"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HDR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spall-capture-depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        let unpadded_bytes_per_row = width * 4;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(ROW_ALIGN) * ROW_ALIGN;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spall-capture-readback"),
            size: u64::from(padded_bytes_per_row) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            width,
            height,
            color_view: color.create_view(&wgpu::TextureViewDescriptor::default()),
            color,
            hdr_view: hdr.create_view(&wgpu::TextureViewDescriptor::default()),
            _hdr: hdr,
            depth_view: depth.create_view(&wgpu::TextureViewDescriptor::default()),
            readback,
            padded_bytes_per_row,
        }
    }

    pub fn color_view(&self) -> &wgpu::TextureView {
        &self.color_view
    }

    pub fn hdr_view(&self) -> &wgpu::TextureView {
        &self.hdr_view
    }

    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    /// Queue a copy of the colour texture into the readback buffer.
    pub fn copy_to_readback(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.color,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Map the readback buffer and return tightly packed RGBA8 rows. Must be
    /// called after the copy has been submitted and the device polled to
    /// completion.
    pub fn read_rgba(&self, ctx: &RenderContext) -> Result<Vec<u8>, RenderError> {
        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        ctx.wait();
        rx.recv()
            .map_err(|_| RenderError::Readback)?
            .map_err(|_| RenderError::Readback)?;

        let padded = self.padded_bytes_per_row as usize;
        let unpadded = (self.width * 4) as usize;
        let mut out = Vec::with_capacity(unpadded * self.height as usize);
        {
            let mapped = slice.get_mapped_range();
            for row in mapped.chunks_exact(padded) {
                out.extend_from_slice(&row[..unpadded]);
            }
        }
        self.readback.unmap();
        Ok(out)
    }
}
