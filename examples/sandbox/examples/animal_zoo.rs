//! Walkable visual showroom of authored animals; no simulated creature actors.
use glam::{Quat, Vec3, vec3};
use sandbox::animal_visual::{AnimalVisual, MAX_ANIMAL_VISUAL_CELLS, material_mapping};
use spall_core::MaterialId;
use spall_render::{
    Camera, CubeInstance, DebugView, EnvironmentPreset, GameRenderer, Material, OffscreenTarget,
    RenderContext,
};
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{CursorGrabMode, Window, WindowAttributes, WindowId},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const SIZE: (u32, u32) = (1280, 800);
const TITLE: &str = "Spall Zoo | 0 All  1 Trout  2 Crow  3 Deer | WASD move | Click to look | Space pause | Tab idle/action";
const X: [f32; 3] = [-3.8, 0.0, 3.8];

struct Specimen {
    visual: AnimalVisual,
    action: &'static str,
    center: Vec3,
    min_y: f32,
}
struct Zoo {
    animals: Vec<Specimen>,
    materials: Vec<Material>,
    tints: BTreeMap<(MaterialId, [u8; 3]), u32>,
    terrain: Vec<CubeInstance>,
    glass: Vec<CubeInstance>,
    ink: u32,
    panel: u32,
}

fn cube(at: [f32; 3], size: [f32; 3], material: u32) -> CubeInstance {
    CubeInstance::new(at, material, size, CubeInstance::IDENTITY_ROTATION)
}
fn linear(v: u8) -> f32 {
    let v = f32::from(v) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

impl Zoo {
    fn new() -> Result<Self> {
        let manifest = sandbox::game::manifest();
        let mapping = material_mapping(&manifest);
        let mut materials = spall_render::materials_from_manifest(&manifest);
        let mut tints = BTreeMap::new();
        let mut animals = Vec::new();
        for (bytes, action) in [
            (
                include_bytes!("../assets/animals/trout.spvox").as_slice(),
                "swim",
            ),
            (
                include_bytes!("../assets/animals/crow.spvox").as_slice(),
                "flap",
            ),
            (
                include_bytes!("../assets/animals/deer.spvox").as_slice(),
                "walk",
            ),
        ] {
            let asset = spall_asset::decode(bytes)?;
            for run in &asset.runs {
                if run.tint_slot == 0 {
                    continue;
                }
                let material = mapping[&asset.material_keys[usize::from(run.material_slot - 1)]];
                let tint = asset.tints_srgb[usize::from(run.tint_slot - 1)];
                tints.entry((material, tint)).or_insert_with(|| {
                    let mut value = materials[usize::from(material.0)];
                    value.base_color = tint.map(linear);
                    let index = materials.len() as u32;
                    materials.push(value);
                    index
                });
            }
            let visual = AnimalVisual::new(asset, &mapping, MAX_ANIMAL_VISUAL_CELLS)?;
            let mut lo = Vec3::splat(f32::INFINITY);
            let mut hi = Vec3::splat(f32::NEG_INFINITY);
            for cell in visual.sample("idle", 0.0)? {
                let half = cell.size * 0.5;
                let extent = (cell.rotation * Vec3::X).abs() * half.x
                    + (cell.rotation * Vec3::Y).abs() * half.y
                    + (cell.rotation * Vec3::Z).abs() * half.z;
                lo = lo.min(cell.center - extent);
                hi = hi.max(cell.center + extent);
            }
            animals.push(Specimen {
                visual,
                action,
                center: (lo + hi) * 0.5,
                min_y: lo.y,
            });
        }
        let mut add = |value| {
            let index = materials.len() as u32;
            materials.push(value);
            index
        };
        let stone = add(Material::new([0.39, 0.43, 0.44], 0.95, 0.0));
        let path = add(Material::new([0.63, 0.58, 0.45], 0.95, 0.0));
        let grass = add(Material::new([0.13, 0.32, 0.11], 0.9, 0.0).jitter(0.08));
        let wood = add(Material::new([0.25, 0.13, 0.06], 0.85, 0.0));
        let sand = add(Material::new([0.56, 0.47, 0.27], 0.95, 0.0));
        let sign = add(Material::new([0.025, 0.07, 0.065], 0.8, 0.0));
        let ink = add(Material::new([0.86, 0.88, 0.73], 0.8, 0.0).emissive(0.6));
        let panel = add(Material::new([0.009, 0.018, 0.021], 1.0, 0.0).emissive(0.4));
        let glass_mat = add(Material::new([0.2, 0.65, 0.7], 0.15, 0.0).opacity(0.12));
        let water_mat = add(Material::new([0.05, 0.31, 0.42], 0.2, 0.0).opacity(0.3));
        let mut terrain = vec![cube([0.0, -0.18, 0.0], [12.6, 0.35, 7.0], stone)];
        for ix in -8..=8 {
            for iz in 2..=4 {
                terrain.push(cube(
                    [ix as f32 * 0.72, 0.015, iz as f32 * 0.72],
                    [0.69, 0.05, 0.69],
                    path,
                ));
            }
        }
        for (i, x) in X.into_iter().enumerate() {
            terrain.push(cube([x, 0.07, 0.0], [3.3, 0.18, 2.8], wood));
            terrain.push(cube(
                [x, 0.18, 0.0],
                [3.05, 0.08, 2.55],
                if i == 0 { sand } else { grass },
            ));
            let height = if i == 1 { 2.5 } else { 0.75 };
            for dx in [-1.5, 1.5] {
                for dz in [-1.2, 1.2] {
                    terrain.push(cube(
                        [x + dx, 0.2 + height * 0.5, dz],
                        [0.09, height, 0.09],
                        wood,
                    ));
                }
            }
            if i != 0 {
                for y in if i == 1 { vec![2.65] } else { vec![0.48, 0.78] } {
                    terrain.push(cube([x, y, -1.2], [3.1, 0.07, 0.07], wood));
                    for dx in [-1.5, 1.5] {
                        terrain.push(cube([x + dx, y, 0.0], [0.07, 0.07, 2.4], wood));
                    }
                }
            }
            terrain.push(cube([x, 2.95, -0.65], [2.2, 0.48, 0.09], sign));
            let label = ["1 TROUT", "2 CROW", "3 DEER"][i];
            text(
                &mut terrain,
                label,
                vec3(x - label.len() as f32 * 0.09, 3.10, -0.59),
                Vec3::X,
                Vec3::Y,
                0.045,
                ink,
            );
        }
        // Aquarium frame and clear panels leave the fish visible from the path.
        for dx in [-1.25, 1.25] {
            for dz in [-0.8, 0.8] {
                terrain.push(cube([X[0] + dx, 0.94, dz], [0.06, 1.42, 0.06], sign));
            }
        }
        for y in [0.25, 1.65] {
            for z in [-0.8, 0.8] {
                terrain.push(cube([X[0], y, z], [2.56, 0.05, 0.05], sign));
            }
            for dx in [-1.25, 1.25] {
                terrain.push(cube([X[0] + dx, y, 0.0], [0.05, 0.05, 1.6], sign));
            }
        }
        for dx in [-0.95, 0.95] {
            terrain.push(cube([X[0] + dx, 0.48, -0.5], [0.045, 0.5, 0.045], grass));
            for side in [-1.0, 1.0] {
                terrain.push(cube(
                    [X[0] + dx + side * 0.1, 0.52, -0.5],
                    [0.22, 0.035, 0.08],
                    grass,
                ));
            }
        }
        let mut glass = vec![cube([X[0], 1.45, 0.0], [2.45, 0.015, 1.5], water_mat)];
        for z in [-0.8, 0.8] {
            glass.push(cube([X[0], 0.94, z], [2.5, 1.4, 0.015], glass_mat));
        }
        for dx in [-1.25, 1.25] {
            glass.push(cube([X[0] + dx, 0.94, 0.0], [0.015, 1.4, 1.6], glass_mat));
        }
        // Open-front aviary and a perch: the authored flying pose stays visible.
        terrain.push(cube([0.0, 0.6, -0.1], [0.12, 0.8, 0.12], wood));
        terrain.push(cube([0.0, 1.0, -0.1], [1.2, 0.09, 0.09], wood));
        for step in -3..=3 {
            terrain.push(cube(
                [step as f32 * 0.4, 1.45, -1.2],
                [0.018, 2.4, 0.018],
                sign,
            ));
        }
        terrain.push(cube([X[2] + 1.0, 0.28, -0.65], [0.45, 0.12, 0.35], stone));
        text(
            &mut terrain,
            "SPALL ZOO",
            vec3(-2.0, 0.24, 2.6),
            Vec3::X,
            Vec3::Y,
            0.055,
            ink,
        );
        Ok(Self {
            animals,
            materials,
            tints,
            terrain,
            glass,
            ink,
            panel,
        })
    }

    fn instances(&self, seconds: f64, action: bool) -> Result<Vec<CubeInstance>> {
        let mut cubes = Vec::with_capacity(638);
        for (i, animal) in self.animals.iter().enumerate() {
            let y = match i {
                0 => 0.96,
                1 if action => 1.65,
                1 => 1.05 + animal.center.y - animal.min_y,
                _ => 0.24 + animal.center.y - animal.min_y,
            };
            let translation = vec3(X[i], y, 0.0) - animal.center;
            for cell in animal
                .visual
                .sample(if action { animal.action } else { "idle" }, seconds * 60.0)?
            {
                let material = cell.tint_srgb.map_or(u32::from(cell.material.0), |tint| {
                    self.tints[&(cell.material, tint)]
                });
                cubes.push(CubeInstance::new(
                    (cell.center + translation).to_array(),
                    material,
                    cell.size.to_array(),
                    cell.rotation.to_array(),
                ));
            }
        }
        Ok(cubes)
    }

    fn upload(
        &self,
        renderer: &mut GameRenderer,
        ctx: &RenderContext,
        camera: &Camera,
        seconds: f64,
        action: bool,
        paused: bool,
    ) -> Result<()> {
        renderer.set_bodies(&ctx.device, &ctx.queue, &self.instances(seconds, action)?);
        let mut glass = self.glass.clone();
        glass.sort_by(|a, b| {
            let distance = |cube: &CubeInstance| {
                (Vec3::from_array(cube.offset) - camera.position).length_squared()
            };
            distance(b).total_cmp(&distance(a))
        });
        renderer.set_transparent_cubes(&ctx.device, &ctx.queue, &glass);
        renderer.set_overlay(&ctx.device, &ctx.queue, &self.hud(camera, action, paused));
        Ok(())
    }

    fn hud(&self, camera: &Camera, action: bool, paused: bool) -> Vec<CubeInstance> {
        let mut cubes = Vec::new();
        let right = camera.right();
        let up = camera.up();
        let forward = camera.forward();
        let anchor = camera.position + forward * 0.9
            - right * (0.9 * (camera.fov_y * 0.5).tan() * camera.aspect - 0.025)
            + up * (0.9 * (camera.fov_y * 0.5).tan() - 0.025);
        let rotation =
            (Quat::from_rotation_y(camera.yaw) * Quat::from_rotation_x(camera.pitch)).to_array();
        cubes.push(CubeInstance::new(
            (anchor + right * 0.305 - up * 0.104 + forward * 0.007).to_array(),
            self.panel,
            [0.63, 0.23, 0.002],
            rotation,
        ));
        let state = format!(
            "{}  {}",
            if paused { "PAUSED" } else { "PLAYING" },
            if action { "ACTION" } else { "IDLE" }
        );
        for (line, value) in [
            "SPALL ZOO",
            "0 ALL   1 TROUT   2 CROW   3 DEER",
            "WASD MOVE  Q/E UP/DOWN  SHIFT FAST",
            "CLICK LOOK  ESC RELEASE/EXIT",
            "SPACE PAUSE   TAB IDLE/ACTION",
            &state,
        ]
        .into_iter()
        .enumerate()
        {
            text(
                &mut cubes,
                value,
                anchor - up * line as f32 * 0.036,
                right,
                up,
                0.0045,
                self.ink,
            );
        }
        cubes
    }
}

// Tiny scene-native sign/HUD glyphs. No UI framework or font runtime needed.
fn glyph(c: char) -> [u8; 5] {
    match c {
        'A' => [2, 5, 7, 5, 5],
        'B' => [6, 5, 6, 5, 6],
        'C' => [7, 4, 4, 4, 7],
        'D' => [6, 5, 5, 5, 6],
        'E' => [7, 4, 6, 4, 7],
        'F' => [7, 4, 6, 4, 4],
        'G' => [7, 4, 5, 5, 7],
        'H' => [5, 5, 7, 5, 5],
        'I' => [7, 2, 2, 2, 7],
        'J' => [1, 1, 1, 5, 7],
        'K' => [5, 5, 6, 5, 5],
        'L' => [4, 4, 4, 4, 7],
        'M' => [5, 7, 7, 5, 5],
        'N' => [5, 7, 7, 7, 5],
        'O' => [7, 5, 5, 5, 7],
        'P' => [6, 5, 6, 4, 4],
        'Q' => [7, 5, 5, 7, 1],
        'R' => [6, 5, 6, 5, 5],
        'S' => [7, 4, 7, 1, 7],
        'T' => [7, 2, 2, 2, 2],
        'U' => [5, 5, 5, 5, 7],
        'V' => [5, 5, 5, 5, 2],
        'W' => [5, 5, 7, 7, 5],
        'X' => [5, 5, 2, 5, 5],
        'Y' => [5, 5, 2, 2, 2],
        'Z' => [7, 1, 2, 4, 7],
        '0' => [7, 5, 5, 5, 7],
        '1' => [2, 6, 2, 2, 7],
        '2' => [7, 1, 7, 4, 7],
        '3' => [7, 1, 7, 1, 7],
        '/' => [1, 1, 2, 4, 4],
        _ => [0; 5],
    }
}
fn text(
    cubes: &mut Vec<CubeInstance>,
    label: &str,
    anchor: Vec3,
    right: Vec3,
    up: Vec3,
    pixel: f32,
    material: u32,
) {
    let backward = right.cross(up);
    let rotation = Quat::from_mat3(&glam::Mat3::from_cols(right, up, backward)).to_array();
    for (letter, c) in label.chars().enumerate() {
        for (row, bits) in glyph(c).into_iter().enumerate() {
            for col in 0..3 {
                if bits & (1 << (2 - col)) != 0 {
                    let at = anchor + right * (letter as f32 * 4.0 + col as f32) * pixel
                        - up * row as f32 * pixel;
                    cubes.push(CubeInstance::new(
                        at.to_array(),
                        material,
                        [pixel * 0.88, pixel * 0.88, pixel * 0.3],
                        rotation,
                    ));
                }
            }
        }
    }
}

fn focus(index: usize, aspect: f32) -> Camera {
    let (eye, target) = match index {
        1 => (vec3(X[0] + 1.7, 1.75, 2.8), vec3(X[0], 0.95, 0.0)),
        2 => (vec3(1.7, 2.15, 2.8), vec3(0.0, 1.55, 0.0)),
        3 => (vec3(X[2] + 1.9, 1.75, 3.0), vec3(X[2], 0.8, 0.0)),
        _ => (vec3(7.6, 5.3, 10.5), vec3(0.0, 1.05, 0.1)),
    };
    Camera::looking_along(eye, target - eye, 55_f32.to_radians(), aspect)
}

struct Display {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    ctx: RenderContext,
    renderer: GameRenderer,
}
impl Display {
    fn new(window: Arc<Window>, zoo: &Zoo) -> Result<Self> {
        let mut descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone()));
        if cfg!(target_os = "windows") {
            descriptor.backends = wgpu::Backends::DX12;
        }
        let instance = wgpu::Instance::new(descriptor);
        let surface = instance.create_surface(window.clone())?;
        let (ctx, caps) = RenderContext::for_surface(&instance, &surface)?;
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .ok_or("no sRGB window format")?;
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&ctx.device, &config);
        let mut renderer = GameRenderer::new(
            &ctx.device,
            &ctx.queue,
            format,
            &zoo.materials,
            (config.width, config.height),
            None,
        );
        renderer.set_terrain(&ctx.device, &ctx.queue, &zoo.terrain);
        println!(
            "{}",
            serde_json::json!({"event":"zoo_ready","adapter":ctx.adapter_name(),"backend":format!("{:?}",ctx.backend()),"animals":["trout","crow","deer"]})
        );
        Ok(Self {
            window,
            surface,
            config,
            ctx,
            renderer,
        })
    }
    fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.ctx.device, &self.config);
        self.renderer
            .resize(&self.ctx.device, size.width, size.height);
    }
    fn draw(
        &mut self,
        zoo: &Zoo,
        camera: &Camera,
        seconds: f64,
        action: bool,
        paused: bool,
    ) -> Result<bool> {
        use wgpu::CurrentSurfaceTexture::*;
        let frame = match self.surface.get_current_texture() {
            Success(frame) => frame,
            Suboptimal(frame) => {
                drop(frame);
                self.surface.configure(&self.ctx.device, &self.config);
                return Ok(false);
            }
            Lost | Outdated => {
                self.surface.configure(&self.ctx.device, &self.config);
                return Ok(false);
            }
            Timeout | Occluded => return Ok(false),
            Validation => return Err("zoo surface validation failure".into()),
        };
        zoo.upload(
            &mut self.renderer,
            &self.ctx,
            camera,
            seconds,
            action,
            paused,
        )?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("animal-zoo"),
            });
        self.renderer.render(
            &self.ctx.device,
            &self.ctx.queue,
            &mut encoder,
            &view,
            camera,
            &EnvironmentPreset::Daylight.environment(),
            DebugView::Shaded,
        );
        self.ctx.queue.submit([encoder.finish()]);
        self.ctx.queue.present(frame);
        Ok(true)
    }
}

