//! GPU device acquisition, decoupled from any window or surface.

use thiserror::Error;

/// A headless wgpu device and queue.
pub struct RenderContext {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    adapter_info: wgpu::AdapterInfo,
    timestamps: bool,
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

        // Opt into render-pass timestamp queries when (and only when) the adapter
        // reports support. Where they are unavailable the capture path reports
        // GPU timing as unavailable rather than substituting a CPU figure.
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let required_features = if timestamps {
            wgpu::Features::TIMESTAMP_QUERY
        } else {
            wgpu::Features::empty()
        };

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("spall-render-headless"),
                required_features,
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))?;

        Ok(Self {
            device,
            queue,
            adapter_info: adapter.get_info(),
            timestamps,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_info.name
    }

    pub fn backend(&self) -> wgpu::Backend {
        self.adapter_info.backend
    }

    /// Whether this device was created with render-pass timestamp queries. When
    /// `false`, callers must report GPU timing as unavailable.
    pub fn supports_gpu_timestamps(&self) -> bool {
        self.timestamps
    }

    /// Nanoseconds per timestamp-query tick for this queue. Only meaningful when
    /// [`RenderContext::supports_gpu_timestamps`] is `true`.
    pub fn timestamp_period_ns(&self) -> f32 {
        self.queue.get_timestamp_period()
    }

    /// Block until every submitted GPU command has completed.
    pub fn wait(&self) {
        self.device.poll(wgpu::Maintain::Wait);
    }
}
