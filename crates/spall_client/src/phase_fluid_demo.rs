//! Local ENG-122 visual comparison. Never advances the authoritative game.
use crate::{
    ClientError,
    window::{AcquireOutcome, WorldRenderer},
};
use glam::Vec3;
use spall_fluid::grid_mac::{MacConfig, PressurePreconditioner};
use spall_fluid::phase_fixtures::PhaseReferenceScene;
use spall_fluid::phase_pressure::{PhasePressureConfig, PhasePressureWorld};
use spall_render::{Camera, CubeInstance, EnvironmentPreset, Material};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorGrabMode, Window, WindowAttributes, WindowId},
};

const DT: Duration = Duration::from_millis(10);
const MAX_STEPS_PER_FRAME: usize = 2;

#[derive(Default)]
pub struct PhaseFluidDemoOptions {
    pub conservative_momentum: bool,
    pub compressible_air: bool,
    pub two_phase_air: bool,
    pub autoplay: bool,
    pub max_frames: Option<u64>,
    pub capture: Option<PathBuf>,
}

pub fn run_phase_fluid_demo_window(options: PhaseFluidDemoOptions) -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = PhaseDemoApp::new(options)?;
    event_loop.run_app(&mut app)?;
    app.result
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AirModel {
    FreelyDisplaced,
    Incompressible,
    Compressible,
}
impl AirModel {
    fn next(self, conservative: bool) -> Self {
        match self {
            Self::FreelyDisplaced => Self::Incompressible,
            Self::Incompressible => Self::Compressible,
            Self::Compressible if conservative => Self::Incompressible,
            Self::Compressible => Self::FreelyDisplaced,
        }
    }
}
struct Comparison {
    world: PhasePressureWorld,
    scene: PhaseReferenceScene,
    conservative: bool,
    air_model: AirModel,
    accepted_steps: u64,
    initial_volume: f64,
    failure: Option<String>,
}
impl Comparison {
    fn new(
        scene: PhaseReferenceScene,
        conservative: bool,
        air_model: AirModel,
    ) -> Result<Self, ClientError> {
        let phase = scene.build().map_err(render_error)?;
        let initial_volume = phase.water_volume_m3();
        let config = PhasePressureConfig {
            mac: MacConfig {
                pressure_max_iterations: 1000,
                ..Default::default()
            },
            air_density_kg_m3: 1.2,
            preconditioner: PressurePreconditioner::Multigrid,
            max_retained_array_bytes: 100_000_000,
        };
        let mut world = if air_model == AirModel::FreelyDisplaced {
            PhasePressureWorld::new_water_only(phase, config)
        } else {
            PhasePressureWorld::new(phase, config)
        }
        .map_err(render_error)?;
        world
            .set_compressible_enclosed_air(air_model == AirModel::Compressible)
            .map_err(render_error)?;
        if conservative {
            world.enable_conservative_momentum().map_err(render_error)?;
        }
        Ok(Self {
            world,
            scene,
            conservative,
            air_model,
            accepted_steps: 0,
            initial_volume,
            failure: None,
        })
    }
    fn step(&mut self) -> bool {
        if self.failure.is_some() {
            return false;
        }
        match self.world.step(DT.as_secs_f64()) {
            Ok(_) => {
                self.accepted_steps += 1;
                true
            }
            Err(e) => {
                eprintln!("phase water viewer: {e}");
                self.failure = Some(e.to_string());
                false
            }
        }
    }
    fn method(&self) -> &'static str {
        if self.conservative {
            "Conservative momentum"
        } else {
            "Original velocity sampling"
        }
    }
    fn air(&self) -> &'static str {
        match self.air_model {
            AirModel::FreelyDisplaced => "Water only - freely displaced air (default)",
            AirModel::Compressible => "Compressible air (experimental)",
            AirModel::Incompressible => "Incompressible air comparison",
        }
    }
}
fn render_error(error: impl std::fmt::Display) -> ClientError {
    ClientError::Render(error.to_string())
}

