//! Native MVP shell. The editor owns only panels and document interactions;
//! its wgpu device/queue are acquired from `spall_render::RenderContext`.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use egui_wgpu::{Renderer, ScreenDescriptor};
use egui_winit::State as EguiWinitState;
use serde::{Deserialize, Serialize};
use spall_editor::{
    AssetId, EditorCommand, EditorEntityId, EditorModel, UndoStack, VoxelAssetFile, VoxelBounds,
    VoxelChange, VoxelCoord, VoxelState,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

fn main() {
    let event_loop = EventLoop::new().expect("create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = EditorApp::default();
    event_loop.run_app(&mut app).expect("run editor event loop");
}

struct EngineGpu {
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    context: spall_render::RenderContext,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

impl EngineGpu {
    fn new(window: Arc<Window>) -> Result<Self, String> {
        // Keep the editor on the same native backend policy as Spall's
        // renderer. `InstanceDescriptor::default()` can select a compatible
        // software/downlevel adapter first on Windows; those often expose a
        // tiny 2048-pixel texture limit despite a much stronger D3D12 GPU.
        let mut instance_descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone()));
        instance_descriptor.backends = if cfg!(target_os = "windows") {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::all()
        };
        let instance = wgpu::Instance::new(instance_descriptor);
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| error.to_string())?;
        let (context, capabilities) = spall_render::RenderContext::for_surface(&instance, &surface)
            .map_err(|error| error.to_string())?;
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(capabilities.formats[0]);
        let requested_size = window.inner_size();
        let size = fit_surface_size(
            requested_size,
            context.device.limits().max_texture_dimension_2d,
        );
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities.alpha_modes[0],
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&context.device, &config);
        let renderer = Renderer::new(
            &context.device,
            format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                ..Default::default()
            },
        );
        Ok(Self {
            _instance: instance,
            surface,
            context,
            config,
            renderer,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> bool {
        if size.width == 0 || size.height == 0 {
            return false;
        }
        let fitted = fit_surface_size(size, self.context.device.limits().max_texture_dimension_2d);
        self.config.width = fitted.width;
        self.config.height = fitted.height;
        self.surface.configure(&self.context.device, &self.config);
        fitted != size
    }
}

/// Scales an oversized render target down without distorting its aspect ratio.
/// The native window is never resized: a maximized editor still fills the
/// display, while the compositor scales this safe target to its client area.
fn fit_surface_size(size: PhysicalSize<u32>, maximum: u32) -> PhysicalSize<u32> {
    let maximum = maximum.max(1);
    let width = size.width.max(1);
    let height = size.height.max(1);
    let scale = (f64::from(maximum) / f64::from(width))
        .min(f64::from(maximum) / f64::from(height))
        .min(1.0);
    PhysicalSize::new(
        (f64::from(width) * scale).floor().max(1.0) as u32,
        (f64::from(height) * scale).floor().max(1.0) as u32,
    )
}

struct EditorApp {
    window: Option<Arc<Window>>,
    gpu: Option<EngineGpu>,
    egui_context: egui::Context,
    egui_state: Option<EguiWinitState>,
    model: Option<EditorModel>,
    undo: UndoStack,
    selected_entity: Option<EditorEntityId>,
    selected_asset: Option<AssetId>,
    new_project_name: String,
    project_path: String,
    asset_name: String,
    spvox_path: String,
    voxel_cell: [i32; 3],
    selected_material: u16,
    selected_color: [u8; 3],
    jitter_enabled: bool,
    jitter_margin: u8,
    box_min: [i32; 3],
    box_max: [i32; 3],
    asset_bounds_min: [i32; 3],
    asset_bounds_max: [i32; 3],
    asset_tool: AssetTool,
    asset_cursor: VoxelCoord,
    cube_extent: u8,
    asset_camera: AssetCamera,
    viewport_zoom: f32,
    workspace: Workspace,
    show_hierarchy: bool,
    show_inspector: bool,
    show_toolbox: bool,
    toolbox_search: String,
    status: String,
    recent_projects: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workspace {
    Scene,
    Asset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetTool {
    Cursor,
    AddVoxel,
    AddCube,
    Chip,
}

impl AssetTool {
    const fn label(self) -> &'static str {
        match self {
            Self::Cursor => "Cursor",
            Self::AddVoxel => "Add voxel",
            Self::AddCube => "Add cube",
            Self::Chip => "Chip voxels",
        }
    }

    const fn icon(self) -> &'static str {
        match self {
            Self::Cursor => "◎",
            Self::AddVoxel => "＋",
            Self::AddCube => "□",
            Self::Chip => "⌫",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AssetCamera {
    zoom: f32,
    pan: Vec2,
    yaw: f32,
    pitch: f32,
}

impl Default for AssetCamera {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
            yaw: std::f32::consts::FRAC_PI_4,
            pitch: 0.62,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AssetProjection {
    screen_center: Pos2,
    cell_pixels: f32,
    yaw: f32,
    pitch: f32,
    world_center: [f32; 3],
}

impl AssetProjection {
    fn project(self, cell: VoxelCoord) -> Pos2 {
        let dx = cell.x as f32 - self.world_center[0];
        let dy = cell.y as f32 - self.world_center[1];
        let dz = cell.z as f32 - self.world_center[2];
        let horizontal = dx * self.yaw.cos() - dz * self.yaw.sin();
        let depth = dx * self.yaw.sin() + dz * self.yaw.cos();
        let vertical = dy * self.pitch.cos() + depth * self.pitch.sin();
        Pos2::new(
            self.screen_center.x + horizontal * self.cell_pixels,
            self.screen_center.y - vertical * self.cell_pixels,
        )
    }

    fn cell_at(self, pointer: Pos2, y: i32) -> VoxelCoord {
        let horizontal = (pointer.x - self.screen_center.x) / self.cell_pixels;
        let vertical = (self.screen_center.y - pointer.y) / self.cell_pixels;
        let dy = y as f32 - self.world_center[1];
        let sine = if self.pitch.sin().is_sign_negative() {
            self.pitch.sin().min(-0.12)
        } else {
            self.pitch.sin().max(0.12)
        };
        let depth = (vertical - dy * self.pitch.cos()) / sine;
        let dx = horizontal * self.yaw.cos() + depth * self.yaw.sin();
        let dz = -horizontal * self.yaw.sin() + depth * self.yaw.cos();
        VoxelCoord {
            x: (dx + self.world_center[0]).round() as i32,
            y,
            z: (dz + self.world_center[2]).round() as i32,
        }
    }

    fn depth(self, cell: VoxelCoord) -> f32 {
        let dx = cell.x as f32 - self.world_center[0];
        let dy = cell.y as f32 - self.world_center[1];
        let dz = cell.z as f32 - self.world_center[2];
        let horizontal_depth = dx * self.yaw.sin() + dz * self.yaw.cos();
        horizontal_depth * self.pitch.cos() - dy * self.pitch.sin()
    }
}

struct AssetViewportOutput {
    response: egui::Response,
    projection: AssetProjection,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentProjects {
    roots: Vec<PathBuf>,
}

impl Default for EditorApp {
    fn default() -> Self {
        let recent_projects = std::fs::read_to_string(recent_projects_path())
            .ok()
            .and_then(|text| ron::from_str::<RecentProjects>(&text).ok())
            .map_or_else(Vec::new, |state| state.roots);
        Self {
            window: None,
            gpu: None,
            egui_context: egui::Context::default(),
            egui_state: None,
            model: None,
            undo: UndoStack::default(),
            selected_entity: None,
            selected_asset: None,
            new_project_name: String::new(),
            project_path: String::new(),
            asset_name: String::new(),
            spvox_path: String::new(),
            voxel_cell: [0; 3],
            selected_material: 1,
            selected_color: [108, 112, 120],
            jitter_enabled: false,
            jitter_margin: 8,
            box_min: [0; 3],
            box_max: [1; 3],
            asset_bounds_min: [0; 3],
            asset_bounds_max: [7; 3],
            asset_tool: AssetTool::Cursor,
            asset_cursor: VoxelCoord { x: 0, y: 0, z: 0 },
            cube_extent: 2,
            asset_camera: AssetCamera::default(),
            viewport_zoom: 1.0,
            workspace: Workspace::Scene,
            show_hierarchy: true,
            show_inspector: true,
            show_toolbox: true,
            toolbox_search: String::new(),
            status: String::new(),
            recent_projects,
        }
    }
}

fn recent_projects_path() -> PathBuf {
    PathBuf::from(".local/spall-editor/recent.ron")
}

impl EditorApp {
    fn select_asset(&mut self, asset: AssetId) {
        self.selected_asset = Some(asset);
        let Some(bounds) = self
            .model
            .as_ref()
            .and_then(|model| model.project.asset_database.assets.get(&asset))
            .and_then(|record| record.authoring_bounds)
        else {
            return;
        };
        self.asset_bounds_min = [bounds.min.x, bounds.min.y, bounds.min.z];
        self.asset_bounds_max = [bounds.max.x, bounds.max.y, bounds.max.z];
        self.asset_cursor = bounds.min;
        self.asset_camera = AssetCamera::default();
    }

    fn bounds_from_inputs(&self) -> VoxelBounds {
        VoxelBounds::new(
            VoxelCoord {
                x: self.asset_bounds_min[0],
                y: self.asset_bounds_min[1],
                z: self.asset_bounds_min[2],
            },
            VoxelCoord {
                x: self.asset_bounds_max[0],
                y: self.asset_bounds_max[1],
                z: self.asset_bounds_max[2],
            },
        )
    }

    fn authoring_bounds(&self, asset: AssetId) -> Option<VoxelBounds> {
        self.model
            .as_ref()
            .and_then(|model| model.project.asset_database.assets.get(&asset))
            .and_then(|record| record.authoring_bounds)
    }

    fn create_project(&mut self) {
        let root = if self.project_path.trim().is_empty() {
            PathBuf::from(".local/editor-project")
        } else {
            PathBuf::from(self.project_path.trim())
        };
        let name = if self.new_project_name.trim().is_empty() {
            "Untitled"
        } else {
            self.new_project_name.trim()
        };
        let model = EditorModel::new(root, name);
        match model.save_all() {
            Ok(()) => {
                self.remember_project(&model.root);
                self.status = "Created project.ron and scenes/main.ron".into();
                self.model = Some(model);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn open_project(&mut self) {
        match EditorModel::load(PathBuf::from(self.project_path.trim())) {
            Ok(model) => {
                self.remember_project(&model.root);
                self.status = format!("Opened {}", model.project.name);
                self.model = Some(model);
                self.undo = UndoStack::default();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn remember_project(&mut self, root: &std::path::Path) {
        self.recent_projects.retain(|candidate| candidate != root);
        self.recent_projects.insert(0, root.to_path_buf());
        self.recent_projects.truncate(8);
        let path = recent_projects_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = ron::ser::to_string_pretty(
            &RecentProjects {
                roots: self.recent_projects.clone(),
            },
            ron::ser::PrettyConfig::default(),
        ) {
            let _ = std::fs::write(path, text);
        }
    }

    fn execute(&mut self, command: EditorCommand) {
        if let Some(model) = self.model.as_mut()
            && let Err(error) = self.undo.execute(model, command)
        {
            self.status = error.to_string();
        }
    }

    fn save(&mut self) {
        if let Some(model) = &self.model {
            self.status = match model.save_all() {
                Ok(()) => "Saved project, scene, and voxel assets".into(),
                Err(error) => error.to_string(),
            };
        }
    }

    fn run_scene(&mut self) {
        self.save();
        let Some(model) = &self.model else {
            return;
        };
        let scene = model.root.join("scenes/main.ron");
        let result = std::process::Command::new("cargo")
            .args([
                "run",
                "-p",
                "sandbox",
                "--features",
                "client",
                "--bin",
                "sandbox-client",
                "--",
                "--offline",
            ])
            .env("SPALL_EDITOR_SCENE", &scene)
            .spawn();
        self.status = match result {
            Ok(_) => format!("Launched sandbox runtime for {}", scene.display()),
            Err(error) => format!("Could not launch sandbox runtime: {error}"),
        };
    }

    fn draw(&mut self, ctx: &mut egui::Ui) {
        egui::Panel::top("menu").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.save();
                }
                if ui
                    .add_enabled(self.undo.can_undo(), egui::Button::new("Undo"))
                    .clicked()
                    && let Some(model) = self.model.as_mut()
                {
                    let _ = self.undo.undo(model);
                }
                if ui
                    .add_enabled(self.undo.can_redo(), egui::Button::new("Redo"))
                    .clicked()
                    && let Some(model) = self.model.as_mut()
                {
                    let _ = self.undo.redo(model);
                }
                if ui.button("Run Scene").clicked() {
                    self.run_scene();
                }
                ui.separator();
                ui.selectable_value(&mut self.workspace, Workspace::Scene, "Scene");
                ui.selectable_value(&mut self.workspace, Workspace::Asset, "Assets");
                ui.separator();
                ui.checkbox(&mut self.show_hierarchy, "Hierarchy");
                ui.checkbox(&mut self.show_inspector, "Inspector");
                ui.checkbox(&mut self.show_toolbox, "Toolbox");
                ui.separator();
                ui.label(&self.status);
            });
        });
        if self.model.is_none() {
            self.draw_launcher(ctx);
        } else {
            self.draw_editor(ctx);
        }
    }

    fn draw_launcher(&mut self, ctx: &mut egui::Ui) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.heading("Spall Editor");
                ui.label("A small native shell for authored projects and voxel assets.");
                ui.add_space(16.0);
                ui.label("Project folder");
                ui.text_edit_singleline(&mut self.project_path);
                ui.label("New project name");
                ui.text_edit_singleline(&mut self.new_project_name);
                ui.horizontal(|ui| {
                    if ui.button("New Project").clicked() {
                        self.create_project();
                    }
                    if ui.button("Open Project").clicked() {
                        self.open_project();
                    }
                });
                ui.add_space(20.0);
                if !self.recent_projects.is_empty() {
                    ui.label("Recent projects");
                    for root in self.recent_projects.clone() {
                        if ui.button(root.display().to_string()).clicked() {
                            self.project_path = root.display().to_string();
                            self.open_project();
                        }
                    }
                }
            });
        });
    }

    fn draw_editor(&mut self, ctx: &mut egui::Ui) {
        match self.workspace {
            Workspace::Scene => self.draw_scene_workspace(ctx),
            Workspace::Asset => self.draw_asset_workspace(ctx),
        }
    }

    fn draw_scene_workspace(&mut self, ctx: &mut egui::Ui) {
        if self.show_toolbox {
            egui::Panel::bottom("asset-toolbox")
                .resizable(true)
                .default_size(180.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Asset toolbox");
                        if ui.small_button("Hide ⌄").clicked() {
                            self.show_toolbox = false;
                        }
                    });
                    self.draw_asset_toolbox(ui);
                });
        }
        if self.show_hierarchy {
            egui::Panel::left("hierarchy")
                .resizable(true)
                .show(ctx, |ui| self.draw_hierarchy(ui));
        }
        if self.show_inspector {
            egui::Panel::right("inspector")
                .resizable(true)
                .show(ctx, |ui| self.draw_inspector(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.draw_viewport(ui));
    }

    fn draw_hierarchy(&mut self, ui: &mut egui::Ui) {
        ui.heading("Hierarchy");
        ui.label("Objects placed in this scene");
        let entities: Vec<_> = self
            .model
            .as_ref()
            .expect("editor model")
            .scene
            .entities
            .values()
            .cloned()
            .collect();
        for entity in entities {
            if ui
                .selectable_label(self.selected_entity == Some(entity.id), &entity.name)
                .clicked()
            {
                self.selected_entity = Some(entity.id);
            }
        }
        if ui.button("+ Empty Object").clicked() {
            let entity = self
                .model
                .as_mut()
                .expect("editor model")
                .scene
                .new_entity("Object");
            self.selected_entity = Some(entity.id);
            self.execute(EditorCommand::CreateEntity { entity });
        }
    }

    fn draw_asset_toolbox(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Toolbox");
            ui.label("Search assets to place into this scene");
            ui.text_edit_singleline(&mut self.toolbox_search);
        });
        let search = self.toolbox_search.to_lowercase();
        let assets: Vec<_> = self
            .model
            .as_ref()
            .expect("editor model")
            .project
            .asset_database
            .assets
            .values()
            .filter(|asset| search.is_empty() || asset.name.to_lowercase().contains(&search))
            .cloned()
            .collect();
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for record in assets {
                    ui.group(|ui| {
                        ui.label(&record.name);
                        if let Some(asset) = self
                            .model
                            .as_ref()
                            .and_then(|model| model.voxel_assets.get(&record.id))
                        {
                            draw_asset_preview(ui, asset, 86.0);
                        }
                        if ui.button("Place").clicked() {
                            let mut entity = self
                                .model
                                .as_mut()
                                .expect("editor model")
                                .scene
                                .new_entity(&record.name);
                            entity.voxel_asset = Some(record.id);
                            self.selected_entity = Some(entity.id);
                            self.execute(EditorCommand::CreateEntity { entity });
                        }
                    });
                }
            });
        });
    }

    fn draw_asset_workspace(&mut self, ctx: &mut egui::Ui) {
        if self.show_hierarchy {
            egui::Panel::left("asset-library")
                .resizable(true)
                .show(ctx, |ui| self.draw_asset_library(ui));
        }
        if self.show_inspector {
            egui::Panel::right("asset-brush")
                .resizable(true)
                .show(ctx, |ui| self.draw_brush_controls(ui));
        }
        egui::CentralPanel::default().show(ctx, |ui| self.draw_asset_editor(ui));
    }

    fn draw_asset_library(&mut self, ui: &mut egui::Ui) {
        ui.heading("Voxel Assets");
        ui.label("Reusable objects; placing one never edits the scene asset itself.");
        let assets: Vec<_> = self
            .model
            .as_ref()
            .expect("editor model")
            .project
            .asset_database
            .assets
            .values()
            .cloned()
            .collect();
        for record in assets {
            if ui
                .selectable_label(self.selected_asset == Some(record.id), &record.name)
                .clicked()
            {
                self.select_asset(record.id);
            }
        }
        ui.separator();
        ui.text_edit_singleline(&mut self.asset_name);
        if ui.button("+ New Voxel Asset").clicked() {
            let name = if self.asset_name.trim().is_empty() {
                "Voxel Asset"
            } else {
                self.asset_name.trim()
            };
            let command = self
                .model
                .as_ref()
                .expect("editor model")
                .new_voxel_asset_command(name);
            if let EditorCommand::CreateVoxelAsset { record, .. } = &command {
                self.select_asset(record.id);
            }
            self.execute(command);
        }
        ui.separator();
        ui.label("SPVX import / export path");
        ui.text_edit_singleline(&mut self.spvox_path);
        ui.horizontal(|ui| {
            if ui.button("Import .spvox").clicked() {
                let source = PathBuf::from(self.spvox_path.trim());
                let command = self
                    .model
                    .as_ref()
                    .expect("editor model")
                    .import_spvox_command(&source);
                match command {
                    Ok(command) => {
                        if let EditorCommand::CreateVoxelAsset { record, .. } = &command {
                            self.select_asset(record.id);
                        }
                        self.execute(command);
                        self.status = format!("Imported {}", source.display());
                    }
                    Err(error) => self.status = error.to_string(),
                }
            }
            if ui
                .add_enabled(
                    self.selected_asset.is_some(),
                    egui::Button::new("Export .spvox"),
                )
                .clicked()
                && let (Some(asset), false) =
                    (self.selected_asset, self.spvox_path.trim().is_empty())
            {
                let destination = PathBuf::from(self.spvox_path.trim());
                self.status = match self
                    .model
                    .as_ref()
                    .expect("editor model")
                    .export_spvox_asset(asset, &destination)
                {
                    Ok(()) => format!("Exported {}", destination.display()),
                    Err(error) => error.to_string(),
                };
            }
        });
    }

    fn draw_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Inspector");
        let Some(id) = self.selected_entity else {
            ui.label("Select an entity");
            return;
        };
        let Some(entity) = self
            .model
            .as_ref()
            .and_then(|m| m.scene.entities.get(&id))
            .cloned()
        else {
            return;
        };
        ui.label(format!("{}", entity.id));
        let mut after = entity.transform;
        ui.label("Transform");
        for (label, value) in [
            ("Position", &mut after.translation),
            ("Rotation", &mut after.rotation_degrees),
            ("Scale", &mut after.scale),
        ] {
            ui.horizontal(|ui| {
                ui.label(label);
                for axis in value {
                    ui.add(egui::DragValue::new(axis).speed(0.05));
                }
            });
        }
        if after != entity.transform {
            self.execute(EditorCommand::SetTransform {
                entity: id,
                before: entity.transform,
                after,
            });
        }
        ui.separator();
        ui.label("Voxel asset");
        let mut assigned = entity.voxel_asset;
        egui::ComboBox::from_id_salt("entity-asset")
            .selected_text(assigned.map_or("None".into(), |asset| asset.to_string()))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut assigned, None, "None");
                for record in self
                    .model
                    .as_ref()
                    .unwrap()
                    .project
                    .asset_database
                    .assets
                    .values()
                {
                    ui.selectable_value(&mut assigned, Some(record.id), &record.name);
                }
            });
        if assigned != entity.voxel_asset {
            self.execute(EditorCommand::SetEntityAsset {
                entity: id,
                before: entity.voxel_asset,
                after: assigned,
            });
        }
    }

    fn draw_viewport(&mut self, ui: &mut egui::Ui) {
        ui.heading("3D Viewport");
        ui.label(
            "Use the normal window maximize button; hover here and use the mouse wheel to zoom.",
        );
        let available = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(available, egui::Sense::hover());
        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.viewport_zoom = (self.viewport_zoom * (scroll * 0.002).exp()).clamp(0.2, 5.0);
            }
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(18, 29, 43));
        let center = rect.center();
        for i in -8..=8 {
            let t = i as f32 * 22.0 * self.viewport_zoom;
            painter.line_segment(
                [
                    Pos2::new(center.x + t, center.y - 150.0 * self.viewport_zoom),
                    Pos2::new(center.x + t * 0.55, center.y + 140.0 * self.viewport_zoom),
                ],
                Stroke::new(1.0, Color32::from_gray(55)),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x - 180.0 * self.viewport_zoom, center.y + t * 0.42),
                    Pos2::new(center.x + 180.0 * self.viewport_zoom, center.y + t * 0.42),
                ],
                Stroke::new(1.0, Color32::from_gray(55)),
            );
        }
        if let Some(model) = &self.model {
            for entity in model.scene.entities.values() {
                let point = Pos2::new(
                    center.x + entity.transform.translation[0] * 28.0 * self.viewport_zoom,
                    center.y
                        - entity.transform.translation[2] * 20.0 * self.viewport_zoom
                        - entity.transform.translation[1] * 24.0 * self.viewport_zoom,
                );
                let selected = self.selected_entity == Some(entity.id);
                painter.circle_filled(
                    point,
                    if selected { 10.0 } else { 7.0 },
                    if selected {
                        Color32::YELLOW
                    } else {
                        Color32::from_rgb(91, 170, 240)
                    },
                );
                painter.text(
                    point + Vec2::new(10.0, -10.0),
                    egui::Align2::LEFT_BOTTOM,
                    &entity.name,
                    egui::FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }
        }
    }

    fn draw_brush_controls(&mut self, ui: &mut egui::Ui) {
        ui.heading("Voxel Brush");
        if let Some(asset) = self.selected_asset {
            ui.label("Build volume");
            ui.horizontal(|ui| {
                ui.label("Min");
                for axis in &mut self.asset_bounds_min {
                    ui.add(egui::DragValue::new(axis));
                }
            });
            ui.horizontal(|ui| {
                ui.label("Max");
                for axis in &mut self.asset_bounds_max {
                    ui.add(egui::DragValue::new(axis));
                }
            });
            let before = self.authoring_bounds(asset);
            let after = self.bounds_from_inputs();
            ui.horizontal(|ui| {
                if ui.button("Set Bounds").clicked() {
                    self.execute(EditorCommand::SetAssetBounds {
                        asset,
                        before,
                        after: Some(after),
                    });
                }
                if ui
                    .add_enabled(before.is_some(), egui::Button::new("Clear Bounds"))
                    .clicked()
                {
                    self.execute(EditorCommand::SetAssetBounds {
                        asset,
                        before,
                        after: None,
                    });
                }
            });
            match before.and_then(|bounds| bounds.cell_count()) {
                Some(cells) => ui.label(format!("Active: {cells} cells")),
                None => ui.colored_label(Color32::YELLOW, "Set a bounded build volume first."),
            };
            ui.separator();
        } else {
            ui.label("Select an asset to define its build volume.");
            ui.separator();
        }
        ui.label("Material ID");
        ui.add(egui::DragValue::new(&mut self.selected_material).range(1..=u16::MAX));
        ui.label("Voxel color");
        let mut color = Color32::from_rgb(
            self.selected_color[0],
            self.selected_color[1],
            self.selected_color[2],
        );
        if ui.color_edit_button_srgba(&mut color).changed() {
            self.selected_color = [color.r(), color.g(), color.b()];
        }
        ui.checkbox(&mut self.jitter_enabled, "Jitter color per voxel");
        ui.add_enabled_ui(self.jitter_enabled, |ui| {
            ui.add(egui::Slider::new(&mut self.jitter_margin, 0..=64).text("± RGB margin"));
        });
        ui.separator();
        ui.collapsing("Precision coordinates", |ui| {
            ui.label("Single voxel");
            for axis in &mut self.voxel_cell {
                ui.add(egui::DragValue::new(axis));
            }
            if let Some(asset) = self.selected_asset {
                let cell = VoxelCoord {
                    x: self.voxel_cell[0],
                    y: self.voxel_cell[1],
                    z: self.voxel_cell[2],
                };
                ui.horizontal(|ui| {
                    if ui.button("Add at coordinates").clicked()
                        && self.cell_is_editable(asset, cell)
                    {
                        self.set_asset_voxel(asset, cell, false);
                    }
                    if ui.button("Chip at coordinates").clicked()
                        && self.cell_is_editable(asset, cell)
                    {
                        self.set_asset_voxel(asset, cell, true);
                    }
                });
                ui.separator();
                ui.label("Box corners");
                ui.label("Min");
                for axis in &mut self.box_min {
                    ui.add(egui::DragValue::new(axis));
                }
                ui.label("Max");
                for axis in &mut self.box_max {
                    ui.add(egui::DragValue::new(axis));
                }
                ui.horizontal(|ui| {
                    if ui.button("Add box at coordinates").clicked() {
                        self.paint_box(asset, false);
                    }
                    if ui.button("Chip box at coordinates").clicked() {
                        self.paint_box(asset, true);
                    }
                });
            }
        });
    }

    fn draw_asset_editor(&mut self, ui: &mut egui::Ui) {
        ui.heading("Voxel Asset Editor");
        let Some(asset) = self.selected_asset else {
            ui.label("Create or select a voxel asset from the asset library.");
            return;
        };
        let bounds = self.authoring_bounds(asset);
        self.draw_asset_tool_toolbar(ui);
        // Keep only a small footer for build/cursor status. The viewport owns
        // the rest of the central panel instead of leaving a fake "bottom
        // pane" of unused space below a fixed-height canvas.
        let viewport_height = (ui.available_height() - 58.0).max(260.0);
        let viewport = {
            let Some(asset_file) = self
                .model
                .as_ref()
                .and_then(|model| model.voxel_assets.get(&asset))
            else {
                return;
            };
            ui.horizontal(|ui| {
                ui.label(format!("Editing {}", asset_file.name));
                ui.label(format!("{} voxels", asset_file.voxels.len()));
            });
            render_asset_build_viewport(
                ui,
                asset_file,
                bounds,
                self.asset_cursor,
                self.asset_camera,
                viewport_height,
            )
        };
        self.handle_asset_viewport_input(asset, bounds, viewport, ui);
        if let Some(bounds) = bounds {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Build volume: ({}, {}, {}) to ({}, {}, {})",
                    bounds.min.x,
                    bounds.min.y,
                    bounds.min.z,
                    bounds.max.x,
                    bounds.max.y,
                    bounds.max.z
                ));
                if ui.button("Fill Bounds").clicked() {
                    self.fill_bounds(asset, bounds);
                }
            });
        } else {
            ui.colored_label(
                Color32::YELLOW,
                "Define build bounds in the Brush panel before filling or chipping.",
            );
        }
        ui.separator();
        ui.label(format!(
            "Cursor: ({}, {}, {}) — click to use {}, or choose Cursor to position only.",
            self.asset_cursor.x,
            self.asset_cursor.y,
            self.asset_cursor.z,
            self.asset_tool.label()
        ));
        ui.label(
            "MMB orbit (including below) · Shift+MMB pan · wheel zoom · F frame · WASD camera-relative / QE vertical · Enter apply",
        );
    }

    fn draw_asset_tool_toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Tools");
            for tool in [
                AssetTool::Cursor,
                AssetTool::AddVoxel,
                AssetTool::AddCube,
                AssetTool::Chip,
            ] {
                let response = ui
                    .selectable_label(self.asset_tool == tool, tool.icon())
                    .on_hover_text(tool.label());
                if response.clicked() {
                    self.asset_tool = tool;
                }
            }
            ui.separator();
            ui.label("Cube size");
            ui.add(egui::DragValue::new(&mut self.cube_extent).range(1..=32));
            if ui.small_button("Frame").clicked() {
                self.asset_camera = AssetCamera::default();
            }
        });
    }

    fn handle_asset_viewport_input(
        &mut self,
        asset: AssetId,
        bounds: Option<VoxelBounds>,
        viewport: AssetViewportOutput,
        ui: &egui::Ui,
    ) {
        let viewport_active = viewport.response.hovered() || viewport.response.has_focus();
        if viewport.response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.asset_camera.zoom =
                    (self.asset_camera.zoom * (scroll * 0.002).exp()).clamp(0.2, 8.0);
            }
            let modifiers = ui.input(|input| input.modifiers);
            let middle_down =
                ui.input(|input| input.pointer.button_down(egui::PointerButton::Middle));
            if middle_down {
                let delta = ui.input(|input| input.pointer.delta());
                if modifiers.shift {
                    self.asset_camera.pan += delta;
                } else {
                    self.asset_camera.yaw += delta.x * 0.01;
                    self.asset_camera.pitch =
                        (self.asset_camera.pitch - delta.y * 0.01).clamp(-1.30, 1.30);
                }
            }
        }
        if viewport_active {
            self.handle_asset_cursor_keys(asset, bounds, ui);
        }
        if viewport.response.clicked_by(egui::PointerButton::Primary)
            && let Some(pointer) = viewport.response.interact_pointer_pos()
        {
            viewport.response.request_focus();
            let cell = viewport.projection.cell_at(pointer, self.asset_cursor.y);
            if bounds.is_some_and(|bounds| !bounds.contains(cell)) {
                self.status = "Click inside the active build bounds to place the cursor".into();
                return;
            }
            self.asset_cursor = cell;
            if self.asset_tool != AssetTool::Cursor {
                self.apply_asset_tool(asset, bounds);
            }
        }
    }

    fn handle_asset_cursor_keys(
        &mut self,
        asset: AssetId,
        bounds: Option<VoxelBounds>,
        ui: &egui::Ui,
    ) {
        if ui.input(|input| input.key_pressed(egui::Key::F)) {
            self.asset_camera = AssetCamera::default();
        }
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.asset_tool = AssetTool::Cursor;
        }
        let mut delta = VoxelCoord { x: 0, y: 0, z: 0 };
        let forward =
            camera_relative_grid_step(self.asset_camera.yaw.sin(), self.asset_camera.yaw.cos());
        let right =
            camera_relative_grid_step(self.asset_camera.yaw.cos(), -self.asset_camera.yaw.sin());
        if ui.input(|input| input.key_pressed(egui::Key::A)) {
            delta.x -= right.0;
            delta.z -= right.1;
        }
        if ui.input(|input| input.key_pressed(egui::Key::D)) {
            delta.x += right.0;
            delta.z += right.1;
        }
        if ui.input(|input| input.key_pressed(egui::Key::W)) {
            delta.x += forward.0;
            delta.z += forward.1;
        }
        if ui.input(|input| input.key_pressed(egui::Key::S)) {
            delta.x -= forward.0;
            delta.z -= forward.1;
        }
        if ui.input(|input| input.key_pressed(egui::Key::Q)) {
            delta.y -= 1;
        }
        if ui.input(|input| input.key_pressed(egui::Key::E)) {
            delta.y += 1;
        }
        if delta != (VoxelCoord { x: 0, y: 0, z: 0 }) {
            self.nudge_asset_cursor(bounds, delta);
        }
        if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            self.apply_asset_tool(asset, bounds);
        }
    }

    fn nudge_asset_cursor(&mut self, bounds: Option<VoxelBounds>, delta: VoxelCoord) {
        let Some(next) = self
            .asset_cursor
            .x
            .checked_add(delta.x)
            .zip(self.asset_cursor.y.checked_add(delta.y))
            .zip(self.asset_cursor.z.checked_add(delta.z))
            .map(|((x, y), z)| VoxelCoord { x, y, z })
        else {
            self.status = "Cursor cannot move beyond the voxel coordinate range".into();
            return;
        };
        if bounds.is_some_and(|bounds| !bounds.contains(next)) {
            self.status = "Cursor is constrained to the active build bounds".into();
            return;
        }
        self.asset_cursor = next;
    }

    fn apply_asset_tool(&mut self, asset: AssetId, bounds: Option<VoxelBounds>) {
        if self.asset_tool == AssetTool::Cursor {
            return;
        }
        let Some(bounds) = bounds else {
            self.status = "Set build bounds before adding or chipping voxels".into();
            return;
        };
        if !bounds.contains(self.asset_cursor) {
            self.status = "The placement cursor is outside the active build bounds".into();
            return;
        }
        match self.asset_tool {
            AssetTool::Cursor => {}
            AssetTool::AddVoxel => self.set_asset_voxel(asset, self.asset_cursor, false),
            AssetTool::Chip => self.set_asset_voxel(asset, self.asset_cursor, true),
            AssetTool::AddCube => {
                let end = VoxelCoord {
                    x: self
                        .asset_cursor
                        .x
                        .saturating_add(i32::from(self.cube_extent) - 1),
                    y: self
                        .asset_cursor
                        .y
                        .saturating_add(i32::from(self.cube_extent) - 1),
                    z: self
                        .asset_cursor
                        .z
                        .saturating_add(i32::from(self.cube_extent) - 1),
                };
                let cube = VoxelBounds::new(self.asset_cursor, end);
                if !bounds.contains(cube.max) {
                    self.status = "Cube would exceed the active build bounds".into();
                    return;
                }
                self.paint_bounds(asset, cube, false);
            }
        }
    }

    fn set_asset_voxel(&mut self, asset: AssetId, cell: VoxelCoord, remove: bool) {
        let before = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&asset))
            .and_then(|asset| asset.state_at(cell));
        let after = (!remove).then(|| self.brush_state(cell));
        if before != after {
            self.execute(EditorCommand::SetVoxel {
                asset,
                cell,
                before,
                after,
            });
        }
    }

    fn brush_state(&self, cell: VoxelCoord) -> VoxelState {
        let margin = if self.jitter_enabled {
            self.jitter_margin
        } else {
            0
        };
        VoxelState::new(
            self.selected_material,
            jittered_color(self.selected_color, cell, margin),
        )
    }

    fn cell_is_editable(&mut self, asset: AssetId, cell: VoxelCoord) -> bool {
        match self.authoring_bounds(asset) {
            Some(bounds) if bounds.contains(cell) => true,
            Some(_) => {
                self.status = "That cell is outside this asset's build bounds".into();
                false
            }
            None => {
                self.status = "Set build bounds before adding or chipping voxels".into();
                false
            }
        }
    }

    fn fill_bounds(&mut self, asset: AssetId, bounds: VoxelBounds) {
        self.paint_bounds(asset, bounds, false);
    }

    fn paint_box(&mut self, asset: AssetId, remove: bool) {
        let Some(active_bounds) = self.authoring_bounds(asset) else {
            self.status = "Set build bounds before adding or chipping voxels".into();
            return;
        };
        let selected = VoxelBounds::new(
            VoxelCoord {
                x: self.box_min[0],
                y: self.box_min[1],
                z: self.box_min[2],
            },
            VoxelCoord {
                x: self.box_max[0],
                y: self.box_max[1],
                z: self.box_max[2],
            },
        );
        if !active_bounds.contains(selected.min) || !active_bounds.contains(selected.max) {
            self.status = "Box selection must stay inside the active build bounds".into();
            return;
        }
        self.paint_bounds(asset, selected, remove);
    }

    fn paint_bounds(&mut self, asset: AssetId, bounds: VoxelBounds, remove: bool) {
        let Some(cells) = bounds.cell_count() else {
            self.status = "Build bounds overflow the supported coordinate range".into();
            return;
        };
        if cells > 16_384 {
            self.status = "One build operation is limited to 16,384 voxels".into();
            return;
        }
        let mut changes = Vec::with_capacity(cells as usize);
        for z in bounds.min.z..=bounds.max.z {
            for y in bounds.min.y..=bounds.max.y {
                for x in bounds.min.x..=bounds.max.x {
                    let cell = VoxelCoord { x, y, z };
                    let before = self
                        .model
                        .as_ref()
                        .and_then(|model| model.voxel_assets.get(&asset))
                        .and_then(|asset| asset.state_at(cell));
                    let after = (!remove).then(|| self.brush_state(cell));
                    if before != after {
                        changes.push(VoxelChange {
                            cell,
                            before,
                            after,
                        });
                    }
                }
            }
        }
        if !changes.is_empty() {
            self.execute(EditorCommand::SetVoxels { asset, changes });
        }
    }

    fn render(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(egui_state) = self.egui_state.as_mut() else {
            return;
        };
        let raw = egui_state.take_egui_input(&window);
        let egui_context = self.egui_context.clone();
        let mut output = egui_context.run_ui(raw, |ui| self.draw(ui));
        self.egui_state
            .as_mut()
            .expect("state above")
            .handle_platform_output(&window, output.platform_output);
        let native_pixels_per_point = egui_winit::pixels_per_point(&self.egui_context, &window);
        let Some(gpu) = self.gpu.as_mut() else {
            output.textures_delta.clear();
            return;
        };
        let window_size = window.inner_size();
        let render_scale = (gpu.config.width as f32 / window_size.width.max(1) as f32)
            .min(gpu.config.height as f32 / window_size.height.max(1) as f32);
        let pixels_per_point = native_pixels_per_point * render_scale;
        let primitives = self
            .egui_context
            .tessellate(output.shapes, pixels_per_point);
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                gpu.renderer
                    .update_texture(&gpu.context.device, &gpu.context.queue, *id, delta);
            }
        }
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                drop(frame);
                gpu.surface.configure(&gpu.context.device, &gpu.config);
                for id in &output.textures_delta.free {
                    gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.context.device, &gpu.config);
                for id in &output.textures_delta.free {
                    gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                for id in &output.textures_delta.free {
                    gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                self.status = "surface acquisition validation error".into();
                for id in &output.textures_delta.free {
                    gpu.renderer.free_texture(id);
                }
                output.textures_delta.clear();
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            gpu.context
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("spall-editor-frame"),
                });
        let screen = ScreenDescriptor {
            size_in_pixels: [gpu.config.width, gpu.config.height],
            pixels_per_point,
        };
        let user_commands = gpu.renderer.update_buffers(
            &gpu.context.device,
            &gpu.context.queue,
            &mut encoder,
            &primitives,
            &screen,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-editor-ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.015,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            gpu.renderer
                .render(&mut pass.forget_lifetime(), &primitives, &screen);
        }
        gpu.context.queue.submit(
            user_commands
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        gpu.context.queue.present(frame);
        for id in &output.textures_delta.free {
            gpu.renderer.free_texture(id);
        }
        output.textures_delta.clear();
    }
}

