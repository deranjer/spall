//! Layer-0 baseline for the ENG-69 movement-jitter investigation: a single
//! process, no network, no server, no chunk streaming/meshing, no
//! rapier/spall_physics. Just a hardcoded block grid, a hand-rolled AABB
//! player, and a fresh minimal wgpu renderer. WASD to move, mouse to look,
//! Space to jump, Escape to release the cursor, click to recapture it.
//!
//! The plan is to layer real systems back on top of this one at a time
//! (chunked meshing, then real physics, then networking) until the jitter
//! reappears, which tells us which layer actually causes it.

mod physics;
mod render;
mod world;

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::sync::Arc;
use std::time::Instant;

use glam::Vec3;
use render::{Instance, Renderer};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use physics::Player;
use world::World;

const MOUSE_SENSITIVITY: f32 = 0.0025;
const MAX_PITCH: f32 = 1.5;

#[derive(Default)]
struct HeldKeys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    jump: bool,
}

impl HeldKeys {
    /// `[strafe_right, forward]`.
    fn wish_local(&self) -> [f32; 2] {
        let mut x = 0.0f32;
        if self.right {
            x += 1.0;
        }
        if self.left {
            x -= 1.0;
        }
        let mut z = 0.0f32;
        if self.forward {
            z += 1.0;
        }
        if self.back {
            z -= 1.0;
        }
        [x, z]
    }
}

fn view_dir_from(yaw: f32, pitch: f32) -> Vec3 {
    let (sin_y, cos_y) = yaw.sin_cos();
    let (sin_p, cos_p) = pitch.sin_cos();
    Vec3::new(sin_y * cos_p, sin_p, -cos_y * cos_p)
}

/// Per-frame JSONL log at `.local/runs/poc-local-frames.jsonl` — raw
/// dt/render-time/position numbers to look at directly instead of relying
/// on "felt" impressions, same idea as the real engine's
/// `interactive-frames.jsonl`. Best-effort: if the file can't be created,
/// logging is just silently disabled rather than failing the run.
struct FrameLog {
    writer: Option<BufWriter<File>>,
    start: Instant,
}

