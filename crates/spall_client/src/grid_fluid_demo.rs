//! Local canal viewer using the gameplay water policy and renderer.
//!
//! This owns a private copy of the CPU grid and never advances the game/server
//! simulation. Presentation snapshots feed the shared smoothed water surface;
//! their byte fractions never feed back into the full-precision solver.

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

/// Opens an isolated interactive window for the bounded MAC fixtures.
pub fn run_grid_fluid_demo_window(
    max_frames: Option<u64>,
    capture: Option<std::path::PathBuf>,
    room_below: bool,
) -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = GridFluidDemoApp::new(if room_below {
        Scene::RoomBelow
    } else {
        Scene::Reservoirs
    })?;
    app.max_frames = max_frames;
    app.capture = capture;
    event_loop.run_app(&mut app)?;
    app.result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scene {
    Reservoirs,
    RoomBelow,
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
            Self::RoomBelow => "Canal above dry room (front wall cutaway)",
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
            KeyCode::Digit7 => Some(Self::RoomBelow),
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
    max_frames: Option<u64>,
    capture: Option<std::path::PathBuf>,
    frames: u64,
}

impl GridFluidDemoApp {
    fn new(scene: Scene) -> Result<Self, ClientError> {
        let fixture = make_fixture(scene)?;
        let terrain = build_terrain(&fixture)?;
        let eye = if scene == Scene::RoomBelow {
            Vec3::new(8.0, 4.0, 6.0)
        } else {
            Vec3::new(6.0, 10.0, 4.0)
        };
        let camera = Camera::looking_along(
            eye,
            Vec3::new(3.0, 0.0, 1.0) - eye,
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
            max_frames: None,
            capture: None,
            frames: 0,
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
                fresh.max_frames = self.max_frames;
                fresh.capture = self.capture.clone();
                fresh.frames = self.frames;
                fresh.camera = self.camera;
                fresh.cursor_locked = self.cursor_locked;
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
            KeyCode::KeyH if self.scene == Scene::RoomBelow => {
                match self.fixture.open_floor_probe() {
                    Ok(()) => {
                        self.message = "Floor hole opened deliberately".into();
                        self.refresh_terrain();
                    }
                    Err(error) => self.message = format!("Floor opening rejected: {error}"),
                }
            }
            KeyCode::KeyR => self.set_scene(self.scene),
            KeyCode::F12 => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.screenshot_request = Some(std::path::PathBuf::from(
                        ".local/screenshots/canal-water.png",
                    ));
                }
            }
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
        renderer.set_water_look(Some(water));
        self.frames += 1;
        if self.capture.is_some()
            && self.frames == self.max_frames.map_or(20, |n| n.saturating_sub(10).max(1))
        {
            renderer.screenshot_request = self.capture.clone();
        }