impl ApplicationHandler for EditorApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("Spall Editor")
                        .with_inner_size(PhysicalSize::new(1440, 900)),
                )
                .expect("create editor window"),
        );
        self.egui_state = Some(EguiWinitState::new(
            self.egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        ));
        match EngineGpu::new(window.clone()) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(error) => self.status = format!("GPU unavailable: {error}"),
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = &self.window else {
            return;
        };
        if let Some(state) = self.egui_state.as_mut() {
            let _ = state.on_window_event(window, &event);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut()
                    && gpu.resize(size)
                {
                    self.status = format!(
                        "Maximized window; rendering at {}×{} for this GPU",
                        gpu.config.width, gpu.config.height
                    );
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

fn jittered_color(base: [u8; 3], cell: VoxelCoord, margin: u8) -> [u8; 3] {
    if margin == 0 {
        return base;
    }
    let mut seed = (cell.x as u32).wrapping_mul(0x9e37_79b9)
        ^ (cell.y as u32).wrapping_mul(0x85eb_ca6b)
        ^ (cell.z as u32).wrapping_mul(0xc2b2_ae35);
    let span = u32::from(margin) * 2 + 1;
    base.map(|component| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let offset = (seed % span) as i16 - i16::from(margin);
        (i16::from(component) + offset).clamp(0, 255) as u8
    })
}

fn draw_asset_preview(ui: &mut egui::Ui, asset: &VoxelAssetFile, side: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, Color32::from_rgb(22, 32, 47));
    let Some(first) = asset.voxels.keys().next().copied() else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Empty asset",
            egui::FontId::proportional(12.0),
            Color32::GRAY,
        );
        return;
    };
    let (mut min_x, mut max_x, mut min_z, mut max_z) = (first.x, first.x, first.z, first.z);
    for cell in asset.voxels.keys() {
        min_x = min_x.min(cell.x);
        max_x = max_x.max(cell.x);
        min_z = min_z.min(cell.z);
        max_z = max_z.max(cell.z);
    }
    let width = (max_x - min_x + 1).max(1) as f32;
    let height = (max_z - min_z + 1).max(1) as f32;
    let cell_side = ((side - 8.0) / width.max(height)).max(2.0);
    for &cell in asset.voxels.keys() {
        let state = asset.state_at(cell).expect("voxel map entry has state");
        let x = rect.left() + 4.0 + (cell.x - min_x) as f32 * cell_side;
        let y = rect.bottom() - 4.0 - (cell.z - min_z + 1) as f32 * cell_side;
        let color = Color32::from_rgb(state.color[0], state.color[1], state.color[2]);
        painter.rect_filled(
            Rect::from_min_size(Pos2::new(x, y), Vec2::splat((cell_side - 1.0).max(1.0))),
            1.0,
            color,
        );
    }
}

