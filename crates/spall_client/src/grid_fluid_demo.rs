//! Local viewer for the ENG-103 MAC fixtures.
//!
//! This owns a private copy of the CPU grid and never advances the game/server
//! simulation. Water cubes show stored VOF cell fractions, not reconstructed
//! free-surface geometry.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::Vec3;
use spall_core::GlobalCell;
use spall_fluid::SolidBoundary;
use spall_fluid::grid_mac::{GridReservoirFixture, PressurePreconditioner};
use spall_render::{Camera, CubeInstance, EnvironmentPreset, Material};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::ClientError;
use crate::window::{AcquireOutcome, WorldRenderer};

const FIXED_DT: Duration = Duration::from_nanos(16_666_667);
const MAX_STEPS_PER_FRAME: usize = 2;
const CAMERA_SPEED_M_S: f32 = 3.5;
const MOUSE_SENSITIVITY: f32 = 0.0025;
const MATERIAL_STONE: u32 = 0;
const MATERIAL_DAM: u32 = 1;
const MATERIAL_WATER: u32 = 2;

/// Opens an isolated interactive window for the bounded MAC fixtures.
pub fn run_grid_fluid_demo_window() -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = GridFluidDemoApp::new(Scene::Reservoirs)?;
    event_loop.run_app(&mut app)?;
    app.result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scene {
    Reservoirs,
    Canal,
    Breach,
    Basin,
    Equilibrium,
    Tunnel,
}

impl Scene {
    fn label(self) -> &'static str {
        match self {
            Self::Reservoirs => "Two reservoirs",
            Self::Canal => "Open canal",
            Self::Breach => "Dam breach",
            Self::Basin => "Spreading basin",
            Self::Equilibrium => "Resting basin",
            Self::Tunnel => "Flooded tunnel",
        }
    }

    fn from_key(key: KeyCode) -> Option<Self> {
        match key {
            KeyCode::Digit1 => Some(Self::Reservoirs),
            KeyCode::Digit2 => Some(Self::Canal),
            KeyCode::Digit3 => Some(Self::Breach),
            KeyCode::Digit4 => Some(Self::Basin),
            KeyCode::Digit5 => Some(Self::Tunnel),
            KeyCode::Digit6 => Some(Self::Equilibrium),
            _ => None,
        }
    }
}

struct GridFluidDemoApp {
    window: Option<Arc<Window>>,
    renderer: Option<WorldRenderer>,
    fixture: GridReservoirFixture,
    scene: Scene,
    terrain: Vec<CubeInstance>,
    terrain_dirty: bool,
    held: HashSet<KeyCode>,
    camera: Camera,
    cursor_locked: bool,
    paused: bool,
    canal_open: bool,
    dam_broken: bool,
    accumulator: Duration,
    simulated_time_s: f64,
    last_frame: Instant,
    title_update: Instant,
    result: Result<(), ClientError>,
    message: String,
}

impl GridFluidDemoApp {
    fn new(scene: Scene) -> Result<Self, ClientError> {
        let fixture = make_fixture(scene)?;
        let terrain = build_terrain(&fixture)?;
        let camera = Camera::looking_along(
            Vec3::new(8.0, 5.5, 7.5),
            Vec3::new(3.0, 0.8, 1.0) - Vec3::new(8.0, 5.5, 7.5),
            60.0_f32.to_radians(),
            16.0 / 9.0,
        );
        Ok(Self {
            window: None,
            renderer: None,
            fixture,
            scene,
            terrain,
            terrain_dirty: true,
            held: HashSet::new(),
            camera,
            cursor_locked: false,
            paused: false,
            canal_open: matches!(scene, Scene::Canal),
            dam_broken: matches!(scene, Scene::Breach),
            accumulator: Duration::ZERO,
            simulated_time_s: 0.0,
            last_frame: Instant::now(),
            title_update: Instant::now(),
            result: Ok(()),
            message: scene.label().into(),
        })
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        eprintln!("sandbox-client MAC water viewer: {error}");
        self.result = Err(error);
        event_loop.exit();
    }

