//! GPU device acquisition, decoupled from any window or surface.

use thiserror::Error;

/// A headless wgpu device and queue.
pub struct RenderContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    adapter_info: wgpu::AdapterInfo,
}

/// Why the renderer could not start or run.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("no compatible GPU adapter is available")]
    NoAdapter,
    #[error("GPU device request failed: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("GPU work failed: {0}")]
    Gpu(String),
    #[error("mesh upload of {bytes} B exceeds the {budget} B budget")]
    UploadBudgetExceeded { bytes: u64, budget: u64 },
    #[error("readback buffer mapping failed")]
    Readback,
    #[error("writing {path}: {source}")]
    Image {
        path: String,
        #[source]
        source: image::ImageError,
    },
}

impl RenderContext {
    /// Acquire a headless device. Returns [`RenderError::NoAdapter`] when the
    /// host has no usable GPU — the caller maps that to the "missing
    /// environment capability" exit code.
    pub fn headless() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok_or(RenderError::NoAdapter)?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("spall-render-headless"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;

        Ok(Self {
            device,
            queue,
            adapter_info: adapter.get_info(),
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_info.name
    }

    pub fn backend(&self) -> wgpu::Backend {
        self.adapter_info.backend
    }

    /// Block until every submitted GPU command has completed.
    pub fn wait(&self) {
        self.device.poll(wgpu::Maintain::Wait);
    }
}