/// An intentionally lightweight isometric editing view. It visualizes the
/// active build volume and actual sparse cells without pretending that the UI
/// painter is Spall's eventual mesh-rendered camera.
fn render_asset_build_viewport(
    ui: &mut egui::Ui,
    asset: &VoxelAssetFile,
    active_bounds: Option<VoxelBounds>,
    cursor: VoxelCoord,
    camera: AssetCamera,
    height: f32,
) -> AssetViewportOutput {
    let width = ui.available_width().max(260.0);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgb(14, 22, 34));
    let bounds = active_bounds.or_else(|| voxel_extents(asset));
    let Some(bounds) = bounds else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Set build bounds, then fill it or add voxel chunks.",
            egui::FontId::proportional(15.0),
            Color32::GRAY,
        );
        return AssetViewportOutput {
            response,
            projection: AssetProjection {
                screen_center: rect.center(),
                cell_pixels: 16.0,
                yaw: camera.yaw,
                pitch: camera.pitch,
                world_center: [0.0; 3],
            },
        };
    };
    let span_x = i64::from(bounds.max.x) - i64::from(bounds.min.x) + 1;
    let span_y = i64::from(bounds.max.y) - i64::from(bounds.min.y) + 1;
    let span_z = i64::from(bounds.max.z) - i64::from(bounds.min.z) + 1;
    let upper_x = bounds.max.x.saturating_add(1);
    let upper_y = bounds.max.y.saturating_add(1);
    let upper_z = bounds.max.z.saturating_add(1);
    let projected_width = (span_x + span_z) as f32 * 0.68;
    let projected_height = (span_x + span_z) as f32 * 0.34 + span_y as f32 * 0.72;
    let cell = ((rect.width() - 36.0) / projected_width.max(1.0))
        .min((rect.height() - 36.0) / projected_height.max(1.0))
        .clamp(3.0, 30.0)
        * camera.zoom;
    let projection = AssetProjection {
        screen_center: rect.center() + camera.pan,
        cell_pixels: cell,
        yaw: camera.yaw,
        pitch: camera.pitch,
        world_center: [
            (bounds.min.x as f32 + bounds.max.x as f32 + 1.0) * 0.5,
            (bounds.min.y as f32 + bounds.max.y as f32 + 1.0) * 0.5,
            (bounds.min.z as f32 + bounds.max.z as f32 + 1.0) * 0.5,
        ],
    };

    if span_x <= 32 && span_z <= 32 {
        for x in bounds.min.x..=upper_x {
            painter.line_segment(
                [
                    projection.project(VoxelCoord {
                        x,
                        y: bounds.min.y,
                        z: bounds.min.z,
                    }),
                    projection.project(VoxelCoord {
                        x,
                        y: bounds.min.y,
                        z: upper_z,
                    }),
                ],
                Stroke::new(1.0, Color32::from_gray(48)),
            );
        }
        for z in bounds.min.z..=upper_z {
            painter.line_segment(
                [
                    projection.project(VoxelCoord {
                        x: bounds.min.x,
                        y: bounds.min.y,
                        z,
                    }),
                    projection.project(VoxelCoord {
                        x: upper_x,
                        y: bounds.min.y,
                        z,
                    }),
                ],
                Stroke::new(1.0, Color32::from_gray(48)),
            );
        }
    }

    let mut cells: Vec<_> = asset.voxels.keys().copied().collect();
    cells.sort_by(|left, right| {
        projection
            .depth(*left)
            .partial_cmp(&projection.depth(*right))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for cell_coord in cells.into_iter().take(65_536) {
        let state = asset
            .state_at(cell_coord)
            .expect("voxel map entry has state");
        let p000 = projection.project(cell_coord);
        let p100 = projection.project(VoxelCoord {
            x: cell_coord.x.saturating_add(1),
            ..cell_coord
        });
        let p001 = projection.project(VoxelCoord {
            z: cell_coord.z.saturating_add(1),
            ..cell_coord
        });
        let p010 = projection.project(VoxelCoord {
            y: cell_coord.y.saturating_add(1),
            ..cell_coord
        });
        let p110 = projection.project(VoxelCoord {
            x: cell_coord.x.saturating_add(1),
            y: cell_coord.y.saturating_add(1),
            ..cell_coord
        });
        let p101 = projection.project(VoxelCoord {
            x: cell_coord.x.saturating_add(1),
            z: cell_coord.z.saturating_add(1),
            ..cell_coord
        });
        let p011 = projection.project(VoxelCoord {
            y: cell_coord.y.saturating_add(1),
            z: cell_coord.z.saturating_add(1),
            ..cell_coord
        });
        let p111 = projection.project(VoxelCoord {
            x: cell_coord.x.saturating_add(1),
            y: cell_coord.y.saturating_add(1),
            z: cell_coord.z.saturating_add(1),
        });
        let color = Color32::from_rgb(state.color[0], state.color[1], state.color[2]);
        painter.add(egui::Shape::convex_polygon(
            vec![p000, p001, p011, p010],
            shaded(color, 0.64),
            Stroke::new(0.5, Color32::from_gray(20)),
        ));
        painter.add(egui::Shape::convex_polygon(
            vec![p000, p100, p110, p010],
            shaded(color, 0.82),
            Stroke::new(0.5, Color32::from_gray(20)),
        ));
        let cap = if camera.pitch >= 0.0 {
            vec![p010, p110, p111, p011]
        } else {
            vec![p000, p001, p101, p100]
        };
        painter.add(egui::Shape::convex_polygon(
            cap,
            color,
            Stroke::new(0.5, Color32::from_gray(20)),
        ));
    }
    let corners = [
        VoxelCoord {
            x: bounds.min.x,
            y: bounds.min.y,
            z: bounds.min.z,
        },
        VoxelCoord {
            x: upper_x,
            y: bounds.min.y,
            z: bounds.min.z,
        },
        VoxelCoord {
            x: upper_x,
            y: bounds.min.y,
            z: upper_z,
        },
        VoxelCoord {
            x: bounds.min.x,
            y: bounds.min.y,
            z: upper_z,
        },
        VoxelCoord {
            x: bounds.min.x,
            y: upper_y,
            z: bounds.min.z,
        },
        VoxelCoord {
            x: upper_x,
            y: upper_y,
            z: bounds.min.z,
        },
        VoxelCoord {
            x: upper_x,
            y: upper_y,
            z: upper_z,
        },
        VoxelCoord {
            x: bounds.min.x,
            y: upper_y,
            z: upper_z,
        },
    ];
    for (from, to) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ] {
        painter.line_segment(
            [
                projection.project(corners[from]),
                projection.project(corners[to]),
            ],
            Stroke::new(1.25, Color32::from_rgb(93, 170, 235)),
        );
    }
    let cursor_corners = voxel_cube_corners(cursor);
    for (from, to) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ] {
        painter.line_segment(
            [
                projection.project(cursor_corners[from]),
                projection.project(cursor_corners[to]),
            ],
            Stroke::new(1.8, Color32::YELLOW),
        );
    }
    AssetViewportOutput {
        response,
        projection,
    }
}