struct App {
    zoo: Zoo,
    display: Option<Display>,
    camera: Camera,
    held: HashSet<KeyCode>,
    locked: bool,
    action: bool,
    paused: bool,
    seconds: f64,
    last: Instant,
    frames: u32,
    limit: Option<u32>,
    resized: bool,
    result: Result<()>,
}
impl App {
    fn grab(&mut self, lock: bool) {
        if let Some(display) = &self.display {
            let window = &display.window;
            self.locked = lock
                && (window.set_cursor_grab(CursorGrabMode::Locked).is_ok()
                    || window.set_cursor_grab(CursorGrabMode::Confined).is_ok());
            if !self.locked {
                let _ = window.set_cursor_grab(CursorGrabMode::None);
            }
            window.set_cursor_visible(!self.locked);
        }
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: Box<dyn Error>) {
        self.result = Err(error);
        event_loop.exit();
    }
}
impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.display.is_some() {
            return;
        }
        let result = (|| {
            let window = Arc::new(
                event_loop.create_window(
                    WindowAttributes::default()
                        .with_title(TITLE)
                        .with_inner_size(PhysicalSize::new(SIZE.0, SIZE.1)),
                )?,
            );
            Display::new(window, &self.zoo)
        })();
        match result {
            Ok(display) => {
                self.display = Some(display);
                self.last = Instant::now();
            }
            Err(error) => self.fail(event_loop, error),
        }
    }
    fn device_event(&mut self, _: &ActiveEventLoop, _: winit::event::DeviceId, event: DeviceEvent) {
        if self.locked
            && let DeviceEvent::MouseMotion { delta } = event
        {
            self.camera
                .look(-delta.0 as f32 * 0.0025, -delta.1 as f32 * 0.0025);
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(display) = &mut self.display {
                    display.resize(size);
                    if size.height > 0 {
                        self.camera.aspect = size.width as f32 / size.height as f32;
                    }
                    self.resized |= size.width == 960;
                }
            }
            WindowEvent::Focused(false) => {
                self.held.clear();
                self.grab(false);
                self.last = Instant::now();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.grab(true),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(key) = event.physical_key {
                    if event.state == ElementState::Released {
                        self.held.remove(&key);
                        return;
                    }
                    self.held.insert(key);
                    if event.repeat {
                        return;
                    }
                    match key {
                        KeyCode::Escape if self.locked => self.grab(false),
                        KeyCode::Escape => event_loop.exit(),
                        KeyCode::Space => self.paused = !self.paused,
                        KeyCode::Tab => {
                            self.action = !self.action;
                            self.seconds = 0.0;
                        }
                        KeyCode::Digit0 => self.camera = focus(0, self.camera.aspect),
                        KeyCode::Digit1 => self.camera = focus(1, self.camera.aspect),
                        KeyCode::Digit2 => self.camera = focus(2, self.camera.aspect),
                        KeyCode::Digit3 => self.camera = focus(3, self.camera.aspect),
                        _ => {}
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                let dt = self.last.elapsed().as_secs_f32().min(0.05);
                self.last = Instant::now();
                if !self.paused {
                    self.seconds += f64::from(dt);
                }
                let axis =
                    |a, b| f32::from(self.held.contains(&a)) - f32::from(self.held.contains(&b));
                let input = vec3(
                    axis(KeyCode::KeyD, KeyCode::KeyA),
                    axis(KeyCode::KeyE, KeyCode::KeyQ),
                    axis(KeyCode::KeyW, KeyCode::KeyS),
                );
                let speed = if self.held.contains(&KeyCode::ShiftLeft) {
                    5.0
                } else {
                    2.2
                };
                self.camera.fly(input.normalize_or_zero() * speed * dt);
                self.camera.position = self
                    .camera
                    .position
                    .clamp(vec3(-12.0, 0.3, -8.0), vec3(12.0, 12.0, 14.0));
                if self.limit.is_some() {
                    if self.frames == 10
                        && let Some(display) = &self.display
                    {
                        let _ = display
                            .window
                            .request_inner_size(PhysicalSize::new(960, 600));
                    }
                    if [20, 40, 60, 80].contains(&self.frames) {
                        self.camera = focus((self.frames / 20) as usize % 4, self.camera.aspect);
                    }
                    if self.frames == 30 || self.frames == 35 {
                        self.paused = !self.paused;
                    }
                    if self.frames == 45 || self.frames == 55 {
                        self.action = !self.action;
                        self.seconds = 0.0;
                    }
                }
                if let Some(display) = &mut self.display {
                    if display.window.inner_size().width == 0
                        || display.window.inner_size().height == 0
                    {
                        return;
                    }
                    match display.draw(
                        &self.zoo,
                        &self.camera,
                        self.seconds,
                        self.action,
                        self.paused,
                    ) {
                        Ok(true) => self.frames += 1,
                        Ok(false) => {}
                        Err(error) => {
                            self.fail(event_loop, error);
                            return;
                        }
                    }
                }
                if self.limit.is_some_and(|limit| self.frames >= limit) {
                    println!(
                        "{}",
                        serde_json::json!({"event":"zoo_smoke_complete","presented_frames":self.frames,"resized":self.resized,"animation_seconds":self.seconds})
                    );
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + std::time::Duration::from_millis(8),
        ));
        if let Some(display) = &self.display {
            display.window.request_redraw();
        }
    }
}

fn capture(zoo: &Zoo, output: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&output)?;
    let ctx = RenderContext::headless()?;
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &zoo.materials,
        SIZE,
        None,
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &zoo.terrain);
    let mut reports = Vec::new();
    for (view, name) in ["overview", "trout", "crow", "deer"]
        .into_iter()
        .enumerate()
    {
        for (phase, (seconds, action)) in [(0.25, true), (0.75, true), (0.37, false)]
            .into_iter()
            .enumerate()
        {
            let camera = focus(view, SIZE.0 as f32 / SIZE.1 as f32);
            zoo.upload(&mut renderer, &ctx, &camera, seconds, action, false)?;
            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("zoo-evidence"),
                });
            renderer.render(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                target.color_view(),
                &camera,
                &EnvironmentPreset::Daylight.environment(),
                DebugView::Shaded,
            );
            target.copy_to_readback(&mut encoder);
            ctx.queue.submit([encoder.finish()]);
            let rgba = target.read_rgba(&ctx)?;
            let file = format!("{name}-{phase}.png");
            image::save_buffer(
                output.join(&file),
                &rgba,
                SIZE.0,
                SIZE.1,
                image::ColorType::Rgba8,
            )?;
            reports.push(serde_json::json!({"file":file,"seconds":seconds,"action":action,"view":name,"animal_cells":zoo.instances(seconds,action)?.len()}));
        }
    }
    std::fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"schema":1,"adapter":ctx.adapter_name(),"backend":format!("{:?}",ctx.backend()),"mode":"authored animation showroom","frames":reports}),
        )?,
    )?;
    println!(
        "{}",
        serde_json::json!({"event":"zoo_captured","frames":reports.len(),"out":output})
    );
    Ok(())
}