        let room_status = if self.scene == Scene::RoomBelow {
            format!(
                " | room water {:.6} m3 | solid floor 0.25 m | front wall hidden for inspection",
                room_water_volume(&self.fixture)
            )
        } else {
            String::new()
        };
        let status = format!(
            "{} · t={:.2}s · {:.3} m³ retained · {:.3} m³ top outflow · {}{}",
            if self.paused { "PAUSED" } else { "LIVE" },
            self.simulated_time_s,
            self.fixture.grid().water_volume_m3(),
            self.fixture.grid().cumulative_open_outflow_m3(),
            self.message,
            room_status,
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
                "WATER CANAL - FREELY DISPLACED AIR",
                "1 reservoirs · 2 canal · 3 breach · 4 basin · 5 tunnel · 6 level basin · 7 room below · H floor hole (room) · C open · B breach · X close · Space pause · R reset · F12 screenshot · WASD/QE move · mouse look",
                &status,
            )),
            None,
        ) {
            self.fail(event_loop, error);
        }
        if self.max_frames.is_some_and(|n| self.frames >= n) {
            event_loop.exit();
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
                            | KeyCode::F12
                            | KeyCode::KeyH
                            | KeyCode::Digit1
                            | KeyCode::Digit2
                            | KeyCode::Digit3
                            | KeyCode::Digit4
                            | KeyCode::Digit5
                            | KeyCode::Digit6
                            | KeyCode::Digit7 => self.action(key),
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
        Scene::RoomBelow => GridReservoirFixture::new_room_below_canal()
            .map_err(|error| ClientError::Render(error.to_string())),
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
        .set_freely_displaced_air()
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
                // The inspection cutaway hides the front wall only in drawing.
                // The captured solver boundary still includes every wall voxel.
                if cell.y < 0 && z == dz as i64 - 1 {
                    continue;
                }
                instances.push(CubeInstance::new(
                    [
                        (x as f32 + 0.5) * h,
                        (cell.y as f32 + 0.5) * h,
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

// Fixtures use quarter-metre, origin-zero grids. The shared renderer consumes
// the same byte-fraction presentation as gameplay; canonical amounts stay f64.
fn build_water(fixture: &GridReservoirFixture) -> crate::water_look::WaterLook {
    let grid = fixture.grid();
    let spec = grid.spec();
    let dimensions = spec.dimensions();
    let frame = spall_protocol::WaterKeyframe {
        server_tick: spall_core::Tick(0),
        frame_seq: 0,
        origin: spec.origin(),
        dimensions,
        coarsen: 1,
        fractions: grid
            .fractions()
            .iter()
            .map(|f| (f * 255.0).round() as u8)
            .collect(),
    };
    let h = grid.config().cell_size_m;
    let center = [
        f64::from(dimensions[0]) * h * 0.5,
        0.0,
        f64::from(dimensions[2]) * h * 0.5,
    ];
    let radius = (f64::from(dimensions[0].max(dimensions[2])) * h * 0.5 + h) as f32;
    crate::water_look::build_water_look(&[frame], center, radius, 0)
}

fn room_water_volume(fixture: &GridReservoirFixture) -> f64 {
    let grid = fixture.grid();
    let spec = grid.spec();
    let [nx, ny, _] = spec.dimensions().map(|n| n as usize);
    grid.fractions()
        .iter()
        .enumerate()
        .filter(|(i, _)| spec.origin().y + ((i / nx % ny) as i64) < 0)
        .map(|(_, f)| f * grid.config().cell_size_m.powi(3))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canal_water_rendering_does_not_change_solver_and_reset_restores_water() {
        let mut app = GridFluidDemoApp::new(Scene::Reservoirs).unwrap();
        let initial = app.fixture.grid().fractions().to_vec();
        let initial_volume = app.fixture.grid().water_volume_m3();
        let look = build_water(&app.fixture);
        assert!(!look.indices.is_empty());
        assert_eq!(app.fixture.grid().fractions(), initial);
        app.action(KeyCode::KeyC);
        assert!(app.canal_open);
        app.action(KeyCode::KeyX);
        assert!(!app.canal_open);
        assert_eq!(app.fixture.grid().water_volume_m3(), initial_volume);
        app.action(KeyCode::Space);
        assert!(app.paused);
        app.action(KeyCode::Space);
        assert!(!app.paused);
        app.action(KeyCode::KeyB);
        assert!(app.dam_broken);
        for _ in 0..120 {
            app.fixture.step(FIXED_DT.as_secs_f64()).unwrap();
        }
        assert_ne!(app.fixture.grid().fractions(), initial);
        let before = app.fixture.grid().fractions().to_vec();
        let _ = build_water(&app.fixture);
        assert_eq!(app.fixture.grid().fractions(), before);
        assert!(
            (app.fixture.grid().water_volume_m3()
                + app.fixture.grid().cumulative_open_outflow_m3()
                - initial_volume)
                .abs()
                < 1e-9
        );
        app.action(KeyCode::KeyR);
        assert!(!app.canal_open && !app.dam_broken);
        assert_eq!(app.fixture.grid().fractions(), initial);
        assert_eq!(app.simulated_time_s, 0.0);
    }

    #[test]
    fn room_stays_dry_below_intact_floor_and_floods_through_real_hole() {
        let mut app = GridFluidDemoApp::new(Scene::RoomBelow).unwrap();
        assert_eq!(room_water_volume(&app.fixture), 0.0);
        app.action(KeyCode::KeyC);
        app.action(KeyCode::KeyB);
        for _ in 0..600 {
            app.fixture.step(FIXED_DT.as_secs_f64()).unwrap();
        }
        assert_eq!(room_water_volume(&app.fixture), 0.0);
        let initial = app.fixture.grid().water_volume_m3();
        app.action(KeyCode::KeyH);
        for _ in 0..600 {
            app.fixture.step(FIXED_DT.as_secs_f64()).unwrap();
        }
        assert!(room_water_volume(&app.fixture) > 0.001);
        println!(
            "{}",
            serde_json::json!({"scenario":"canal_room_floor", "intact_steps":600, "intact_room_water_m3":0.0, "hole_steps":600, "hole_room_water_m3":room_water_volume(&app.fixture), "cell_size_m":0.25})
        );
        assert!(
            (app.fixture.grid().water_volume_m3()
                + app.fixture.grid().cumulative_open_outflow_m3()
                - initial)
                .abs()
                < 1e-9
        );
        app.action(KeyCode::KeyR);
        assert_eq!(room_water_volume(&app.fixture), 0.0);
        let boundary =
            SolidBoundary::capture(app.fixture.volume(), app.fixture.grid().spec()).unwrap();
        assert_eq!(boundary.is_solid(GlobalCell::new(6, 0, 3)), Some(true));
    }

    #[test]
    fn all_canal_scenes_step_with_gameplay_water_policy() {
        for scene in [
            Scene::Reservoirs,
            Scene::Canal,
            Scene::Breach,
            Scene::Basin,
            Scene::Tunnel,
            Scene::Equilibrium,
        ] {
            let mut fixture = make_fixture(scene).unwrap();
            let initial = fixture.grid().water_volume_m3();
            for _ in 0..600 {
                fixture.step(FIXED_DT.as_secs_f64()).unwrap();
            }
            assert!(
                (fixture.grid().water_volume_m3() + fixture.grid().cumulative_open_outflow_m3()
                    - initial)
                    .abs()
                    < 1e-9,
                "{}",
                scene.label()
            );
            assert!(!build_water(&fixture).indices.is_empty());
        }
    }
}