struct PhaseDemoApp {
    window: Option<Arc<Window>>,
    renderer: Option<WorldRenderer>,
    state: Comparison,
    options: PhaseFluidDemoOptions,
    terrain: Vec<CubeInstance>,
    terrain_dirty: bool,
    camera: Camera,
    held: HashSet<KeyCode>,
    cursor_locked: bool,
    paused: bool,
    accumulator: Duration,
    last_frame: Instant,
    frames: u64,
    result: Result<(), ClientError>,
}
impl PhaseDemoApp {
    fn new(options: PhaseFluidDemoOptions) -> Result<Self, ClientError> {
        let state = Comparison::new(
            PhaseReferenceScene::Channel,
            options.conservative_momentum,
            if options.compressible_air {
                AirModel::Compressible
            } else if options.two_phase_air || options.conservative_momentum {
                AirModel::Incompressible
            } else {
                AirModel::FreelyDisplaced
            },
        )?;
        let terrain = build_terrain(&state.world);
        let camera = scene_camera(state.scene);
        let paused = !options.autoplay;
        Ok(Self {
            window: None,
            renderer: None,
            state,
            options,
            terrain,
            terrain_dirty: true,
            camera,
            held: HashSet::new(),
            cursor_locked: false,
            paused,
            accumulator: Duration::ZERO,
            last_frame: Instant::now(),
            frames: 0,
            result: Ok(()),
        })
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        eprintln!("phase water viewer: {error}");
        self.result = Err(error);
        event_loop.exit();
    }
    fn cursor(&mut self, locked: bool) {
        let Some(window) = &self.window else { return };
        let outcome = if locked {
            window
                .set_cursor_grab(CursorGrabMode::Confined)
                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked))
        } else {
            window.set_cursor_grab(CursorGrabMode::None)
        };
        if outcome.is_ok() {
            window.set_cursor_visible(!locked);
            self.cursor_locked = locked;
        }
    }
    fn reset(&mut self, scene: PhaseReferenceScene, conservative: bool, air: AirModel) {
        match Comparison::new(scene, conservative, air) {
            Ok(state) => {
                if scene != self.state.scene {
                    self.camera = scene_camera(scene);
                }
                self.terrain = build_terrain(&state.world);
                self.terrain_dirty = true;
                self.state = state;
                self.paused = true;
                self.accumulator = Duration::ZERO;
                self.last_frame = Instant::now();
            }
            Err(error) => {
                self.state.failure = Some(error.to_string());
                self.paused = true;
            }
        }
    }
    fn action(&mut self, key: KeyCode) {
        let (scene, method, air) = (
            self.state.scene,
            self.state.conservative,
            self.state.air_model,
        );
        match key {
            KeyCode::Digit1 => self.reset(PhaseReferenceScene::Channel, method, air),
            KeyCode::Digit2 => self.reset(PhaseReferenceScene::LowDam, method, air),
            KeyCode::Digit3 => self.reset(PhaseReferenceScene::FullWall, method, air),
            KeyCode::KeyM if air != AirModel::FreelyDisplaced => self.reset(scene, !method, air),
            KeyCode::KeyG => self.reset(scene, method, air.next(method)),
            KeyCode::KeyR => self.reset(scene, method, air),
            KeyCode::Space if self.state.failure.is_none() => {
                self.paused = !self.paused;
                self.accumulator = Duration::ZERO;
            }
            KeyCode::KeyN if self.paused => {
                self.state.step();
            }
            KeyCode::F12 => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.screenshot_request =
                        Some(PathBuf::from(".local/screenshots/phase-water.png"));
                }
            }
            _ => {}
        }
    }
    fn update(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_frame)
            .min(Duration::from_millis(100));
        self.last_frame = now;
        if !self.paused {
            self.accumulator += elapsed;
            let mut steps = 0;
            while self.accumulator >= DT && steps < MAX_STEPS_PER_FRAME {
                if !self.state.step() {
                    self.paused = true;
                    self.accumulator = Duration::ZERO;
                    break;
                }
                self.accumulator -= DT;
                steps += 1;
            }
            // Bounded viewer workload. Simulated time advances only accepted
            // fixed steps, never stretched dt to catch up a slow renderer.
            if steps == MAX_STEPS_PER_FRAME && self.accumulator >= DT {
                self.accumulator = Duration::ZERO;
            }
        }
        let mut movement = Vec3::ZERO;
        for (key, dir) in [
            (KeyCode::KeyW, Vec3::Z),
            (KeyCode::KeyS, -Vec3::Z),
            (KeyCode::KeyD, Vec3::X),
            (KeyCode::KeyA, -Vec3::X),
            (KeyCode::KeyE, Vec3::Y),
            (KeyCode::KeyQ, -Vec3::Y),
        ] {
            if self.held.contains(&key) {
                movement += dir;
            }
        }
        if movement.length_squared() > 0.0 {
            self.camera
                .fly(movement.normalize() * 3.5 * elapsed.as_secs_f32());
        }
    }
    fn render(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        self.update(now);
        let water = build_water(&self.state.world);
        let status = if let Some(error) = &self.state.failure {
            format!(
                "STOPPED at {:.2}s: {error}\nThe failed step was not applied. M/G switches reset; R resets this case.",
                self.state.accepted_steps as f64 * DT.as_secs_f64()
            )
        } else {
            format!(
                "{} | Time {:.2}s | Water {:.4} m3 | Accounting error {:.2e} m3\nLocal experiment with the game water look; not a generated game world.",
                if self.paused {
                    "PAUSED - Space to run"
                } else {
                    "RUNNING"
                },
                self.state.accepted_steps as f64 * DT.as_secs_f64(),
                self.state.world.phase().water_volume_m3(),
                self.state.world.phase().water_volume_m3()
                    + self.state.world.solver().cumulative_open_outflow_m3()
                    - self.state.initial_volume
            )
        };
        let heading = format!(
            "{} | {}\n{}",
            self.state.scene.label(),
            self.state.method(),
            self.state.air()
        );
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        let frame = match renderer
            .begin_frame(self.terrain_dirty.then_some(self.terrain.as_slice()), &[])
        {
            Ok(AcquireOutcome::Ready(frame)) => frame,
            Ok(AcquireOutcome::Skipped(_)) => return,
            Err(e) => return self.fail(event_loop, e),
        };
        self.terrain_dirty = false;
        renderer.set_water_look(Some(water));
        self.frames += 1;
        let capture_frame = self
            .options
            .max_frames
            .map_or(20, |n| n.saturating_sub(10).max(1));
        if self.frames == capture_frame && self.options.capture.is_some() {
            renderer.screenshot_request = self.options.capture.clone();
        }
        if let Some(window) = &self.window {
            window.set_title(&format!(
                "Spall water comparison - {} - {}",
                self.state.method(),
                if self.state.failure.is_some() {
                    "STOPPED"
                } else if self.paused {
                    "PAUSED"
                } else {
                    "RUNNING"
                }
            ));
        }
        if let Err(e)=renderer.finish_frame(frame,Some(&(self.camera.position,self.camera.forward())),&[],&[],
            Some((&heading,"G switch air | M momentum (air comparisons only) | 1 channel / 2 low dam / 3 wall\nSpace play/pause | N one step | R reset | Click + mouse look | WASD/QE move | Esc release | F12 screenshot",&status)),None,None) {
            return self.fail(event_loop,e);
        }
        if self.options.max_frames.is_some_and(|n| self.frames >= n) {
            event_loop.exit();
        }
    }
}
impl ApplicationHandler for PhaseDemoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        event_loop.set_control_flow(ControlFlow::Poll);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall water comparison")
                .with_inner_size(PhysicalSize::new(1440, 900)),
        ) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, render_error(e)),
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
            false,
        ) {
            Ok(r) => {
                self.window = Some(window);
                self.renderer = Some(r);
                self.last_frame = Instant::now();
            }
            Err(e) => self.fail(event_loop, e),
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let Some(renderer) = &mut self.renderer {
            renderer.handle_window_event(&event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.camera.aspect = size.width as f32 / size.height.max(1) as f32;
                if let Some(r) = &mut self.renderer {
                    r.resize(size);
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
                if event.state == ElementState::Pressed {
                    if !event.repeat {
                        if key == KeyCode::Escape {
                            self.cursor(false);
                        } else {
                            self.action(key);
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
            self.camera
                .look(-(delta.0 as f32) * 0.0025, -(delta.1 as f32) * 0.0025);
        }
    }
    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}
fn scene_camera(scene: PhaseReferenceScene) -> Camera {
    let (eye, target) = if scene == PhaseReferenceScene::Channel {
        (Vec3::new(14.0, 11.0, 7.0), Vec3::new(5.0, 1.0, 1.5))
    } else {
        (Vec3::new(3.5, 2.5, 3.0), Vec3::new(1.1, 0.5, 0.4))
    };
    Camera::looking_along(eye, target - eye, 60.0_f32.to_radians(), 1440.0 / 900.0)
}
fn build_terrain(world: &PhasePressureWorld) -> Vec<CubeInstance> {
    let geometry = world.phase().geometry();
    let spec = geometry.fine_spec();
    let [nx, ny, nz] = spec.dimensions();
    let h = world.phase().voxel_size_m() as f32;
    let mut out = Vec::new();
    for i in 0..spec.cell_count() {
        let cell = spall_core::GlobalCell::new(
            (i % nx as usize) as i64,
            (i / nx as usize % ny as usize) as i64,
            (i / (nx as usize * ny as usize)) as i64,
        );
        if geometry.component_at(cell).is_none() {
            let (x, y, z) = (
                i % nx as usize,
                i / nx as usize % ny as usize,
                i / (nx as usize * ny as usize),
            );
            out.push(CubeInstance::new(
                [
                    (x as f32 + 0.5) * h,
                    (y as f32 + 0.5) * h,
                    (z as f32 + 0.5) * h,
                ],
                0,
                [h; 3],
                CubeInstance::IDENTITY_ROTATION,
            ));
        }
    }
    // Visualize the already-closed lower domain boundary; no added collision.
    out.push(CubeInstance::new(
        [nx as f32 * h * 0.5, -0.025, nz as f32 * h * 0.5],
        0,
        [nx as f32 * h, 0.05, nz as f32 * h],
        CubeInstance::IDENTITY_ROTATION,
    ));
    out
}
// The local solver remains full precision. This snapshot is presentation-only:
// use the same byte fractions and surface/underwater path as replicated water.
fn build_water(world: &PhasePressureWorld) -> crate::water_look::WaterLook {
    let spec = world.phase().geometry().fine_spec();
    let dimensions = spec.dimensions();
    let frame = spall_protocol::WaterKeyframe {
        server_tick: spall_core::Tick(0),
        frame_seq: 0,
        origin: spec.origin(),
        dimensions,
        coarsen: 1,
        fractions: world
            .phase()
            .fractions()
            .iter()
            .map(|fraction| (fraction * 255.0).round() as u8)
            .collect(),
    };
    // These shared reference fixtures are quarter-metre, origin-zero domains.
    // Bound the field to the fixture rather than the much larger game window.
    let h = world.phase().voxel_size_m();
    let center = [
        f64::from(dimensions[0]) * h * 0.5,
        0.0,
        f64::from(dimensions[2]) * h * 0.5,
    ];
    let radius = (f64::from(dimensions[0].max(dimensions[2])) * h * 0.5 + h) as f32;
    crate::water_look::build_water_look(&[frame], center, radius, 0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_water_only_and_explicit_air_comparisons_reset_without_silent_model_changes() {
        let mut app = PhaseDemoApp::new(PhaseFluidDemoOptions::default()).unwrap();
        let initial = app.state.world.phase().fractions().to_vec();
        assert_eq!(app.state.air_model, AirModel::FreelyDisplaced);
        app.action(KeyCode::KeyM);
        assert!(!app.state.conservative);
        assert_eq!(app.state.air_model, AirModel::FreelyDisplaced);
        app.action(KeyCode::KeyN);
        assert_eq!(app.state.accepted_steps, 1);
        app.action(KeyCode::KeyG);
        assert_eq!(app.state.air_model, AirModel::Incompressible);
        assert_eq!(app.state.accepted_steps, 0);
        assert_eq!(app.state.world.phase().fractions(), initial);
        app.action(KeyCode::KeyM);
        app.action(KeyCode::KeyG);
        assert_eq!(app.state.air_model, AirModel::Compressible);
        app.action(KeyCode::KeyG);
        assert_eq!(app.state.air_model, AirModel::Incompressible);
        assert!(app.state.conservative);
        app.action(KeyCode::KeyM);
        app.action(KeyCode::KeyG);
        app.action(KeyCode::KeyG);
        assert_eq!(app.state.air_model, AirModel::FreelyDisplaced);
        assert_eq!(app.state.world.phase().fractions(), initial);
        assert!(app.paused);
    }
    #[test]
    fn game_water_look_tracks_fixture_resets_without_mutating_phase() {
        let mut app = PhaseDemoApp::new(PhaseFluidDemoOptions::default()).unwrap();
        let fractions = app.state.world.phase().fractions().to_vec();
        let look = build_water(&app.state.world);
        assert!(!look.indices.is_empty());
        assert_eq!(look.field.surface_at(0.125, 0.125), Some(2.25));
        assert_eq!(look.field.surface_at(6.125, 1.625), None);
        assert_eq!(app.state.world.phase().fractions(), fractions);
        app.action(KeyCode::Digit3);
        let look = build_water(&app.state.world);
        assert_eq!(look.field.surface_at(0.125, 0.125), Some(0.5));
        assert_eq!(look.field.surface_at(1.125, 0.125), None);
        assert_eq!(look.field.surface_at(1.625, 0.125), None);
    }
    #[test]
    fn switching_method_restarts_identical_phase_without_changing_air_or_camera() {
        let mut app = PhaseDemoApp::new(PhaseFluidDemoOptions {
            two_phase_air: true,
            ..Default::default()
        })
        .unwrap();
        let initial = app.state.world.phase().fractions().to_vec();
        let eye = app.camera.position;
        app.action(KeyCode::KeyN);
        assert_eq!(app.state.accepted_steps, 1);
        app.action(KeyCode::KeyM);
        assert!(app.state.conservative);
        assert_eq!(app.state.air_model, AirModel::Incompressible);
        assert!(app.paused);
        assert_eq!(app.state.accepted_steps, 0);
        assert_eq!(app.state.world.phase().fractions(), initial);
        assert_eq!(app.camera.position, eye);
        app.action(KeyCode::KeyG);
        assert_eq!(app.state.air_model, AirModel::Compressible);
        assert!(app.state.conservative);
        assert_eq!(app.state.world.phase().fractions(), initial);
        app.action(KeyCode::KeyM);
        assert!(!app.state.conservative);
        assert_eq!(app.state.air_model, AirModel::Compressible);
    }
    #[test]
    fn rejected_step_stays_visible_and_requires_explicit_reset_or_switch() {
        let mut state =
            Comparison::new(PhaseReferenceScene::Channel, true, AirModel::Compressible).unwrap();
        for _ in 0..60 {
            let before = state.world.phase().fractions().to_vec();
            let steps = state.accepted_steps;
            if !state.step() {
                assert_eq!(state.accepted_steps, steps);
                assert_eq!(state.world.phase().fractions(), before);
                break;
            }
        }
        assert!(
            state
                .failure
                .as_ref()
                .unwrap()
                .contains("dual mass mismatch")
        );
        let before = state.world.phase().fractions().to_vec();
        assert!(!state.step());
        assert_eq!(state.world.phase().fractions(), before);
        let mut app = PhaseDemoApp::new(PhaseFluidDemoOptions::default()).unwrap();
        app.state = state;
        app.action(KeyCode::Space);
        assert!(app.paused);
        assert!(app.state.failure.is_some());
        app.action(KeyCode::KeyG);
        assert!(app.state.failure.is_none());
        assert_eq!(app.state.accepted_steps, 0);
        assert!(app.state.conservative);
        assert_eq!(app.state.air_model, AirModel::Incompressible);
        assert!(app.state.step());
    }
}