    fn cursor(&mut self, locked: bool) {
        let Some(window) = &self.window else { return };
        let result = if locked {
            window
                .set_cursor_grab(CursorGrabMode::Confined)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked))
        } else {
            window.set_cursor_grab(CursorGrabMode::None)
        };
        if result.is_ok() {
            window.set_cursor_visible(!locked);
            self.cursor_locked = locked;
        }
    }

    fn set_scene(&mut self, scene: Scene) {
        match Self::new(scene) {
            Ok(mut fresh) => {
                fresh.window = self.window.clone();
                fresh.renderer = self.renderer.take();
                fresh.last_frame = Instant::now();
                *self = fresh;
            }
            Err(error) => self.message = format!("Could not load {}: {error}", scene.label()),
        }
    }

    fn action(&mut self, key: KeyCode) {
        if let Some(scene) = Scene::from_key(key) {
            self.set_scene(scene);
            return;
        }
        match key {
            KeyCode::Space => {
                self.paused = !self.paused;
                self.message = if self.paused { "Paused" } else { "Running" }.into();
            }
            KeyCode::KeyC if !self.canal_open => match self.fixture.excavate_canal() {
                Ok(()) => {
                    self.canal_open = true;
                    self.message = "Canal opened".into();
                    self.refresh_terrain();
                }
                Err(error) => self.message = format!("Canal edit rejected: {error}"),
            },
            KeyCode::KeyB if !self.dam_broken => match self.fixture.breach_dam() {
                Ok(()) => {
                    self.dam_broken = true;
                    self.message = "Dam breached".into();
                    self.refresh_terrain();
                }
                Err(error) => self.message = format!("Breach rejected: {error}"),
            },
            KeyCode::KeyX if self.canal_open && !self.dam_broken => {
                match self.fixture.close_canal() {
                    Ok(()) => {
                        self.canal_open = false;
                        self.message = "Canal closed".into();
                        self.refresh_terrain();
                    }
                    Err(error) => self.message = format!("Canal closure rejected: {error}"),
                }
            }
            KeyCode::KeyR => self.set_scene(self.scene),
            _ => {}
        }
    }

    fn refresh_terrain(&mut self) {
        match build_terrain(&self.fixture) {
            Ok(instances) => {
                self.terrain = instances;
                self.terrain_dirty = true;
            }
            Err(error) => self.message = format!("Terrain refresh failed: {error}"),
        }
    }

    fn update_simulation(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_frame)
            .min(Duration::from_millis(100));
        self.last_frame = now;
        if !self.paused {
            self.accumulator += elapsed;
            let mut steps = 0;
            while self.accumulator >= FIXED_DT && steps < MAX_STEPS_PER_FRAME {
                match self.fixture.step(FIXED_DT.as_secs_f64()) {
                    Ok(_) => {
                        self.simulated_time_s += FIXED_DT.as_secs_f64();
                        self.accumulator -= FIXED_DT;
                    }
                    Err(error) => {
                        self.paused = true;
                        self.message = format!("Simulation stopped: {error}");
                        self.accumulator = Duration::ZERO;
                        break;
                    }
                }
                steps += 1;
            }
            if steps == MAX_STEPS_PER_FRAME && self.accumulator >= FIXED_DT {
                self.accumulator = Duration::ZERO;
            }
        }

        let mut movement = Vec3::ZERO;
        if self.held.contains(&KeyCode::KeyW) {
            movement.z += 1.0;
        }
        if self.held.contains(&KeyCode::KeyS) {
            movement.z -= 1.0;
        }
        if self.held.contains(&KeyCode::KeyD) {
            movement.x += 1.0;
        }
        if self.held.contains(&KeyCode::KeyA) {
            movement.x -= 1.0;
        }
        if self.held.contains(&KeyCode::KeyE) {
            movement.y += 1.0;
        }
        if self.held.contains(&KeyCode::KeyQ) {
            movement.y -= 1.0;
        }
        if movement.length_squared() > 0.0 {
            self.camera
                .fly(movement.normalize() * CAMERA_SPEED_M_S * elapsed.as_secs_f32());
        }
    }

    fn render(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        self.update_simulation(now);
        let water = build_water(&self.fixture);
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let terrain = self.terrain_dirty.then_some(self.terrain.as_slice());
        let acquired = match renderer.begin_frame(terrain, &[]) {
            Ok(AcquireOutcome::Ready(frame)) => frame,
            Ok(AcquireOutcome::Skipped(_)) => return,
            Err(error) => return self.fail(event_loop, error),
        };
        self.terrain_dirty = false;
        renderer.set_debug_water(&water);

        let status = format!(
            "{} · t={:.2}s · {:.3} m³ retained · {:.3} m³ top outflow · {}",
            if self.paused { "PAUSED" } else { "LIVE" },
            self.simulated_time_s,
            self.fixture.grid().water_volume_m3(),
            self.fixture.grid().cumulative_open_outflow_m3(),
            self.message,
        );
        if self.title_update.elapsed() >= Duration::from_millis(200) {
            if let Some(window) = &self.window {
                window.set_title(&format!("Spall MAC water inspection — {status}"));
            }
            self.title_update = now;
        }
        if let Err(error) = renderer.finish_frame(
            acquired,
            Some(&(self.camera.position, self.camera.forward())),
            &[],
            &[],
            Some((
                "ENG-103 TWO-PHASE WATER INSPECTION",
                "1 reservoirs · 2 canal · 3 breach · 4 basin · 5 tunnel · 6 level basin · C open · B breach · X close · Space pause · R reset · WASD/QE move · mouse look",
                &status,
            )),
            None,
        ) {
            self.fail(event_loop, error);
        }
    }
}