impl FrameLog {
    fn open() -> Self {
        let writer = std::fs::create_dir_all(".local/runs")
            .and_then(|()| File::create(".local/runs/poc-local-frames.jsonl"))
            .map(BufWriter::new)
            .ok();
        Self {
            writer,
            start: Instant::now(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        dt_ms: f32,
        frame_ms: f32,
        position: [f32; 3],
        velocity: [f32; 3],
        yaw: f32,
        wish_local: [f32; 2],
        mouse_events_since_last: u32,
        mouse_delta_sum: [f32; 2],
        grounded: bool,
    ) {
        let Some(writer) = &mut self.writer else {
            return;
        };
        let _ = writeln!(
            writer,
            "{{\"wall_ms\":{:.3},\"dt_ms\":{:.3},\"frame_ms\":{:.3},\"pos\":[{:.4},{:.4},{:.4}],\"vel\":[{:.4},{:.4},{:.4}],\"yaw\":{:.6},\"wish_local\":[{:.2},{:.2}],\"mouse_events\":{},\"mouse_delta_sum\":[{:.3},{:.3}],\"grounded\":{}}}",
            self.start.elapsed().as_secs_f64() * 1000.0,
            dt_ms,
            frame_ms,
            position[0],
            position[1],
            position[2],
            velocity[0],
            velocity[1],
            velocity[2],
            yaw,
            wish_local[0],
            wish_local[1],
            mouse_events_since_last,
            mouse_delta_sum[0],
            mouse_delta_sum[1],
            grounded,
        );
    }
}

struct App {
    world: World,
    player: Player,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    held: HeldKeys,
    yaw: f32,
    pitch: f32,
    cursor_locked: bool,
    last_frame_at: Option<Instant>,
    fps_ema: f32,
    /// Worst raw frame-to-frame delta and worst total `RedrawRequested` cost
    /// since the last title update — an average hides exactly the kind of
    /// occasional stall/burst that reads as jitter (see the real engine's
    /// `Hud::max_frame_ms` finding, ENG-69 round 12).
    max_dt_ms: f32,
    max_frame_ms: f32,
    last_title_update: Option<Instant>,
    frame_log: FrameLog,
    /// Raw mouse-motion events (and their summed delta) received since the
    /// last `RedrawRequested` — lets the frame log show exactly how many
    /// device events got coalesced into one rendered frame, and whether
    /// their sum is consistent with the observed yaw change.
    mouse_events_since_last: u32,
    mouse_delta_sum: [f32; 2],
    result: Result<(), String>,
}

impl App {
    fn new() -> Self {
        let world = World::generate();
        let spawn = world.spawn_point();
        Self {
            world,
            player: Player::new(spawn),
            window: None,
            renderer: None,
            held: HeldKeys::default(),
            yaw: 0.0,
            pitch: 0.0,
            cursor_locked: false,
            last_frame_at: None,
            fps_ema: 0.0,
            max_dt_ms: 0.0,
            max_frame_ms: 0.0,
            last_title_update: None,
            frame_log: FrameLog::open(),
            mouse_events_since_last: 0,
            mouse_delta_sum: [0.0, 0.0],
            result: Ok(()),
        }
    }

    fn set_cursor_locked(&mut self, locked: bool) {
        let Some(window) = &self.window else { return };
        if locked {
            let grabbed = window
                .set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
                .is_ok();
            window.set_cursor_visible(!grabbed);
            self.cursor_locked = grabbed;
        } else {
            let _ = window.set_cursor_grab(CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.cursor_locked = false;
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.result = Err(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("poc-local")
                .with_inner_size(PhysicalSize::new(1280, 720)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, error.to_string()),
        };
        let instances: Vec<Instance> = self
            .world
            .iter_blocks()
            .map(|(offset, color)| Instance { offset, color })
            .collect();
        match Renderer::new(window.clone(), &instances) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.set_cursor_locked(true);
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !self.cursor_locked => {
                self.set_cursor_locked(true);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let held = event.state == ElementState::Pressed;
                match code {
                    KeyCode::KeyW => self.held.forward = held,
                    KeyCode::KeyS => self.held.back = held,
                    KeyCode::KeyA => self.held.left = held,
                    KeyCode::KeyD => self.held.right = held,
                    KeyCode::Space => self.held.jump = held,
                    KeyCode::Escape if held && !event.repeat => self.set_cursor_locked(false),
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => {
                let frame_start = Instant::now();
                let now = frame_start;
                let raw_dt_ms = self
                    .last_frame_at
                    .map(|prev| (now - prev).as_secs_f32() * 1000.0)
                    .unwrap_or(0.0);
                let dt = (raw_dt_ms / 1000.0).min(0.05); // clamp so a stall (e.g. window drag) doesn't fling the player
                self.last_frame_at = Some(now);
                self.max_dt_ms = self.max_dt_ms.max(raw_dt_ms);
                if dt > 0.0 {
                    self.fps_ema = if self.fps_ema == 0.0 {
                        1.0 / dt
                    } else {
                        self.fps_ema * 0.9 + (1.0 / dt) * 0.1
                    };
                }

                let wish_local = self.held.wish_local();
                self.player.step(
                    &self.world,
                    dt,
                    wish_local,
                    self.yaw,
                    self.held.jump,
                    self.world.spawn_point(),
                );

                let eye = Vec3::new(
                    self.player.position[0],
                    self.player.position[1] + physics::EYE_HEIGHT,
                    self.player.position[2],
                );
                let look_dir = view_dir_from(self.yaw, self.pitch);

                if let Some(renderer) = &mut self.renderer
                    && let Err(error) = renderer.render(eye, look_dir)
                {
                    self.fail(event_loop, error);
                    return;
                }

                let frame_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
                self.max_frame_ms = self.max_frame_ms.max(frame_ms);
                self.frame_log.record(
                    raw_dt_ms,
                    frame_ms,
                    self.player.position,
                    self.player.velocity,
                    self.yaw,
                    wish_local,
                    self.mouse_events_since_last,
                    self.mouse_delta_sum,
                    self.player.grounded,
                );
                self.mouse_events_since_last = 0;
                self.mouse_delta_sum = [0.0, 0.0];

                if self
                    .last_title_update
                    .is_none_or(|t| now - t >= std::time::Duration::from_millis(250))
                {
                    self.last_title_update = Some(now);
                    if let Some(window) = &self.window {
                        window.set_title(&format!(
                            "poc-local | {:.0} fps (avg) | max dt {:.1} ms | max frame {:.1} ms | pos ({:.2}, {:.2}, {:.2}) | grounded {}",
                            self.fps_ema,
                            self.max_dt_ms,
                            self.max_frame_ms,
                            self.player.position[0],
                            self.player.position[1],
                            self.player.position[2],
                            self.player.grounded,
                        ));
                    }
                    self.max_dt_ms = 0.0;
                    self.max_frame_ms = 0.0;
                }

                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event
            && self.cursor_locked
        {
            self.yaw += delta.0 as f32 * MOUSE_SENSITIVITY;
            self.pitch =
                (self.pitch - delta.1 as f32 * MOUSE_SENSITIVITY).clamp(-MAX_PITCH, MAX_PITCH);
            self.mouse_events_since_last += 1;
            self.mouse_delta_sum[0] += delta.0 as f32;
            self.mouse_delta_sum[1] += delta.1 as f32;
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::new();
    event_loop.run_app(&mut app)?;
    app.result.map_err(Into::into)
}
