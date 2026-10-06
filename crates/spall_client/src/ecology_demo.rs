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
    pub focus: GlobalCell,
    pub state: EcologyState,
    pub inputs: EcologyInputs,
    pub definitions: BTreeMap<SpeciesId, SpeciesDefinition>,
    pub config: EcologyConfig,
    pub grass_patch_id: u64,
    pub grass_material: MaterialId,
    pub foliage_material: MaterialId,
}

struct EcologyDemoState {
    state: EcologyState,
    inputs: EcologyInputs,
    definitions: BTreeMap<SpeciesId, SpeciesDefinition>,
    config: EcologyConfig,
    grass_patch_id: u64,
    grass_material: MaterialId,
    foliage_material: MaterialId,
}

/// Opens the local visualizer. Ecology time controls never change wall-clock
/// simulation, networking, or persistence state.
pub fn run_ecology_demo_window(setup: EcologyDemoSetup) -> Result<(), ClientError> {
    let event_loop = EventLoop::new()?;
    let mut app = EcologyDemoApp::new(setup)?;
    event_loop.run_app(&mut app)?;
    app.result
}

/// Bounded visual evidence from the same Simulation and instance builder as
/// the interactive window. Does not open a window or create a network session.
pub fn capture_ecology_demo(
    setup: EcologyDemoSetup,
    output: &std::path::Path,
) -> Result<(), ClientError> {
    use spall_render::{DebugView, GameRenderer, OffscreenTarget, RenderContext};
    std::fs::create_dir_all(output).map_err(|e| ClientError::Render(e.to_string()))?;
    let mut app = EcologyDemoApp::new(setup)?;
    let ctx = RenderContext::headless().map_err(|e| ClientError::Gpu(e.to_string()))?;
    let materials = materials_from_manifest(app.sim.world().materials());
    let (width, height) = (1280, 800);
    app.camera.aspect = width as f32 / height as f32;
    let target = OffscreenTarget::new(&ctx.device, width, height);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        (width, height),
        None,
    );
    let mut summary = Vec::new();
    for (name, seconds, action) in [
        ("seedling", 0, None),
        ("juvenile", 3, None),
        ("mature", 9, None),
        ("dispersal", 18, None),
        ("branch-cut", 0, Some(KeyCode::KeyB)),
        ("root-cut", 0, Some(KeyCode::KeyX)),
    ] {
        for _ in 0..seconds {
            app.advance_ecology(1000);
        }
        if let Some(key) = action {
            app.act(key);
        }
        app.refresh_geometry();
        renderer.set_terrain(&ctx.device, &ctx.queue, &app.terrain);
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ecology-evidence"),
            });
        renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &app.camera,
            &EnvironmentPreset::Daylight.environment(),
            DebugView::Shaded,
        );
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        let rgba = target
            .read_rgba(&ctx)
            .map_err(|e| ClientError::Render(e.to_string()))?;
        image::save_buffer(
            output.join(format!("{name}.png")),
            &rgba,
            width,
            height,
            image::ColorType::Rgba8,
        )
        .map_err(|e| ClientError::Render(e.to_string()))?;
        summary.push(serde_json::json!({"frame": name, "ecology_ms": app.setup.state.ecological_time_ms, "plants": app.setup.state.plants.len(), "seeds": app.setup.state.seeds.len(), "wood_cells": app.setup.state.plants.values().map(|p| p.committed_cells).sum::<u32>(), "instances": app.terrain.len(), "message": app.message}));
    }
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .map_err(|e| ClientError::Render(e.to_string()))?;
    Ok(())
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
    physics_credit: Duration,
    branch_cut: bool,
    root_cut: bool,
    message: String,
    result: Result<(), ClientError>,
}

