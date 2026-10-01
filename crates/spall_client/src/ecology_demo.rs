//! Interactive, client-local visualization of the CPU ecology increment.
//! The window owns its own Simulation and never changes a network session.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::Vec3;
use spall_core::{BRUSH_UNIT, BrushPoint, EntityId, GlobalCell, MaterialId, SphereBrush};
use spall_ecology::{
    CommitAck, EcologyConfig, EcologyInputs, EcologyState, SpeciesDefinition, SpeciesId,
    acknowledge, cut_branch, destroy_root, harvest_grass, proposal_current, update,
};
use spall_render::{Camera, CubeInstance, EnvironmentPreset, materials_from_manifest};
use spall_sim::{
    EditIntent, EditKind, EditTarget, RequestId, Simulation, SimulationConfig, WorldSetup,
};
use spall_voxel::Sample;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{DeviceEvent, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, DeviceEvents, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowAttributes, WindowId};

use crate::ClientError;
use crate::window::{AcquireOutcome, WorldRenderer};

const CELL_M: f32 = 0.25;
const CAMERA_SPEED_M_S: f32 = 8.0;
const MOUSE_SENSITIVITY: f32 = 0.0025;

/// Game-authored setup for the isolated ecology showcase window.
pub struct EcologyDemoSetup {
    pub world: WorldSetup,
    /// Small, terrain-validated ground crop around the initial tree.
    pub ground_cells: Vec<(GlobalCell, MaterialId)>,
    pub focus: GlobalCell,
    pub state: EcologyState,
    pub inputs: EcologyInputs,
    pub definitions: BTreeMap<SpeciesId, SpeciesDefinition>,
    pub config: EcologyConfig,
    pub grass_patch_id: u64,
    pub grass_material: MaterialId,
}

struct EcologyDemoState {
    ground_cells: Vec<(GlobalCell, MaterialId)>,
    state: EcologyState,
    inputs: EcologyInputs,
    definitions: BTreeMap<SpeciesId, SpeciesDefinition>,
    config: EcologyConfig,
    grass_patch_id: u64,
    grass_material: MaterialId,
}

/// Opens the local visualizer. Ecology time controls never change wall-clock
/// simulation, networking, or persistence state.
pub fn run_ecology_demo_window(setup: EcologyDemoSetup) -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = EcologyDemoApp::new(setup)?;
    event_loop.run_app(&mut app)?;
    app.result
}

#[derive(Debug, Clone, PartialEq)]
struct DemoClock {
    paused: bool,
    speed_index: u8,
    fractional_ms: f64,
}

impl Default for DemoClock {
    fn default() -> Self {
        Self {
            paused: false,
            speed_index: 0,
            fractional_ms: 0.0,
        }
    }
}

impl DemoClock {
    fn multiplier(&self) -> u64 {
        [1, 4, 16][usize::from(self.speed_index.min(2))]
    }

    fn advance_ms(&mut self, wall: Duration) -> u64 {
        if self.paused {
            0
        } else {
            let total = self.fractional_ms + wall.as_secs_f64() * 1000.0 * self.multiplier() as f64;
            let whole = total.floor() as u64;
            self.fractional_ms = total - whole as f64;
            whole
        }
    }
}

struct EcologyDemoApp {
    window: Option<Arc<Window>>,
    renderer: Option<WorldRenderer>,
    sim: Simulation,
    setup: EcologyDemoState,
    terrain: Vec<CubeInstance>,
    terrain_dirty: bool,
    held: HashSet<KeyCode>,
    camera: Camera,
    cursor_locked: bool,
    clock: DemoClock,
    last_frame: Instant,
    title_update: Instant,
    request: u64,
    next_tree_cell: usize,
    branch_cut: bool,
    root_cut: bool,
    message: String,
    result: Result<(), ClientError>,
}