impl ApplicationHandler for GridFluidDemoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        event_loop.set_control_flow(ControlFlow::Poll);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall MAC water inspection — click to capture mouse")
                .with_inner_size(PhysicalSize::new(1440, 900)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, ClientError::Gpu(error.to_string())),
        };
        let materials = [
            Material::new([0.29, 0.24, 0.17], 0.95, 0.0),
            Material::new([0.12, 0.075, 0.03], 0.9, 0.0),
            Material::new([0.015, 0.22, 0.78], 0.18, 0.05).emissive(0.06),
        ];
        match WorldRenderer::new(
            window.clone(),
            EnvironmentPreset::Daylight.environment(),
            &materials,
            false,
        ) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.last_frame = Instant::now();
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
            WindowEvent::Focused(false) => {
                self.held.clear();
                self.cursor(false);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !self.cursor_locked => self.cursor(true),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(key) = event.physical_key else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                if pressed {
                    if !event.repeat {
                        match key {
                            KeyCode::Escape => self.cursor(false),
                            KeyCode::Space
                            | KeyCode::KeyC
                            | KeyCode::KeyB
                            | KeyCode::KeyX
                            | KeyCode::KeyR
                            | KeyCode::Digit1
                            | KeyCode::Digit2
                            | KeyCode::Digit3
                            | KeyCode::Digit4
                            | KeyCode::Digit5
                            | KeyCode::Digit6 => self.action(key),
                            _ => {}
                        }
                    }
                    self.held.insert(key);
                } else {
                    self.held.remove(&key);
                }
            }
            WindowEvent::RedrawRequested => self.render(event_loop, Instant::now()),
            _ => {}
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: winit::event::DeviceId, event: DeviceEvent) {
        if self.cursor_locked
            && let DeviceEvent::MouseMotion { delta } = event
        {
            self.camera.look(
                -(delta.0 as f32) * MOUSE_SENSITIVITY,
                -(delta.1 as f32) * MOUSE_SENSITIVITY,
            );
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

fn make_fixture(scene: Scene) -> Result<GridReservoirFixture, ClientError> {
    let make_base = || {
        GridReservoirFixture::new(1, true)
            .map_err(|error| ClientError::Render(format!("{} fixture: {error}", scene.label())))
    };
    let mut fixture = match scene {
        Scene::Reservoirs => make_base(),
        Scene::Canal => {
            let mut fixture = make_base()?;
            fixture
                .excavate_canal()
                .map_err(|error| ClientError::Render(error.to_string()))?;
            Ok(fixture)
        }
        Scene::Breach => {
            let mut fixture = make_base()?;
            fixture
                .breach_dam()
                .map_err(|error| ClientError::Render(error.to_string()))?;
            Ok(fixture)
        }
        Scene::Basin => GridReservoirFixture::new_basin(1)
            .map_err(|error| ClientError::Render(format!("{} fixture: {error}", scene.label()))),
        Scene::Equilibrium => GridReservoirFixture::new_equilibrium_basin(1, 1)
            .map_err(|error| ClientError::Render(format!("{} fixture: {error}", scene.label()))),
        Scene::Tunnel => GridReservoirFixture::new_tunnel(1)
            .map_err(|error| ClientError::Render(format!("{} fixture: {error}", scene.label()))),
    }?;
    fixture
        .grid_mut()
        .set_ambient_density(1.2)
        .map_err(|error| ClientError::Render(error.to_string()))?;
    fixture
        .grid_mut()
        .set_pressure_preconditioner(PressurePreconditioner::Multigrid);
    Ok(fixture)
}

fn build_terrain(fixture: &GridReservoirFixture) -> Result<Vec<CubeInstance>, ClientError> {
    let spec = fixture.grid().spec();
    let boundary = SolidBoundary::capture(fixture.volume(), spec)
        .map_err(|error| ClientError::Render(format!("voxel boundary: {error}")))?;
    let origin = spec.origin();
    let [dx, dy, dz] = spec.dimensions();
    let h = fixture.grid().config().cell_size_m as f32;
    let mut instances = Vec::new();
    for z in 0..dz as i64 {
        for y in 0..dy as i64 {
            for x in 0..dx as i64 {
                let cell = GlobalCell::new(origin.x + x, origin.y + y, origin.z + z);
                if boundary.is_solid(cell) != Some(true) {
                    continue;
                }
                instances.push(CubeInstance::new(
                    [
                        (x as f32 + 0.5) * h,
                        (y as f32 + 0.5) * h,
                        (z as f32 + 0.5) * h,
                    ],
                    if x == 12 * i64::from(fixture.scale()) {
                        MATERIAL_DAM
                    } else {
                        MATERIAL_STONE
                    },
                    [h; 3],
                    CubeInstance::IDENTITY_ROTATION,
                ));
            }
        }
    }
    Ok(instances)
}

fn build_water(fixture: &GridReservoirFixture) -> Vec<CubeInstance> {
    let grid = fixture.grid();
    let spec = grid.spec();
    let origin = spec.origin();
    let [dx, dy, dz] = spec.dimensions();
    let h = grid.config().cell_size_m;
    let mut instances = Vec::new();
    for z in 0..dz {
        for y in 0..dy {
            for x in 0..dx {
                let cell = GlobalCell::new(
                    origin.x + i64::from(x),
                    origin.y + i64::from(y),
                    origin.z + i64::from(z),
                );
                let Some(fraction) = grid.fraction_at(cell) else {
                    continue;
                };
                if fraction <= 0.0 {
                    continue;
                }
                let height = (h * fraction) as f32;
                instances.push(CubeInstance::new(
                    [
                        (x as f64 + 0.5) as f32 * h as f32,
                        (y as f64) as f32 * h as f32 + 0.5 * height,
                        (z as f64 + 0.5) as f32 * h as f32,
                    ],
                    MATERIAL_WATER,
                    [h as f32, height, h as f32],
                    CubeInstance::IDENTITY_ROTATION,
                ));
            }
        }
    }
    instances
}