impl EcologyDemoApp {
    fn new(setup: EcologyDemoSetup) -> Result<Self, ClientError> {
        let EcologyDemoSetup {
            world,
            focus,
            state,
            inputs,
            definitions,
            config,
            grass_patch_id,
            grass_material,
            foliage_material,
        } = setup;
        let sim = Simulation::new(SimulationConfig::new(world))
            .map_err(|error| ClientError::Render(format!("ecology simulation: {error}")))?;
        let focus = cell_m(focus) + Vec3::Y * 1.3;
        let eye = focus + Vec3::new(4.5, 2.5, 6.0);
        let camera = Camera::looking_along(eye, focus - eye, 60.0_f32.to_radians(), 16.0 / 9.0);
        let setup = EcologyDemoState {
            state,
            inputs,
            definitions,
            config,
            grass_patch_id,
            grass_material,
            foliage_material,
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
            physics_credit: Duration::ZERO,
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
                *index < plant.committed_cells as usize
                    && (branch.cell.x != plant.root.x || branch.cell.z != plant.root.z)
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
                self.message = "Branch cut; removed branch stays removed".into();
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
                // This inspection harness submits existing single-cell intents.
                // Acknowledge the accepted prefix if a later cell fails; never
                // leave submitted requests orphaned or reject already-grown wood.
                let mut accepted = proposal.clone();
                accepted.cells.clear();
                for &(cell, material) in &proposal.cells {
                    if !matches!(
                        self.sim.world().terrain().volume.sample(cell),
                        Ok(Sample::Empty { .. })
                    ) {
                        break;
                    }
                    match self.submit_cell_edit(cell, EditKind::Place(material)) {
                        Ok(true) => accepted.cells.push((cell, material)),
                        Ok(false) => break,
                        Err(error) => {
                            self.message = error.to_string();
                            break;
                        }
                    }
                }
                if !accepted.cells.is_empty() {
                    let plant = &self.setup.state.plants[&proposal.plant_id];
                    let last = accepted.cells.last().unwrap().0;
                    accepted.next_committed_cells =
                        plant.skeleton.iter().position(|b| b.cell == last).unwrap() as u32 + 1;
                    accepted.acknowledged_elapsed_ms = accepted.cells.len() as u64
                        * self.setup.definitions[&plant.species].cell_growth_ms;
                    acknowledge(&mut self.setup.state, &accepted, CommitAck::Accepted);
                    self.message = format!(
                        "Tree grew {} connected 25 cm wood cells",
                        accepted.cells.len()
                    );
                } else {
                    acknowledge(&mut self.setup.state, &proposal, CommitAck::Rejected);
                }
            }
        }
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
        self.terrain = presentation_instances(&self.sim, &self.setup);
        self.terrain_dirty = true;
    }

    fn render(&mut self, event_loop: &ActiveEventLoop, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_frame)
            .min(Duration::from_millis(100));
        self.last_frame = now;
        let ecology_ms = self.clock.advance_ms(elapsed);
        self.advance_ecology(ecology_ms);
        self.physics_credit += elapsed;
        let tick_dt = Duration::from_secs_f64(f64::from(spall_sim::TICK_DT_S));
        while self.physics_credit >= tick_dt {
            self.physics_credit -= tick_dt;
            if let Err(error) = self.sim.tick() {
                self.message = error.to_string();
                self.clock.paused = true;
                break;
            }
        }
        if self.sim.world().bodies().next().is_some() {
            self.refresh_geometry();
        }
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