impl EcologyDemoApp {
    fn new(setup: EcologyDemoSetup) -> Result<Self, ClientError> {
        let EcologyDemoSetup {
            world,
            ground_cells,
            focus,
            state,
            inputs,
            definitions,
            config,
            grass_patch_id,
            grass_material,
        } = setup;
        let sim = Simulation::new(SimulationConfig::new(world))
            .map_err(|error| ClientError::Render(format!("ecology simulation: {error}")))?;
        let focus = cell_m(focus);
        let eye = focus + Vec3::new(6.0, 6.0, 9.0);
        let camera = Camera::looking_along(eye, focus - eye, 60.0_f32.to_radians(), 16.0 / 9.0);
        let setup = EcologyDemoState {
            ground_cells,
            state,
            inputs,
            definitions,
            config,
            grass_patch_id,
            grass_material,
        };
        let mut app = Self {
            window: None,
            renderer: None,
            sim,
            setup,
            terrain: Vec::new(),
            terrain_dirty: true,
            held: HashSet::new(),
            camera,
            cursor_locked: false,
            clock: DemoClock::default(),
            last_frame: Instant::now(),
            title_update: Instant::now(),
            request: 1,
            next_tree_cell: 0,
            branch_cut: false,
            root_cut: false,
            message: "Showcase ready".into(),
            result: Ok(()),
        };
        app.refresh_geometry();
        Ok(app)
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: ClientError) {
        eprintln!("sandbox-client ecology demo: {error}");
        self.result = Err(error);
        event_loop.exit();
    }