fn voxel_cube_corners(cell: VoxelCoord) -> [VoxelCoord; 8] {
    let x = cell.x.saturating_add(1);
    let y = cell.y.saturating_add(1);
    let z = cell.z.saturating_add(1);
    [
        cell,
        VoxelCoord {
            x,
            y: cell.y,
            z: cell.z,
        },
        VoxelCoord { x, y: cell.y, z },
        VoxelCoord {
            x: cell.x,
            y: cell.y,
            z,
        },
        VoxelCoord {
            x: cell.x,
            y,
            z: cell.z,
        },
        VoxelCoord { x, y, z: cell.z },
        VoxelCoord { x, y, z },
        VoxelCoord { x: cell.x, y, z },
    ]
}

fn voxel_extents(asset: &VoxelAssetFile) -> Option<VoxelBounds> {
    let first = asset.voxels.keys().next().copied()?;
    Some(
        asset
            .voxels
            .keys()
            .copied()
            .fold(VoxelBounds::new(first, first), |bounds, cell| VoxelBounds {
                min: VoxelCoord {
                    x: bounds.min.x.min(cell.x),
                    y: bounds.min.y.min(cell.y),
                    z: bounds.min.z.min(cell.z),
                },
                max: VoxelCoord {
                    x: bounds.max.x.max(cell.x),
                    y: bounds.max.y.max(cell.y),
                    z: bounds.max.z.max(cell.z),
                },
            }),
    )
}