// The complete live volume is drawn, including terrain beneath new plants.
// Foliage and grass are presentation instances only, never physics/topology cells.
fn presentation_instances(sim: &Simulation, setup: &EcologyDemoState) -> Vec<CubeInstance> {
    let volume = &sim.world().terrain().volume;
    let mut instances = crate::window::build_instances(volume, [8.0, 2.0, 8.0]);
    for body in sim.world().bodies() {
        let rotation = body.pose.rotation.as_quat();
        let translation = Vec3::from_array(body.pose.translation_m.map(|v| v as f32));
        for mut cube in crate::window::build_instances(&body.volume, [8.0, 2.0, 8.0]) {
            cube.offset = (rotation * Vec3::from_array(cube.offset) + translation).to_array();
            cube.rotation = rotation.to_array();
            instances.push(cube);
        }
    }
    for patch in setup.state.grass.values() {
        let def = &setup.definitions[&patch.species];
        let count = usize::from(patch.biomass) * 81 / usize::from(patch.capacity.max(1));
        for i in 0..count {
            let reference = GlobalCell::new(
                patch.anchor.x + (i % 9) as i64 - 4,
                patch.anchor.y,
                patch.anchor.z + (i / 9) as i64 - 4,
            );
            if let Some(root) = supported_root(volume, reference, def, setup) {
                for blade in 0..3 {
                    let mut center = cell_center(root);
                    let height = 0.10 + ((i + blade) % 4) as f32 * 0.025;
                    center[0] += (blade as f32 - 1.0) * 0.045;
                    center[1] = root.y as f32 * CELL_M + height * 0.5;
                    center[2] += ((i % 3) as f32 - 1.0) * 0.025;
                    instances.push(CubeInstance::new(
                        center,
                        u32::from(setup.grass_material.0),
                        [0.02, height, 0.025],
                        CubeInstance::IDENTITY_ROTATION,
                    ));
                }
            }
        }
    }
    for seed in setup.state.seeds.values() {
        if let Some(root) =
            supported_root(volume, seed.cell, &setup.definitions[&seed.species], setup)
        {
            let mut center = cell_center(root);
            center[1] = root.y as f32 * CELL_M + 0.015;
            instances.push(CubeInstance::new(
                center,
                u32::from(setup.grass_material.0),
                [0.04, 0.03, 0.04],
                CubeInstance::IDENTITY_ROTATION,
            ));
        }
    }
    for plant in setup.state.plants.values().filter(|p| p.root_alive) {
        if plant.committed_cells == 0 {
            if supported_root(
                volume,
                plant.root,
                &setup.definitions[&plant.species],
                setup,
            ) != Some(plant.root)
            {
                continue;
            }
            let mut center = cell_center(plant.root);
            center[1] = plant.root.y as f32 * CELL_M + 0.1;
            instances.push(CubeInstance::new(
                center,
                u32::from(setup.definitions[&plant.species].wood.0),
                [0.025, 0.2, 0.025],
                CubeInstance::IDENTITY_ROTATION,
            ));
            for dx in [-0.045, 0.045] {
                let mut leaf = center;
                leaf[0] += dx;
                leaf[1] += 0.04;
                instances.push(CubeInstance::new(
                    leaf,
                    u32::from(setup.foliage_material.0),
                    [0.075, 0.04, 0.06],
                    CubeInstance::IDENTITY_ROTATION,
                ));
            }
            continue;
        }
        let grown = &plant.skeleton[..(plant.committed_cells as usize).min(plant.skeleton.len())];
        for (index, branch) in grown.iter().enumerate() {
            if branch.removed
                || !matches!(volume.sample(branch.cell), Ok(Sample::Filled(m)) if m == setup.definitions[&plant.species].wood)
            {
                continue;
            }
            if grown
                .iter()
                .any(|c| !c.removed && c.parent == Some(index as u32))
            {
                continue;
            }
            let radius = if plant.committed_cells < 12 { 1 } else { 3 };
            // Rounded, slightly irregular leaf clusters attached to live tips.
            for z in -radius..=radius {
                for y in -radius..=radius {
                    for x in -radius..=radius {
                        if x * x + z * z + 2 * y * y > radius * radius + 1 {
                            continue;
                        }
                        let mut center = cell_center(branch.cell);
                        center[0] += x as f32 * 0.22;
                        center[1] += 0.3 + y as f32 * 0.22;
                        center[2] += z as f32 * 0.22;
                        let cell = GlobalCell::new(
                            (center[0] / CELL_M).floor() as i64,
                            (center[1] / CELL_M).floor() as i64,
                            (center[2] / CELL_M).floor() as i64,
                        );
                        if !matches!(volume.sample(cell), Ok(Sample::Empty { .. })) {
                            continue;
                        }
                        let width = if (x + z + index as i32).rem_euclid(3) == 0 {
                            0.18
                        } else {
                            0.22
                        };
                        instances.push(CubeInstance::new(
                            center,
                            u32::from(setup.foliage_material.0),
                            [width, 0.20, width],
                            CubeInstance::IDENTITY_ROTATION,
                        ));
                    }
                }
            }
        }
    }
    instances
}