    fn capture_cursor(&mut self, locked: bool) {
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

    fn submit_cell_edit(&mut self, cell: GlobalCell, kind: EditKind) -> Result<bool, ClientError> {
        let center = BrushPoint::from_units(
            cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
            cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
            cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
        );
        let request = RequestId(self.request);
        self.request = self.request.saturating_add(1);
        self.sim
            .submit(EditIntent {
                request_id: request,
                actor: EntityId::new(1).map_err(|e| ClientError::Render(e.to_string()))?,
                target: EditTarget::Terrain,
                kind,
                brush: SphereBrush::new(center, 0)
                    .map_err(|e| ClientError::Render(e.to_string()))?,
                explosion: None,
            })
            .map_err(|e| ClientError::Render(e.to_string()))?;
        let report = self
            .sim
            .tick()
            .map_err(|e| ClientError::Render(e.to_string()))?;
        Ok(report.committed.iter().any(|(id, _)| *id == request))
    }

    fn act(&mut self, key: KeyCode) {
        match key {
            KeyCode::Space => self.clock.paused = !self.clock.paused,
            KeyCode::Equal | KeyCode::NumpadAdd => {
                self.clock.speed_index = self.clock.speed_index.saturating_add(1).min(2)
            }
            KeyCode::Minus | KeyCode::NumpadSubtract => {
                self.clock.speed_index = self.clock.speed_index.saturating_sub(1)
            }
            KeyCode::Period if self.clock.paused => {
                self.advance_ecology(self.setup.config.update_interval_ms)
            }
            KeyCode::KeyH => {
                let removed = harvest_grass(&mut self.setup.state, self.setup.grass_patch_id, 25);
                self.message = format!("Harvested {removed} grass biomass");
                self.refresh_geometry();
            }
            KeyCode::KeyB if !self.branch_cut => self.cut_one_branch(),
            KeyCode::KeyX if !self.root_cut => self.destroy_one_root(),
            _ => {}
        }
    }

    fn cut_one_branch(&mut self) {
        let Some((&plant_id, plant)) = self.setup.state.plants.iter().next() else {
            return;
        };
        let candidate = plant
            .skeleton
            .iter()
            .enumerate()
            .skip(1)
            .find(|(index, branch)| {
                *index < self.next_tree_cell
                    && !branch.removed
                    && matches!(
                        self.sim.world().terrain().volume.sample(branch.cell),
                        Ok(Sample::Filled(_))
                    )
            })
            .map(|(index, branch)| (index, branch.cell));
        let Some((index, cell)) = candidate else {
            self.message = "No grown branch to cut yet".into();
            return;
        };
        match self.submit_cell_edit(cell, EditKind::Cut) {
            Ok(true) => {
                cut_branch(&mut self.setup.state, plant_id, index);
                self.branch_cut = true;
                self.message = "Branch cut; regrowth stays attached to surviving skeleton".into();
                self.refresh_geometry();
            }
            Ok(false) => self.message = "Branch cut was not committed".into(),
            Err(error) => self.message = error.to_string(),
        }
    }

    fn destroy_one_root(&mut self) {
        let Some((&plant_id, plant)) = self.setup.state.plants.iter().next() else {
            return;
        };
        let root = plant.root;
        match self.submit_cell_edit(root, EditKind::Cut) {
            Ok(true) => {
                destroy_root(&mut self.setup.state, plant_id);
                self.root_cut = true;
                self.message = "Root destroyed; this tree stops growing and seeding".into();
                self.refresh_geometry();
            }
            Ok(false) => self.message = "Root cut was not committed".into(),
            Err(error) => self.message = error.to_string(),
        }
    }

    fn advance_ecology(&mut self, elapsed_ms: u64) {
        if elapsed_ms == 0 {
            return;
        }
        let prior_plants = self.setup.state.plants.len();
        let prior_seeds = self.setup.state.seeds.len();
        let prior_biomass = self
            .setup
            .state
            .grass
            .get(&self.setup.grass_patch_id)
            .map_or(0, |patch| patch.biomass);
        let prior_committed = self
            .setup
            .state
            .plants
            .values()
            .map(|plant| plant.committed_cells)
            .sum::<u32>();
        let mut remaining = elapsed_ms;
        while remaining > 0 {
            let slice = remaining.min(self.setup.config.update_interval_ms.max(1));
            remaining -= slice;
            let result = update(
                &mut self.setup.state,
                &self.sim.world().terrain().volume,
                &self.setup.definitions,
                &self.setup.inputs,
                self.setup.config,
                slice,
            );
            let Ok((_, proposals)) = result else {
                self.clock.paused = true;
                self.message = "Ecology update failed; time paused".into();
                return;
            };
            for proposal in proposals {
                if !proposal_current(
                    &self.sim.world().terrain().volume,
                    &self.setup.inputs,
                    &proposal,
                ) {
                    acknowledge(&mut self.setup.state, &proposal, CommitAck::Stale);
                    continue;
                }
                let mut requests = Vec::with_capacity(proposal.cells.len());
                let mut targets_valid = true;
                for (cell, material) in &proposal.cells {
                    if !matches!(
                        self.sim.world().terrain().volume.sample(*cell),
                        Ok(Sample::Empty { .. })
                    ) {
                        targets_valid = false;
                        break;
                    }
                    let request = RequestId(self.request);
                    self.request = self.request.saturating_add(1);
                    let center = BrushPoint::from_units(
                        cell.x * BRUSH_UNIT + BRUSH_UNIT / 2,
                        cell.y * BRUSH_UNIT + BRUSH_UNIT / 2,
                        cell.z * BRUSH_UNIT + BRUSH_UNIT / 2,
                    );
                    let actor = match EntityId::new(1) {
                        Ok(actor) => actor,
                        Err(error) => {
                            self.message = error.to_string();
                            targets_valid = false;
                            break;
                        }
                    };
                    let brush = match SphereBrush::new(center, 0) {
                        Ok(brush) => brush,
                        Err(error) => {
                            self.message = error.to_string();
                            targets_valid = false;
                            break;
                        }
                    };
                    match self.sim.submit(EditIntent {
                        request_id: request,
                        actor,
                        target: EditTarget::Terrain,
                        kind: EditKind::Place(*material),
                        brush,
                        explosion: None,
                    }) {
                        Ok(_) => requests.push(request),
                        Err(error) => {
                            self.message = error.to_string();
                            targets_valid = false;
                            break;
                        }
                    }
                }
                let committed = if targets_valid && requests.len() == proposal.cells.len() {
                    match self.sim.tick() {
                        Ok(tick) => requests
                            .iter()
                            .filter(|id| {
                                tick.committed
                                    .iter()
                                    .any(|(committed_id, _)| committed_id == *id)
                            })
                            .count(),
                        Err(error) => {
                            self.message = error.to_string();
                            0
                        }
                    }
                } else {
                    0
                };
                let all_committed = committed == proposal.cells.len();
                acknowledge(
                    &mut self.setup.state,
                    &proposal,
                    if all_committed {
                        CommitAck::Accepted
                    } else {
                        CommitAck::Rejected
                    },
                );
                if all_committed {
                    self.message =
                        format!("Tree grew {} connected wood cells", proposal.cells.len());
                }
            }
        }
        self.next_tree_cell = self
            .setup
            .state
            .plants
            .values()
            .next()
            .map_or(0, |plant| plant.committed_cells as usize);
        let current_committed = self
            .setup
            .state
            .plants
            .values()
            .map(|plant| plant.committed_cells)
            .sum::<u32>();
        let current_biomass = self
            .setup
            .state
            .grass
            .get(&self.setup.grass_patch_id)
            .map_or(0, |patch| patch.biomass);
        if prior_plants != self.setup.state.plants.len()
            || prior_seeds != self.setup.state.seeds.len()
            || prior_biomass != current_biomass
            || prior_committed != current_committed
        {
            self.refresh_geometry();
        }
    }

    fn refresh_geometry(&mut self) {
        let mut instances = Vec::with_capacity(self.setup.ground_cells.len() + 128);
        for (cell, material) in &self.setup.ground_cells {
            instances.push(CubeInstance::new(
                cell_center(*cell),
                u32::from(material.0),
                [CELL_M; 3],
                CubeInstance::IDENTITY_ROTATION,
            ));
        }
        if let Some(patch) = self.setup.state.grass.get(&self.setup.grass_patch_id) {
            let count =
                usize::from(patch.biomass).saturating_mul(9) / usize::from(patch.capacity.max(1));
            for i in 0..count {
                let x = (i % 3) as i64 - 1;
                let z = (i / 3) as i64 - 1;
                let cell = GlobalCell::new(patch.anchor.x + x, patch.anchor.y, patch.anchor.z + z);
                let mut center = cell_center(cell);
                center[1] -= CELL_M * 0.48;
                instances.push(CubeInstance::new(
                    center,
                    u32::from(self.setup.grass_material.0),
                    [CELL_M * 0.8, CELL_M * 0.12, CELL_M * 0.8],
                    CubeInstance::IDENTITY_ROTATION,
                ));
            }
        }
        for plant in self.setup.state.plants.values() {
            for branch in plant.skeleton.iter().take(plant.committed_cells as usize) {
                if branch.removed
                    || !matches!(
                        self.sim.world().terrain().volume.sample(branch.cell),
                        Ok(Sample::Filled(_))
                    )
                {
                    continue;
                }
                let material = self.setup.definitions[&plant.species].wood;
                instances.push(CubeInstance::new(
                    cell_center(branch.cell),
                    u32::from(material.0),
                    [CELL_M; 3],
                    CubeInstance::IDENTITY_ROTATION,
                ));
            }
        }
        for seed in self.setup.state.seeds.values() {
            let mut center = cell_center(seed.cell);
            center[1] += CELL_M * 0.3;
            instances.push(CubeInstance::new(
                center,
                u32::from(self.setup.grass_material.0),
                [CELL_M * 0.22; 3],
                CubeInstance::IDENTITY_ROTATION,
            ));
        }
        for plant in self
            .setup
            .state
            .plants
            .values()
            .filter(|plant| plant.committed_cells == 0)
        {
            instances.push(CubeInstance::new(
                cell_center(plant.root),
                u32::from(self.setup.grass_material.0),
                [CELL_M * 0.4, CELL_M * 0.7, CELL_M * 0.4],
                CubeInstance::IDENTITY_ROTATION,
            ));
        }
        self.terrain = instances;
        self.terrain_dirty = true;
    }

    fn render(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_frame)
            .min(Duration::from_millis(100));
        self.last_frame = now;
        let ecology_ms = self.clock.advance_ms(elapsed);
        self.advance_ecology(ecology_ms);
        let mut movement = Vec3::ZERO;
        for (key, axis) in [
            (KeyCode::KeyW, Vec3::NEG_Z),
            (KeyCode::KeyS, Vec3::Z),
            (KeyCode::KeyD, Vec3::X),
            (KeyCode::KeyA, Vec3::NEG_X),
            (KeyCode::KeyE, Vec3::Y),
            (KeyCode::KeyQ, Vec3::NEG_Y),
        ] {
            if self.held.contains(&key) {
                movement += axis;
            }
        }
        if movement.length_squared() > 0.0 {
            self.camera
                .fly(movement.normalize() * CAMERA_SPEED_M_S * elapsed.as_secs_f32());
        }
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
        let plant = self.setup.state.plants.values().next();
        let status = format!(
            "{} · ecology {:.1}s · {}× · grass {} / 100 · plants {} ({:?}) · seeds {} · {}",
            if self.clock.paused { "PAUSED" } else { "LIVE" },
            self.setup.state.ecological_time_ms as f64 / 1000.0,
            self.clock.multiplier(),
            self.setup
                .state
                .grass
                .get(&self.setup.grass_patch_id)
                .map_or(0, |p| p.biomass),
            self.setup.state.plants.len(),
            plant.map(|p| p.stage),
            self.setup.state.seeds.len(),
            self.message,
        );
        if self.title_update.elapsed() > Duration::from_millis(250) {
            if let Some(window) = &self.window {
                window.set_title(&format!("Spall ecology showcase — {status}"));
            }
            self.title_update = now;
        }
        if let Err(error) = renderer.finish_frame(
            acquired,
            Some(&(self.camera.position, self.camera.forward())),
            &[],
            &[],
            Some(("ECOLOGY SHOWCASE", "Space pause · . step +1s · +/- speed 1×/4×/16× · H harvest grass · B cut branch · X destroy root · WASD/QE fly · click capture mouse · Esc release", &status)),
            None,
        ) { self.fail(event_loop, error); }
    }
}