fn shaded(color: Color32, factor: f32) -> Color32 {
    Color32::from_rgb(
        (f32::from(color.r()) * factor) as u8,
        (f32::from(color.g()) * factor) as u8,
        (f32::from(color.b()) * factor) as u8,
    )
}

/// Converts a camera-facing planar vector into one or two integer cell steps.
/// Diagonal movement is deliberate: the cursor remains visually aligned with
/// the viewport after orbiting to a different side of the asset.
fn camera_relative_grid_step(x: f32, z: f32) -> (i32, i32) {
    fn component(value: f32) -> i32 {
        if value > 0.25 {
            1
        } else if value < -0.25 {
            -1
        } else {
            0
        }
    }
    (component(x), component(z))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_maximized_window_uses_safe_internal_surface_size() {
        let fitted = fit_surface_size(PhysicalSize::new(2560, 1369), 2048);
        assert_eq!(fitted, PhysicalSize::new(2048, 1095));
        assert!(fitted.width <= 2048 && fitted.height <= 2048);
    }

    #[test]
    fn supported_surface_size_is_unchanged() {
        assert_eq!(
            fit_surface_size(PhysicalSize::new(1440, 900), 2048),
            PhysicalSize::new(1440, 900)
        );
    }

    #[test]
    fn isometric_viewport_click_round_trips_to_the_cursor_cell() {
        let projection = AssetProjection {
            screen_center: Pos2::new(400.0, 300.0),
            cell_pixels: 24.0,
            yaw: 0.72,
            pitch: 0.61,
            world_center: [2.5, 3.5, -1.5],
        };
        let cell = VoxelCoord { x: 4, y: 3, z: -2 };
        assert_eq!(projection.cell_at(projection.project(cell), cell.y), cell);
    }

    #[test]
    fn cursor_controls_follow_the_camera_and_underview_projection() {
        assert_eq!(camera_relative_grid_step(0.0, 1.0), (0, 1));
        assert_eq!(camera_relative_grid_step(1.0, 0.0), (1, 0));
        let projection = AssetProjection {
            screen_center: Pos2::new(100.0, 100.0),
            cell_pixels: 20.0,
            yaw: -0.9,
            pitch: -0.7,
            world_center: [0.0; 3],
        };
        let cell = VoxelCoord { x: -2, y: 3, z: 1 };
        assert_eq!(projection.cell_at(projection.project(cell), cell.y), cell);
    }
}