fn supported_root(
    volume: &spall_voxel::Volume,
    reference: GlobalCell,
    def: &SpeciesDefinition,
    setup: &EcologyDemoState,
) -> Option<GlobalCell> {
    let (root, _) = spall_ecology::surface_root(volume, reference, setup.config.bounds);
    let root = root?;
    let soil = GlobalCell::new(root.x, root.y - 1, root.z);
    if !matches!(volume.sample(soil), Ok(Sample::Filled(m)) if def.soil_materials.contains(&m))
        || setup.inputs.prohibited.contains(&root)
    {
        return None;
    }
    Some(root)
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
    use super::*;
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
    fn fixture() -> EcologyDemoSetup {
        use spall_core::{CellSizeCode, VolumeId};
        use spall_voxel::{EditPlan, Volume};
        let id = VolumeId::new(1).unwrap();
        let mut volume = Volume::new(id, CellSizeCode::Quarter);
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(31, 95, 31),
                MaterialId::AIR,
            ))
            .unwrap();
        volume
            .apply_edit(&EditPlan::filled_box(
                id,
                GlobalCell::new(0, 0, 0),
                GlobalCell::new(31, 0, 31),
                MaterialId(1),
            ))
            .unwrap();
        let species = SpeciesDefinition {
            version: 1,
            id: SpeciesId(1),
            wood: MaterialId(2),
            soil_materials: [MaterialId(1); 4],
            min_moisture: 20,
            max_moisture: 220,
            min_sky_exposure: 12,
            min_spacing_cells: 8,
            seed_radius_cells: 12,
            seed_lifetime_ms: 30_000,
            seedling_ms: 3_000,
            juvenile_ms: 7_000,
            cell_growth_ms: 200,
        };
        let mut inputs = EcologyInputs::default();
        for z in 0..32 {
            for x in 0..32 {
                inputs.moisture.insert(GlobalCell::new(x, 0, z), 128);
            }
        }
        let config = EcologyConfig {
            update_interval_ms: 1000,
            max_seed_records: 0,
            max_plants: 1,
            bounds: Some((GlobalCell::new(0, 0, 0), GlobalCell::new(31, 95, 31))),
            ..Default::default()
        };
        let focus = GlobalCell::new(12, 1, 12);
        let mut state = EcologyState::default();
        spall_ecology::place_tree(&mut state, &volume, species, &inputs, config, focus).unwrap();
        let grass_patch_id = spall_ecology::place_grass_patch(
            &mut state,
            SpeciesId(1),
            GlobalCell::new(22, 1, 22),
            100,
        )
        .unwrap();
        EcologyDemoSetup {
            world: WorldSetup {
                terrain: volume,
                terrain_collider_region: config.bounds.unwrap(),
                materials: spall_sim::fixtures::stone_manifest(),
                anchor: spall_sim::fixtures::flat_terrain_setup().anchor,
                physics: spall_physics::PhysicsConfig {
                    disable_ccd: true,
                    ..Default::default()
                },
            },
            focus,
            state,
            inputs,
            definitions: BTreeMap::from([(species.id, species)]),
            config,
            grass_patch_id,
            grass_material: MaterialId(1),
            foliage_material: MaterialId(1),
        }
    }

    #[test]
    fn soft_vegetation_bases_are_on_the_soil_surface_and_holes_are_not_drawn() {
        use spall_voxel::EditPlan;
        let mut setup = fixture();
        setup.state.seeds.insert(
            9,
            spall_ecology::SeedRecord {
                id: 9,
                species: SpeciesId(1),
                cell: GlobalCell::new(6, 8, 6),
                expires_at_ms: 50_000,
            },
        );
        let app = EcologyDemoApp::new(setup).unwrap();
        let seed = app
            .terrain
            .iter()
            .find(|i| i.size == [0.04, 0.03, 0.04])
            .unwrap();
        assert!((seed.offset[1] - seed.size[1] * 0.5 - CELL_M).abs() < 1e-6);
        let stem = app
            .terrain
            .iter()
            .find(|i| i.size == [0.025, 0.2, 0.025])
            .unwrap();
        assert!((stem.offset[1] - stem.size[1] * 0.5 - CELL_M).abs() < 1e-6);
        let mut setup = fixture();
        setup
            .world
            .terrain
            .apply_edit(&EditPlan::filled_box(
                setup.world.terrain.id(),
                GlobalCell::new(6, 0, 6),
                GlobalCell::new(6, 0, 6),
                MaterialId::AIR,
            ))
            .unwrap();
        setup.state.seeds.insert(
            9,
            spall_ecology::SeedRecord {
                id: 9,
                species: SpeciesId(1),
                cell: GlobalCell::new(6, 8, 6),
                expires_at_ms: 50_000,
            },
        );
        let app = EcologyDemoApp::new(setup).unwrap();
        assert!(!app.terrain.iter().any(|i| i.size == [0.04, 0.03, 0.04]));
    }

    #[test]
    fn full_growth_has_foliage_correct_wood_volume_and_branch_cut_preserves_trunk() {
        let mut app = EcologyDemoApp::new(fixture()).unwrap();
        for _ in 0..12 {
            app.advance_ecology(1000);
        }
        let plant = app.setup.state.plants.values().next().unwrap();
        assert_eq!(plant.stage, spall_ecology::PlantStage::Mature);
        assert_eq!(plant.committed_cells as usize, plant.skeleton.len());
        let expected = plant.committed_cells as f32 * CELL_M.powi(3);
        let rendered: f32 = app
            .terrain
            .iter()
            .filter(|i| i.material == 2)
            .map(|i| i.size.iter().product::<f32>())
            .sum();
        assert!((rendered - expected).abs() < 1e-5);
        assert!(
            app.terrain
                .iter()
                .any(|i| i.size[1] == 0.20 && i.size[0] >= 0.18)
        );
        app.cut_one_branch();
        assert!(app.branch_cut);
        let root = app.setup.state.plants.values().next().unwrap().root;
        assert!(matches!(
            app.sim.world().terrain().volume.sample(root),
            Ok(Sample::Filled(MaterialId(2)))
        ));
        app.destroy_one_root();
        assert!(app.root_cut);
        assert!(!app.setup.state.plants.values().next().unwrap().root_alive);
        assert!(
            app.sim.world().bodies().next().is_some(),
            "detached wood remains a real body"
        );
        let seed_count = app.setup.state.seeds.len();
        for _ in 0..10 {
            app.advance_ecology(1000);
        }
        assert_eq!(app.setup.state.seeds.len(), seed_count);
    }
}