impl ApplicationHandler for EcologyDemoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        event_loop.listen_device_events(DeviceEvents::Always);
        event_loop.set_control_flow(ControlFlow::Poll);
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Spall ecology showcase — click to capture mouse")
                .with_inner_size(PhysicalSize::new(1440, 900)),
        ) {
            Ok(window) => Arc::new(window),
            Err(error) => return self.fail(event_loop, ClientError::Gpu(error.to_string())),
        };
        let materials = materials_from_manifest(self.sim.world().materials());
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
                self.capture_cursor(false);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !self.cursor_locked => self.capture_cursor(true),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(key) = event.physical_key else {
                    return;
                };
                if event.state == ElementState::Pressed {
                    if !event.repeat {
                        match key {
                            KeyCode::Escape => self.capture_cursor(false),
                            KeyCode::Space
                            | KeyCode::Equal
                            | KeyCode::NumpadAdd
                            | KeyCode::Minus
                            | KeyCode::NumpadSubtract
                            | KeyCode::Period
                            | KeyCode::KeyH
                            | KeyCode::KeyB
                            | KeyCode::KeyX => self.act(key),
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

fn cell_center(cell: GlobalCell) -> [f32; 3] {
    [
        ((cell.x as f32) + 0.5) * CELL_M,
        ((cell.y as f32) + 0.5) * CELL_M,
        ((cell.z as f32) + 0.5) * CELL_M,
    ]
}

fn cell_m(cell: GlobalCell) -> Vec3 {
    Vec3::from_array(cell_center(cell))
}

#[cfg(test)]
mod tests {
    use super::DemoClock;
    use std::time::Duration;

    #[test]
    fn pause_and_speed_controls_produce_expected_ecological_time() {
        let mut clock = DemoClock::default();
        assert_eq!(clock.advance_ms(Duration::from_millis(16)), 16);
        assert_eq!(clock.advance_ms(Duration::from_micros(667)), 0);
        assert_eq!(clock.advance_ms(Duration::from_micros(667)), 1);
        let mut fast = DemoClock {
            speed_index: 2,
            ..DemoClock::default()
        };
        assert_eq!(fast.advance_ms(Duration::from_millis(250)), 4_000);
        let mut paused = DemoClock {
            paused: true,
            ..fast
        };
        assert_eq!(paused.advance_ms(Duration::from_secs(10)), 0);
    }
}