fn main() -> Result<()> {
    let mut output = None;
    let mut limit = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capture" => output = Some(PathBuf::from(".local/animal-zoo")),
            "--out" => output = Some(args.next().ok_or("--out needs a directory")?.into()),
            "--frames" => {
                let count = args
                    .next()
                    .ok_or("--frames needs a count")?
                    .parse::<u32>()?;
                if !(1..=600).contains(&count) {
                    return Err("frame count must be 1..600".into());
                }
                limit = Some(count);
            }
            "--help" | "-h" => {
                println!(
                    "animal-zoo [--capture --out DIRECTORY] [--frames 90]\n0 overview; 1 trout; 2 crow; 3 deer; WASD move; Q/E up/down; Shift faster; click mouse look; Space pause; Tab idle/action; Esc release/exit."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    let zoo = Zoo::new()?;
    if let Some(output) = output {
        return capture(&zoo, output);
    }
    let event_loop = EventLoop::new()?;
    let mut app = App {
        zoo,
        display: None,
        camera: focus(0, SIZE.0 as f32 / SIZE.1 as f32),
        held: HashSet::new(),
        locked: false,
        action: true,
        paused: false,
        seconds: 0.0,
        last: Instant::now(),
        frames: 0,
        limit,
        resized: false,
        result: Ok(()),
    };
    event_loop.run_app(&mut app)?;
    app.result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entire_animation_stays_inside_each_exhibit_with_exact_source_cells() {
        let zoo = Zoo::new().unwrap();
        for action in [false, true] {
            for step in 0..=120 {
                let cells = zoo.instances(f64::from(step) / 60.0, action).unwrap();
                assert_eq!(cells.len(), 136 + 217 + 285);
                for (i, range) in [0..136, 136..353, 353..638].into_iter().enumerate() {
                    let (half_x, half_z, bottom, top) = if i == 0 {
                        (1.2, 0.75, 0.27, 1.44)
                    } else {
                        (1.4, 1.1, 0.05, 2.6)
                    };
                    for cell in &cells[range] {
                        assert_eq!(cell.size, [0.0625; 3]);
                        assert!((cell.material as usize) < zoo.materials.len());
                        let center = Vec3::from_array(cell.offset);
                        let rotation = Quat::from_array(cell.rotation);
                        assert!(center.is_finite() && rotation.is_finite());
                        assert!((rotation.length() - 1.0).abs() < 1e-5);
                        for x in [-0.03125, 0.03125] {
                            for y in [-0.03125, 0.03125] {
                                for z in [-0.03125, 0.03125] {
                                    let p = center + rotation * vec3(x, y, z);
                                    assert!((p.x - X[i]).abs() < half_x && p.z.abs() < half_z);
                                    assert!(p.y >= bottom && p.y <= top, "animal {i}: {p:?}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn each_action_animates_and_idle_can_be_sampled_repeatably() {
        let zoo = Zoo::new().unwrap();
        let first = zoo.instances(0.0, true).unwrap();
        for range in [0..136, 136..353, 353..638] {
            let changes = (1..60).any(|step| {
                let next = zoo.instances(f64::from(step) / 60.0, true).unwrap();
                range.clone().any(|index| {
                    first[index].offset != next[index].offset
                        || first[index].rotation != next[index].rotation
                })
            });
            assert!(changes, "action must animate every specimen");
        }
        let idle = zoo.instances(0.37, false).unwrap();
        let repeated = zoo.instances(0.37, false).unwrap();
        for (a, b) in idle.iter().zip(repeated) {
            assert_eq!(a.offset, b.offset);
            assert_eq!(a.rotation, b.rotation);
        }
        // Switching to idle lowers the crow from the flight display to its perch.
        let flying = zoo.instances(0.37, true).unwrap();
        assert!(idle[136].offset[1] < flying[136].offset[1]);
    }
}
