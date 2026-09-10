//! Native render-window host and the client-side replica. GPU voxel extraction
//! and transport wiring are added by later tasks.

pub mod net;
pub mod predict;
pub mod replica;
pub mod residency;

pub use net::{
    BaselineScene, ClientNetConfig, ClientNetError, ClientResidencyLimits, ClientSummary,
    MovementStep, ScriptTarget, ScriptedAction, cut_request, run_replication_client,
};
pub use predict::{ClientPhysics, PlayerMovementSummary, PredictedPlayer};
pub use replica::{ApplyOutcome, MotionTrack, ReplicaConfig, ReplicaWorld};
pub use residency::{ClientResidency, ClientResidencyPass};

use spall_core::{JsonlError, JsonlLog, ProcessEvent, ProcessRecord, ProcessRole};
use std::{path::PathBuf, sync::Arc};
use thiserror::Error;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub log_json: PathBuf,
    /// A bounded graphical run used only by capability smoke tests.
    pub max_frames: Option<u64>,
    /// Runs an actual winit resize before bounded shutdown for the T00 window smoke.
    pub scripted_resize: bool,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Log(#[from] JsonlError),
    #[error("window event loop failed: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("GPU initialization failed: {0}")]
    Gpu(String),
    #[error("rendering failed: {0}")]
    Render(String),
}

pub fn run_window(config: ClientConfig) -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = ClientApp::new(config)?;
    event_loop.run_app(&mut app)?;
    app.result
}

struct ClientApp {
    config: ClientConfig,
    log: JsonlLog,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    frames: u64,
    scripted_resize_observed: bool,
    result: Result<(), ClientError>,
}

impl ClientApp {
    fn new(config: ClientConfig) -> Result<Self, ClientError> {
        let mut log = JsonlLog::create(&config.log_json)?;
        log.write(&ProcessRecord::new(
            ProcessEvent::Started,
            ProcessRole::Client,
            Some("offline_render_host=true; transport_unimplemented=T09".into()),
        ))?;
        Ok(Self {
            config,
            log,
            window: None,
            renderer: None,
            frames: 0,
            scripted_resize_observed: false,
            result: Ok(()),
        })
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        let _ = self.log.write(&ProcessRecord::new(
            ProcessEvent::Failed,
            ProcessRole::Client,
            Some(error.to_string()),
        ));
        self.result = Err(error);
        event_loop.exit();
    }

    fn stop(&mut self, event_loop: &ActiveEventLoop, detail: &str) {
        let detail = format!(
            "{detail}; presented_frames={}; scripted_resize_observed={}",
            self.frames, self.scripted_resize_observed
        );
        if let Err(error) = self.log.write(&ProcessRecord::new(
            ProcessEvent::Stopped,
            ProcessRole::Client,
            Some(detail),
        )) {
            self.result = Err(ClientError::Log(error));
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for ClientApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall sandbox")
                .with_inner_size(PhysicalSize::new(1280, 720)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, ClientError::Gpu(error.to_string())),
        };
        match Renderer::new(window.clone()) {
            Ok(renderer) => {
                if let Err(error) = self.log.write(&ProcessRecord::new(
                    ProcessEvent::Ready,
                    ProcessRole::Client,
                    Some(format!(
                        "adapter={}; backend={:?}",
                        renderer.adapter.get_info().name,
                        renderer.adapter.get_info().backend
                    )),
                )) {
                    return self.fail(event_loop, ClientError::Log(error));
                }
                self.window = Some(window);
                self.renderer = Some(renderer);
                if self.config.scripted_resize {
                    let _ = self
                        .window
                        .as_ref()
                        .expect("window was set")
                        .request_inner_size(PhysicalSize::new(960, 540));
                }
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.stop(event_loop, "window_closed"),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                if self.config.scripted_resize && size.width == 960 && size.height == 540 {
                    self.scripted_resize_observed = true;
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                let presented = match renderer.render() {
                    Ok(presented) => presented,
                    Err(error) => {
                        self.fail(event_loop, error);
                        return;
                    }
                };
                if presented {
                    self.frames += 1;
                }
                if self
                    .config
                    .max_frames
                    .is_some_and(|limit| self.frames >= limit)
                    && (!self.config.scripted_resize || self.scripted_resize_observed)
                {
                    self.stop(event_loop, "bounded_frame_run_complete");
                } else if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

struct Renderer {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
}

impl Renderer {
    fn new(window: Arc<Window>) -> Result<Self, ClientError> {
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .ok_or_else(|| ClientError::Gpu("no compatible GPU adapter".into()))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("spall-client-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .map_err(|error| ClientError::Gpu(error.to_string()))?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| ClientError::Gpu("surface exposes no texture format".into()))?;
        let size = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);
        Ok(Self {
            adapter,
            device,
            queue,
            surface,
            surface_config,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.surface_config.width = size.width;
        self.surface_config.height = size.height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    fn render(&mut self) -> Result<bool, ClientError> {
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.surface_config);
                return Ok(false);
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(false),
            Err(error) => return Err(ClientError::Render(error.to_string())),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spall-clear-encoder"),
            });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-clear-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.025,
                            g: 0.04,
                            b: 0.08,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
        }
        self.queue.submit([encoder.finish()]);
        frame.present();
        Ok(true)
    }
}
