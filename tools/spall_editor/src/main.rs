//! Native Yakui editor shell. Document edits continue to flow through
//! `EditorCommand`; Yakui owns presentation and input only.

use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use spall_editor::{
    AssetId, EditorCommand, EditorEntityId, EditorModel, UndoStack, VoxelBounds, VoxelChange,
    VoxelCoord, VoxelLayer, VoxelLayerCell, VoxelLayerChange, VoxelState,
};
use spall_render::EnvironmentPreset as PreviewEnvironment;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};
use yakui::{Vec2, Vec4};

mod scene_viewport;
mod theme;
mod thumbnail;
mod ui;

use thumbnail::ThumbMesh;
use ui::{Modal, RightTab, SceneTab, SceneView};

fn main() {
    let event_loop = EventLoop::new().expect("create winit event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop
        .run_app(&mut EditorApp::default())
        .expect("run editor");
}

struct EngineGpu {
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    context: spall_render::RenderContext,
    config: wgpu::SurfaceConfiguration,
    state: yakui::Yakui,
    yakui: yakui_wgpu::YakuiWgpu,
    buffers: yakui_wgpu::Buffers,
    /// Renders the scene viewport with the engine's shadow/HDR/tone-map
    /// passes; yakui samples its colour target as `viewport_texture`.
    viewport: spall_render::ViewportRenderer,
    viewport_texture: yakui::TextureId,
    /// Physical pixels per yakui layout unit.
    ui_scale: f32,
}

impl EngineGpu {
    fn new(window: Arc<Window>) -> Result<Self, String> {
        let mut descriptor =
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone()));
        descriptor.backends = if cfg!(target_os = "windows") {
            wgpu::Backends::DX12
        } else {
            wgpu::Backends::all()
        };
        let instance = wgpu::Instance::new(descriptor);
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| e.to_string())?;
        let (context, caps) = spall_render::RenderContext::for_surface(&instance, &surface)
            .map_err(|e| e.to_string())?;
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = fit_surface_size(
            window.inner_size(),
            context.device.limits().max_texture_dimension_2d,
        );
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&context.device, &config);
        let mut yakui = yakui_wgpu::YakuiWgpu::new(context.device.clone(), context.queue.clone());
        let buffers = yakui.buffers();
        let state = yakui::Yakui::new();
        let viewport = spall_render::ViewportRenderer::new(&context, &[]);
        let viewport_texture = yakui.add_texture(
            viewport.color_view().clone(),
            wgpu::FilterMode::Linear,
            wgpu::FilterMode::Linear,
            wgpu::MipmapFilterMode::Nearest,
            wgpu::AddressMode::ClampToEdge,
        );
        Ok(Self {
            _instance: instance,
            surface,
            context,
            config,
            state,
            yakui,
            buffers,
            viewport,
            viewport_texture,
            ui_scale: 1.0,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> bool {
        if size.width > 0 && size.height > 0 {
            let fitted =
                fit_surface_size(size, self.context.device.limits().max_texture_dimension_2d);
            self.config.width = fitted.width;
            self.config.height = fitted.height;
            self.surface.configure(&self.context.device, &self.config);
            return fitted != size;
        }
        false
    }

    fn sync_ui_surface(&mut self, native_size: PhysicalSize<u32>, dpi: f32) {
        let x_scale = self.config.width as f32 / native_size.width.max(1) as f32;
        let y_scale = self.config.height as f32 / native_size.height.max(1) as f32;
        let render_scale = x_scale.min(y_scale).min(1.0);
        self.state.set_surface_size(yakui::Vec2::new(
            self.config.width as f32,
            self.config.height as f32,
        ));
        self.ui_scale = dpi * render_scale;
        self.state.set_scale_factor(self.ui_scale);
    }
}

/// Keep the target within the selected adapter's texture limit while the
/// window itself remains at its native size.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Workspace {
    Scene,
    Assets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    File,
    Edit,
    View,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelDrag {
    Left,
    Right,
    Bottom,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RecentProjects {
    roots: Vec<PathBuf>,
}

struct EditorApp {
    window: Option<Arc<Window>>,
    yakui_input: Option<yakui_winit::YakuiWinit>,
    gpu: Option<EngineGpu>,
    model: Option<EditorModel>,
    engine_asset_session: bool,
    undo: UndoStack,
    workspace: Workspace,
    open_menu: Option<Menu>,
    panel_drag: Option<PanelDrag>,
    panel_hover: Option<PanelDrag>,
    show_engine_assets: bool,
    imported_builtins: std::collections::HashMap<&'static str, AssetId>,
    left_panel_width: f32,
    right_panel_width: f32,
    bottom_panel_height: f32,
    left_panel_open: bool,
    right_panel_open: bool,
    bottom_panel_open: bool,
    selected_entity: Option<EditorEntityId>,
    selected_asset: Option<AssetId>,
    selected_layer: Option<u32>,
    selected_builtin: Option<&'static str>,
    builtin_preview: Option<spall_editor::VoxelAssetFile>,
    builtin_cards: Vec<(
        spall_voxel::builtin_assets::BuiltinVoxelAsset,
        spall_editor::VoxelAssetFile,
        Arc<ThumbMesh>,
    )>,
    project_thumbnails: Vec<(AssetId, Arc<ThumbMesh>)>,
    project_path: String,
    project_name: String,
    asset_name: String,
    layer_name: String,
    search: String,
    coord: [String; 3],
    bounds_min: [String; 3],
    bounds_max: [String; 3],
    material: String,
    color: [String; 3],
    translation: [String; 3],
    rotation: [String; 3],
    scale: [String; 3],
    box_min: [String; 3],
    box_max: [String; 3],
    jitter: String,
    chip_mode: bool,
    status: String,
    recents: Vec<PathBuf>,
    preview_camera: PreviewCamera,
    preview_environment: PreviewEnvironment,
    environment_menu_open: bool,
    view_menu_open: bool,
    show_voxel_boundaries: bool,
    show_asset_bounds: bool,
    preview_drag: Option<PreviewDrag>,
    preview_cursor: Option<(f64, f64)>,
    preview_hover_cell: Arc<std::sync::Mutex<Option<VoxelCoord>>>,
    selected_voxel: Option<VoxelCoord>,
    shift_held: bool,
    ctrl_held: bool,
    exit_requested: bool,
    modal: Option<Modal>,
    scene_tab: SceneTab,
    scene_select: bool,
    scene_view: Option<SceneView>,
    /// Bumped whenever `scene_view` is recomputed, so derived data can tell
    /// the scene changed.
    scene_revision: u64,
    scene_mesh: Option<SceneMeshState>,
    /// Layout rect (position, size) the scene viewport last painted into, in
    /// yakui units. The renderer sizes its target from it.
    scene_viewport_rect: Arc<std::sync::Mutex<Option<(Vec2, Vec2)>>>,
    right_tab: RightTab,
}

/// The scene viewport's CPU mesh and whether the GPU holds a copy of it.
pub(crate) struct SceneMeshState {
    key: SceneMeshKey,
    mesh: Arc<spall_editor::scene_mesh::SceneMesh>,
    uploaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SceneMeshKey {
    revision: u64,
    selected: Option<EditorEntityId>,
    grid_lines: bool,
}

#[derive(Debug, Clone, Copy)]
struct PreviewCamera {
    yaw: f32,
    pitch: f32,
    zoom: f32,
    pan: Vec2,
}

/// Ambient, direct and tint for the flat 2D asset preview only. The scene
/// viewport lights through `spall_render::Environment` instead.
fn preview_lighting(environment: PreviewEnvironment) -> (f32, f32, [f32; 3]) {
    match environment {
        PreviewEnvironment::Studio => (0.32, 0.68, [1.0, 1.0, 1.0]),
        PreviewEnvironment::Daylight => (0.20, 0.80, [1.0, 1.0, 0.96]),
        PreviewEnvironment::Overcast => (0.62, 0.38, [0.88, 0.93, 1.0]),
        PreviewEnvironment::Sunset => (0.25, 0.75, [1.0, 0.66, 0.42]),
        PreviewEnvironment::Night => (0.12, 0.48, [0.48, 0.62, 1.0]),
    }
}

impl Default for PreviewCamera {
    fn default() -> Self {
        Self {
            yaw: 0.785,
            pitch: 0.52,
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PreviewDrag {
    Orbit,
    Pan,
}

impl Default for EditorApp {
    fn default() -> Self {
        let recents = std::fs::read_to_string(".local/spall-editor/recent.ron")
            .ok()
            .and_then(|s| ron::from_str::<RecentProjects>(&s).ok())
            .map_or_else(Vec::new, |r| r.roots);
        Self {
            window: None,
            yakui_input: None,
            gpu: None,
            model: None,
            engine_asset_session: false,
            undo: UndoStack::default(),
            workspace: Workspace::Scene,
            open_menu: None,
            panel_drag: None,
            panel_hover: None,
            show_engine_assets: false,
            imported_builtins: std::collections::HashMap::new(),
            left_panel_width: 280.0,
            right_panel_width: 300.0,
            bottom_panel_height: 230.0,
            left_panel_open: true,
            right_panel_open: true,
            bottom_panel_open: true,
            selected_entity: None,
            selected_asset: None,
            selected_layer: None,
            selected_builtin: None,
            builtin_preview: None,
            builtin_cards: Vec::new(),
            project_thumbnails: Vec::new(),
            project_path: String::new(),
            project_name: String::new(),
            asset_name: String::new(),
            layer_name: String::new(),
            search: String::new(),
            coord: ["0".into(), "0".into(), "0".into()],
            bounds_min: ["0".into(), "0".into(), "0".into()],
            bounds_max: ["7".into(), "7".into(), "7".into()],
            material: "1".into(),
            color: ["108".into(), "112".into(), "120".into()],
            status: String::new(),
            recents,
            translation: ["0".into(), "0".into(), "0".into()],
            rotation: ["0".into(), "0".into(), "0".into()],
            scale: ["1".into(), "1".into(), "1".into()],
            box_min: ["0".into(), "0".into(), "0".into()],
            box_max: ["1".into(), "1".into(), "1".into()],
            jitter: "8".into(),
            chip_mode: false,
            preview_camera: PreviewCamera::default(),
            preview_environment: PreviewEnvironment::default(),
            environment_menu_open: false,
            view_menu_open: false,
            show_voxel_boundaries: false,
            show_asset_bounds: false,
            preview_drag: None,
            preview_cursor: None,
            preview_hover_cell: Arc::new(std::sync::Mutex::new(None)),
            selected_voxel: None,
            shift_held: false,
            ctrl_held: false,
            exit_requested: false,
            modal: None,
            scene_tab: SceneTab::default(),
            scene_select: true,
            scene_view: None,
            scene_revision: 0,
            scene_mesh: None,
            scene_viewport_rect: Arc::new(std::sync::Mutex::new(None)),
            right_tab: RightTab::default(),
        }
    }
}

fn int3(values: &[String; 3]) -> Option<[i32; 3]> {
    Some([
        values[0].parse().ok()?,
        values[1].parse().ok()?,
        values[2].parse().ok()?,
    ])
}

impl EditorApp {
    /// Which panel divider (if any) lies under the physical-pixel position.
    fn panel_edge_at(&self, x: f64, y: f64) -> Option<PanelDrag> {
        let window = self.window.as_ref()?;
        self.model.as_ref()?;
        if self.modal.is_some() {
            return None;
        }
        let dpi = window.scale_factor();
        let size = window.inner_size();
        let near = |a: f64, b: f64| (a - b).abs() <= 6.0 * dpi;
        let body_top = f64::from(theme::TOP_BAR_HEIGHT) * dpi;
        let status = f64::from(theme::STATUS_BAR_HEIGHT) * dpi;
        let bottom_edge =
            f64::from(size.height) - status - (f64::from(self.bottom_panel_height) + 1.0) * dpi;
        let body_bottom = if self.bottom_panel_open {
            bottom_edge
        } else {
            f64::from(size.height) - status
        };
        let in_body = y > body_top && y < body_bottom;
        if self.left_panel_open && in_body && near(x, f64::from(self.left_panel_width) * dpi) {
            Some(PanelDrag::Left)
        } else if self.right_panel_open
            && in_body
            && near(
                x,
                f64::from(size.width) - (f64::from(self.right_panel_width) + 1.0) * dpi,
            )
        {
            Some(PanelDrag::Right)
        } else if self.bottom_panel_open && near(y, bottom_edge) {
            Some(PanelDrag::Bottom)
        } else {
            None
        }
    }

    /// Whether the cursor is over the asset viewport, derived from the fixed
    /// chrome sizes in `theme` and the current panel sizes. Overlay controls in
    /// the viewport's top-right corner are excluded so clicking them does not
    /// select the voxel behind them.
    fn pointer_in_viewport(&self) -> bool {
        let (Some((x, y)), Some(window)) = (self.preview_cursor, self.window.as_ref()) else {
            return false;
        };
        if self.modal.is_some() {
            return false;
        }
        let dpi = window.scale_factor();
        let size = window.inner_size();
        let left = if self.left_panel_open {
            f64::from(self.left_panel_width) + 1.0
        } else {
            0.0
        };
        let right = if self.right_panel_open {
            f64::from(self.right_panel_width) + 1.0
        } else {
            0.0
        };
        let bottom = (if self.bottom_panel_open {
            f64::from(self.bottom_panel_height) + 1.0
        } else {
            0.0
        }) + f64::from(theme::STATUS_BAR_HEIGHT);
        let top = f64::from(theme::TOP_BAR_HEIGHT);
        let viewport_right = f64::from(size.width) - right * dpi;
        let controls_height = if self.environment_menu_open || self.view_menu_open {
            220.0
        } else {
            52.0
        };
        let over_controls = x > viewport_right - 380.0 * dpi && y < (top + controls_height) * dpi;
        x > left * dpi
            && x < viewport_right
            && y > top * dpi
            && y < f64::from(size.height) - bottom * dpi
            && !over_controls
    }

    fn remember(&mut self, path: &std::path::Path) {
        self.recents.retain(|p| p != path);
        self.recents.insert(0, path.into());
        self.recents.truncate(8);
        let _ = std::fs::create_dir_all(".local/spall-editor");
        if let Ok(s) = ron::ser::to_string_pretty(
            &RecentProjects {
                roots: self.recents.clone(),
            },
            ron::ser::PrettyConfig::default(),
        ) {
            let _ = std::fs::write(".local/spall-editor/recent.ron", s);
        }
    }
    /// Native file dialog pre-set to the last project's parent folder and
    /// parented to the editor window. Blocks until the dialog closes.
    fn file_dialog(&self, title: &str) -> rfd::FileDialog {
        let mut dialog = rfd::FileDialog::new().set_title(title);
        if let Some(parent) = self.recents.first().and_then(|root| root.parent())
            && parent.is_dir()
        {
            dialog = dialog.set_directory(parent);
        }
        if let Some(window) = self.window.as_ref() {
            dialog = dialog.set_parent(window.as_ref());
        }
        dialog
    }
    /// Choose a project folder. Returns `None` if the dialog was cancelled.
    fn pick_project_folder(&mut self, title: &str) -> Option<PathBuf> {
        let folder = self.file_dialog(title).pick_folder()?;
        self.project_path = folder.display().to_string();
        Some(folder)
    }
    /// File > Open: choose `project.ron` or a scene `.ron` inside a project.
    fn open_project_dialog(&mut self) {
        let Some(file) = self
            .file_dialog("Open project or scene")
            .add_filter("Spall project or scene (*.ron)", &["ron"])
            .pick_file()
        else {
            return;
        };
        self.project_path = file.display().to_string();
        self.open_project();
    }
    /// File > New Project: pick the folder, name the project after it unless a
    /// name was already typed.
    fn new_project_dialog(&mut self) {
        if let Some(folder) = self.pick_project_folder("Choose a folder for the new project") {
            if self.project_name.trim().is_empty()
                && let Some(name) = folder.file_name()
            {
                self.project_name = name.to_string_lossy().into_owned();
            }
            self.new_project();
        }
    }
    fn refresh_builtin_cards(&mut self) {
        let Some(model) = self.model.as_ref() else {
            self.builtin_cards.clear();
            self.refresh_project_thumbnails();
            return;
        };
        let mut failures = Vec::new();
        self.builtin_cards = spall_voxel::builtin_assets::builtin_voxel_assets()
            .iter()
            .copied()
            .filter_map(|builtin| match model.preview_builtin_spvox(builtin.bytes) {
                Ok(preview) => {
                    let thumbnail = ThumbMesh::build(&preview);
                    Some((builtin, preview, thumbnail))
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", builtin.name));
                    None
                }
            })
            .collect();
        if !failures.is_empty() {
            self.status = format!("Engine assets unavailable: {}", failures.join("; "));
        }
        self.refresh_project_thumbnails();
    }
    fn refresh_project_thumbnails(&mut self) {
        let Some(model) = self.model.as_ref() else {
            self.project_thumbnails.clear();
            return;
        };
        self.project_thumbnails = model
            .project
            .asset_database
            .assets
            .keys()
            .filter_map(|id| {
                model
                    .voxel_assets
                    .get(id)
                    .map(|asset| (*id, ThumbMesh::build(asset)))
            })
            .collect();
    }
    fn open_project(&mut self) {
        match EditorModel::load_file(PathBuf::from(self.project_path.trim())) {
            Ok(m) => {
                self.project_path = m.root.display().to_string();
                self.remember(&m.root);
                self.status = format!("Opened {}", m.project.name);
                self.preview_environment =
                    PreviewEnvironment::from_key(&m.scene.environment).unwrap_or_default();
                self.model = Some(m);
                self.scene_view = None;
                self.refresh_builtin_cards();
                self.engine_asset_session = false;
                self.undo = UndoStack::default();
            }
            Err(e) => self.status = e.to_string(),
        }
    }
    fn new_project(&mut self) {
        let root = if self.project_path.trim().is_empty() {
            PathBuf::from(".local/editor-project")
        } else {
            PathBuf::from(self.project_path.trim())
        };
        let m = EditorModel::new(
            &root,
            if self.project_name.trim().is_empty() {
                "Untitled"
            } else {
                self.project_name.trim()
            },
        );
        match m.save_all() {
            Ok(()) => {
                self.remember(&m.root);
                self.status = "Created project".into();
                self.preview_environment =
                    PreviewEnvironment::from_key(&m.scene.environment).unwrap_or_default();
                self.model = Some(m);
                self.scene_view = None;
                self.refresh_builtin_cards();
                self.engine_asset_session = false;
            }
            Err(e) => self.status = e.to_string(),
        }
    }
    fn open_engine_assets(&mut self) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let root = PathBuf::from(".local/spall-editor").join(format!("engine-assets-{stamp}"));
        self.model = Some(EditorModel::new(root, "Engine Asset Workspace"));
        self.scene_view = None;
        self.refresh_builtin_cards();
        self.engine_asset_session = true;
        self.workspace = Workspace::Assets;
        self.left_panel_open = true;
        self.bottom_panel_open = true;
        self.selected_voxel = None;
        if let Some(builtin) = spall_voxel::builtin_assets::builtin_voxel_assets().first() {
            self.selected_builtin = Some(builtin.key);
            self.builtin_preview = self
                .model
                .as_ref()
                .and_then(|model| model.preview_builtin_spvox(builtin.bytes).ok());
        }
        self.status =
            "Browsing immutable engine assets. Copy an asset into this workspace to edit it."
                .into();
    }
    fn execute(&mut self, cmd: EditorCommand) {
        self.scene_view = None;
        if let Some(m) = self.model.as_mut()
            && let Err(e) = self.undo.execute(m, cmd)
        {
            self.status = e.to_string();
        }
        self.refresh_project_thumbnails();
    }
    fn edit_voxels(&mut self, asset: AssetId, changes: Vec<VoxelChange>) {
        let Some(doc) = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&asset))
        else {
            return;
        };
        if doc.layers.is_empty() {
            self.execute(EditorCommand::SetVoxels { asset, changes });
            return;
        }
        let layer = self
            .selected_layer
            .and_then(|id| {
                doc.layers
                    .iter()
                    .find(|layer| layer.id == id && layer.visible)
            })
            .or_else(|| doc.layers.iter().rev().find(|layer| layer.visible));
        let Some(layer) = layer else {
            self.status = "Show a layer before editing voxels".into();
            return;
        };
        let changes = changes
            .into_iter()
            .map(|change| VoxelLayerChange {
                cell: change.cell,
                before: layer.cells.get(&change.cell).copied(),
                after: change.after.map(|state| VoxelLayerCell {
                    material: state.material,
                    tint: Some(state.color),
                }),
            })
            .filter(|change| change.before != change.after)
            .collect();
        self.execute(EditorCommand::SetLayerVoxels {
            asset,
            layer_id: layer.id,
            changes,
        });
    }
    fn save(&mut self) {
        if let Some(m) = &self.model {
            self.status = match m.save_all() {
                Ok(()) => "Saved project, scene, and assets".into(),
                Err(e) => e.to_string(),
            };
        }
    }
    fn run_scene(&mut self) {
        self.save();
        let Some(m) = &self.model else {
            return;
        };
        let Some(scene) = m
            .project
            .scenes
            .get(&m.scene.name)
            .map(|relative| m.root.join(relative))
        else {
            self.status = "Active scene has no project entry; cannot run".into();
            return;
        };
        // `cargo xtask play` builds the sandbox, starts its server on this
        // scene, and opens an interactive client window. It must run from the
        // workspace, which is two levels above this crate.
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .map(std::path::Path::to_path_buf);
        let mut command = std::process::Command::new("cargo");
        command
            .args(["xtask", "play", "--editor-scene"])
            .arg(&scene);
        if let Some(workspace) = workspace {
            command.current_dir(workspace);
        }
        self.status = match command.spawn() {
            Ok(_) => format!(
                "Launched {} (building first run may take a while)",
                scene.display()
            ),
            Err(e) => format!("Could not launch cargo xtask play: {e}"),
        };
    }
    fn render(&mut self) {
        let (frame, mut encoder, mut state) = {
            let Some(gpu) = self.gpu.as_mut() else {
                return;
            };
            let frame = match gpu.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                    gpu.surface.configure(&gpu.context.device, &gpu.config);
                    f
                }
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                    gpu.surface.configure(&gpu.context.device, &gpu.config);
                    return;
                }
                _ => return,
            };
            let encoder =
                gpu.context
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("spall-editor-frame"),
                    });
            let state = std::mem::replace(&mut gpu.state, yakui::Yakui::new());
            (frame, encoder, state)
        };
        state.start();
        self.ui();
        state.finish();
        self.sync_scene_viewport();
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("spall-editor-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.015,
                            g: 0.02,
                            b: 0.03,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        gpu.state = state;
        gpu.yakui.paint_with_encoder(
            &mut gpu.state,
            &mut gpu.buffers,
            &mut encoder,
            yakui_wgpu::SurfaceInfo {
                format: gpu.config.format,
                sample_count: 1,
                color_attachment: &view,
                resolve_target: None,
            },
        );
        gpu.context.queue.submit([encoder.finish()]);
        gpu.context.queue.present(frame);
    }
}

fn bounds_cells(b: VoxelBounds) -> impl Iterator<Item = VoxelCoord> {
    (b.min.z..=b.max.z).flat_map(move |z| {
        (b.min.y..=b.max.y)
            .flat_map(move |y| (b.min.x..=b.max.x).map(move |x| VoxelCoord { x, y, z }))
    })
}
fn jittered(base: [u8; 3], cell: VoxelCoord, margin: u8) -> [u8; 3] {
    let mut seed = (cell.x as u32).wrapping_mul(0x9e37_79b9)
        ^ (cell.y as u32).wrapping_mul(0x85eb_ca6b)
        ^ (cell.z as u32).wrapping_mul(0xc2b2_ae35);
    let span = u32::from(margin) * 2 + 1;
    base.map(|c| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (i16::from(c) + ((seed % span) as i16 - i16::from(margin))).clamp(0, 255) as u8
    })
}

fn voxel_extents(asset: &spall_editor::VoxelAssetFile) -> Option<(VoxelCoord, VoxelCoord)> {
    let first = asset.voxels.keys().next().copied()?;
    let mut min = first;
    let mut max = first;
    for cell in asset.voxels.keys() {
        min.x = min.x.min(cell.x);
        min.y = min.y.min(cell.y);
        min.z = min.z.min(cell.z);
        max.x = max.x.max(cell.x);
        max.y = max.y.max(cell.y);
        max.z = max.z.max(cell.z);
    }
    Some((min, max))
}

/// Draw a selected SPVOX asset as a shaded isometric voxel model. This is a
/// 3D projection of the full asset, not a single horizontal slice.
fn draw_asset_3d_preview(
    asset: spall_editor::VoxelAssetFile,
    camera: PreviewCamera,
    environment: PreviewEnvironment,
    cursor: Option<Vec2>,
    hover_cell: Option<Arc<std::sync::Mutex<Option<VoxelCoord>>>>,
    selected_voxel: Option<VoxelCoord>,
    show_voxel_boundaries: bool,
    asset_bounds: Option<VoxelBounds>,
    show_asset_bounds: bool,
) {
    yakui::expanded(|| {
        let background = environment.environment().background;
        yakui::colored_box_container(
            yakui::Color::rgb(background[0], background[1], background[2]),
            || {
                // Yakui's custom canvas meshes do not clip themselves to their
                // layout rectangle. Use a non-scrolling Scrollable as a clip
                // container so zoomed/panned geometry stays inside this viewport.
                yakui::expanded(|| {
                    yakui::widgets::Scrollable::none().show(|| {
                        yakui::canvas(move |ctx| {
                            paint_viewport_grid(ctx, background);
                            paint_asset_model(
                                &asset,
                                camera,
                                environment,
                                cursor,
                                hover_cell.clone(),
                                selected_voxel,
                                show_voxel_boundaries,
                                asset_bounds,
                                show_asset_bounds,
                                ctx,
                            )
                        });
                    });
                });
            },
        );
    });
}

/// Faint 32px grid behind the model, a few percent brighter than the
/// environment background.
fn paint_viewport_grid(ctx: &mut yakui::widget::PaintContext<'_>, background: [u8; 3]) {
    const SPACING: f32 = 32.0;
    let widget = ctx.dom.current();
    let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
        return;
    };
    let base = yakui::Color::rgb(background[0], background[1], background[2]);
    let mut line = base.lerp(&yakui::Color::WHITE, 0.05).to_linear();
    line.w = 0.55;
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let columns = (rect.size().x / SPACING).ceil() as usize;
    let rows = (rect.size().y / SPACING).ceil() as usize;
    for column in 0..=columns {
        let x = rect.pos().x + column as f32 * SPACING;
        append_line_quad(
            &mut vertices,
            &mut indices,
            Vec2::new(x, rect.pos().y),
            Vec2::new(x, rect.max().y),
            line,
            1.0,
        );
    }
    for row in 0..=rows {
        let y = rect.pos().y + row as f32 * SPACING;
        append_line_quad(
            &mut vertices,
            &mut indices,
            Vec2::new(rect.pos().x, y),
            Vec2::new(rect.max().x, y),
            line,
            1.0,
        );
    }
    if !vertices.is_empty() {
        ctx.paint
            .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
    }
}

/// Yakui merges consecutive meshes with the same pipeline, texture and clip
/// into one draw call whose indices are `u16`, and panics once that call passes
/// 65,536 vertices. A scene with more than ~16k visible faces reaches that, so
/// paint model geometry through this: it starts a fresh call before the current
/// one would overflow, using an invisible zero-area mesh on the other pipeline
/// as the separator.
#[derive(Default)]
struct MeshBudget {
    merged_vertices: usize,
}

impl MeshBudget {
    /// Headroom below 65,536 for the small meshes (bounds, cursor) painted after
    /// the bulk geometry.
    const MAX_MERGED: usize = 60_000;

    fn add(
        &mut self,
        ctx: &mut yakui::widget::PaintContext<'_>,
        vertices: Vec<yakui::paint::Vertex>,
        indices: Vec<u16>,
    ) {
        if vertices.is_empty() {
            return;
        }
        debug_assert!(
            vertices.len() <= Self::MAX_MERGED,
            "one mesh must fit a call"
        );
        if self.merged_vertices + vertices.len() > Self::MAX_MERGED {
            let clear = Vec4::ZERO;
            let vertex = |x: f32| yakui::paint::Vertex::new(Vec2::new(x, 0.0), Vec2::ZERO, clear);
            ctx.paint.add_mesh(yakui::paint::PaintMesh {
                vertices: [vertex(0.0), vertex(0.0), vertex(0.0)],
                indices: [0_u16, 1, 2],
                texture: None,
                pipeline: yakui::paint::Pipeline::Text,
            });
            self.merged_vertices = 0;
        }
        self.merged_vertices += vertices.len();
        ctx.paint
            .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
    }
}

fn paint_asset_model(
    asset: &spall_editor::VoxelAssetFile,
    camera: PreviewCamera,
    environment: PreviewEnvironment,
    cursor: Option<Vec2>,
    hover_cell: Option<Arc<std::sync::Mutex<Option<VoxelCoord>>>>,
    selected_voxel: Option<VoxelCoord>,
    show_voxel_boundaries: bool,
    asset_bounds: Option<VoxelBounds>,
    show_asset_bounds: bool,
    ctx: &mut yakui::widget::PaintContext<'_>,
) {
    let voxel_bounds = voxel_extents(asset).map(|(min, max)| VoxelBounds::new(min, max));
    let fit_bounds = if show_asset_bounds {
        match (voxel_bounds, asset_bounds) {
            (Some(voxels), Some(bounds)) => Some(VoxelBounds::new(
                VoxelCoord {
                    x: voxels.min.x.min(bounds.min.x),
                    y: voxels.min.y.min(bounds.min.y),
                    z: voxels.min.z.min(bounds.min.z),
                },
                VoxelCoord {
                    x: voxels.max.x.max(bounds.max.x),
                    y: voxels.max.y.max(bounds.max.y),
                    z: voxels.max.z.max(bounds.max.z),
                },
            )),
            (Some(voxels), None) => Some(voxels),
            (None, bounds) => bounds,
        }
    } else {
        voxel_bounds
    };
    let Some(bounds) = fit_bounds else {
        return;
    };
    let (min, max) = (bounds.min, bounds.max);
    let widget = ctx.dom.current();
    let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
        return;
    };
    let (sin_yaw, cos_yaw) = camera.yaw.sin_cos();
    let (sin_pitch, cos_pitch) = camera.pitch.sin_cos();
    let rotate = |x: f32, y: f32, z: f32| {
        let horizontal = x * cos_yaw - z * sin_yaw;
        let depth = x * sin_yaw + z * cos_yaw;
        (horizontal, depth, y)
    };
    let project = |x: f32, y: f32, z: f32| {
        let (horizontal, depth, vertical) = rotate(x, y, z);
        Vec2::new(horizontal, depth * sin_pitch - vertical * cos_pitch)
    };
    let corners = [
        (min.x, min.y, min.z),
        (max.x.saturating_add(1), min.y, min.z),
        (min.x, max.y.saturating_add(1), min.z),
        (min.x, min.y, max.z.saturating_add(1)),
        (max.x.saturating_add(1), max.y.saturating_add(1), min.z),
        (max.x.saturating_add(1), min.y, max.z.saturating_add(1)),
        (min.x, max.y.saturating_add(1), max.z.saturating_add(1)),
        (
            max.x.saturating_add(1),
            max.y.saturating_add(1),
            max.z.saturating_add(1),
        ),
    ];
    let projected: Vec<_> = corners
        .iter()
        .map(|&(x, y, z)| project(x as f32, y as f32, z as f32))
        .collect();
    let min_x = projected.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
    let max_x = projected
        .iter()
        .map(|p| p.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = projected.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let max_y = projected
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max);
    // Fit and center from the 3D bounds rather than their projection. The
    // projected box changes size and position as the camera orbits, which made
    // the model zoom and drift while dragging. The bounding sphere and the
    // bounds' center are the same from every angle, so orbiting only rotates.
    let span = |lo: i32, hi: i32| (i64::from(hi) - i64::from(lo) + 1) as f32;
    let (span_x, span_y, span_z) = (span(min.x, max.x), span(min.y, max.y), span(min.z, max.z));
    let diameter: f32 = (span_x * span_x + span_y * span_y + span_z * span_z).sqrt();
    let fit_scale =
        ((rect.size().x.min(rect.size().y) - 40.0).max(1.0) / diameter.max(1.0)).clamp(0.01, 48.0);
    let scale = fit_scale * camera.zoom;
    let pan = clamp_preview_pan(
        camera.pan,
        Vec2::new((max_x - min_x) * scale, (max_y - min_y) * scale),
        rect.size(),
    );
    let center = (
        (min.x as f32 + max.x as f32 + 1.0) * 0.5,
        (min.y as f32 + max.y as f32 + 1.0) * 0.5,
        (min.z as f32 + max.z as f32 + 1.0) * 0.5,
    );
    let origin = rect.center() - project(center.0, center.1, center.2) * scale + pan;
    let screen = |x: f32, y: f32, z: f32| origin + project(x, y, z) * scale;

    let mut cells: Vec<_> = asset.voxels.keys().copied().collect();
    cells.sort_by(|a, b| {
        let depth = |cell: &VoxelCoord| {
            let (_, d, y) = rotate(
                cell.x as f32 + 0.5,
                cell.y as f32 + 0.5,
                cell.z as f32 + 0.5,
            );
            d * cos_pitch + y * sin_pitch
        };
        depth(a).total_cmp(&depth(b))
    });
    let mut hovered_cell = None;
    let mut budget = MeshBudget::default();
    for batch in cells.chunks(3000) {
        let mut vertices = Vec::with_capacity(batch.len() * 36);
        let mut indices = Vec::with_capacity(batch.len() * 54);
        for cell in batch {
            let Some(state) = asset.state_at(*cell) else {
                continue;
            };
            let cell = *cell;
            let faces = voxel_faces(cell, sin_yaw, cos_yaw, sin_pitch, cos_pitch);
            for (neighbor, corners, brightness, normal) in faces {
                if normal <= 0.0 || asset.state_at(neighbor).is_some() {
                    continue;
                }
                let points = corners.map(|(px, py, pz)| screen(px, py, pz));
                if let Some(cursor) = cursor
                    && point_in_quad(cursor, points)
                {
                    hovered_cell = Some(cell);
                }
                let base = vertices.len() as u16;
                let (ambient, direct, tint) = preview_lighting(environment);
                let light = ambient + direct * brightness;
                let color = Vec4::new(
                    f32::from(state.color[0]) / 255.0 * light * tint[0],
                    f32::from(state.color[1]) / 255.0 * light * tint[1],
                    f32::from(state.color[2]) / 255.0 * light * tint[2],
                    1.0,
                );
                vertices.extend(
                    points.map(|point| yakui::paint::Vertex::new(point, Vec2::ZERO, color)),
                );
                indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
        budget.add(ctx, vertices, indices);
    }
    if let Some(hover_cell) = hover_cell
        && let Ok(mut current) = hover_cell.lock()
    {
        *current = hovered_cell;
    }

    if show_voxel_boundaries {
        for batch in cells.chunks(1000) {
            let mut vertices = Vec::with_capacity(batch.len() * 48);
            let mut indices = Vec::with_capacity(batch.len() * 72);
            for &cell in batch {
                for (neighbor, corners, _, normal) in
                    voxel_faces(cell, sin_yaw, cos_yaw, sin_pitch, cos_pitch)
                {
                    if normal <= 0.0 || asset.state_at(neighbor).is_some() {
                        continue;
                    }
                    let points = corners.map(|(x, y, z)| screen(x, y, z));
                    for edge in 0..4 {
                        append_line_quad(
                            &mut vertices,
                            &mut indices,
                            points[edge],
                            points[(edge + 1) % 4],
                            Vec4::new(0.025, 0.035, 0.05, 0.95),
                            1.25,
                        );
                    }
                }
            }
            budget.add(ctx, vertices, indices);
        }
    }

    if show_asset_bounds && let Some(bounds) = asset_bounds {
        draw_wire_box(
            bounds.min,
            bounds.max,
            &screen,
            Vec4::new(0.16, 0.88, 1.0, 0.95),
            1.8,
            ctx,
        );
    }

    if let Some(cell) = selected_voxel {
        draw_wire_box(
            cell,
            cell,
            &screen,
            Vec4::new(1.0, 0.82, 0.16, 1.0),
            2.5,
            ctx,
        );
    }

    if let Some(cursor) = cursor.filter(|point| {
        point.x >= rect.pos().x
            && point.x <= rect.max().x
            && point.y >= rect.pos().y
            && point.y <= rect.max().y
    }) {
        let color = Vec4::new(1.0, 0.92, 0.35, 1.0);
        let mut vertices = Vec::with_capacity(8);
        let mut indices = Vec::with_capacity(12);
        for (x, y, width, height) in [
            (cursor.x - 10.0, cursor.y - 1.0, 20.0, 2.0),
            (cursor.x - 1.0, cursor.y - 10.0, 2.0, 20.0),
        ] {
            let base = vertices.len() as u16;
            vertices.extend([
                yakui::paint::Vertex::new(Vec2::new(x, y), Vec2::ZERO, color),
                yakui::paint::Vertex::new(Vec2::new(x + width, y), Vec2::ZERO, color),
                yakui::paint::Vertex::new(Vec2::new(x + width, y + height), Vec2::ZERO, color),
                yakui::paint::Vertex::new(Vec2::new(x, y + height), Vec2::ZERO, color),
            ]);
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        ctx.paint
            .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
    }
}

fn voxel_faces(
    cell: VoxelCoord,
    sin_yaw: f32,
    cos_yaw: f32,
    sin_pitch: f32,
    cos_pitch: f32,
) -> [(VoxelCoord, [(f32, f32, f32); 4], f32, f32); 6] {
    let (x, y, z) = (cell.x as f32, cell.y as f32, cell.z as f32);
    [
        (
            VoxelCoord {
                x: cell.x,
                y: cell.y + 1,
                z: cell.z,
            },
            [
                (x, y + 1.0, z),
                (x + 1.0, y + 1.0, z),
                (x + 1.0, y + 1.0, z + 1.0),
                (x, y + 1.0, z + 1.0),
            ],
            1.0,
            sin_pitch,
        ),
        (
            VoxelCoord {
                x: cell.x,
                y: cell.y - 1,
                z: cell.z,
            },
            [
                (x, y, z + 1.0),
                (x + 1.0, y, z + 1.0),
                (x + 1.0, y, z),
                (x, y, z),
            ],
            0.45,
            -sin_pitch,
        ),
        (
            VoxelCoord {
                x: cell.x + 1,
                y: cell.y,
                z: cell.z,
            },
            [
                (x + 1.0, y, z),
                (x + 1.0, y + 1.0, z),
                (x + 1.0, y + 1.0, z + 1.0),
                (x + 1.0, y, z + 1.0),
            ],
            0.78,
            sin_yaw * cos_pitch,
        ),
        (
            VoxelCoord {
                x: cell.x - 1,
                y: cell.y,
                z: cell.z,
            },
            [
                (x, y, z + 1.0),
                (x, y + 1.0, z + 1.0),
                (x, y + 1.0, z),
                (x, y, z),
            ],
            0.58,
            -sin_yaw * cos_pitch,
        ),
        (
            VoxelCoord {
                x: cell.x,
                y: cell.y,
                z: cell.z + 1,
            },
            [
                (x, y, z + 1.0),
                (x + 1.0, y, z + 1.0),
                (x + 1.0, y + 1.0, z + 1.0),
                (x, y + 1.0, z + 1.0),
            ],
            0.86,
            cos_yaw * cos_pitch,
        ),
        (
            VoxelCoord {
                x: cell.x,
                y: cell.y,
                z: cell.z - 1,
            },
            [
                (x, y + 1.0, z),
                (x + 1.0, y + 1.0, z),
                (x + 1.0, y, z),
                (x, y, z),
            ],
            0.64,
            -cos_yaw * cos_pitch,
        ),
    ]
}

fn draw_wire_box(
    min: VoxelCoord,
    max: VoxelCoord,
    project: &impl Fn(f32, f32, f32) -> Vec2,
    color: Vec4,
    thickness: f32,
    ctx: &mut yakui::widget::PaintContext<'_>,
) {
    let corners = [
        (min.x, min.y, min.z),
        (max.x.saturating_add(1), min.y, min.z),
        (min.x, max.y.saturating_add(1), min.z),
        (min.x, min.y, max.z.saturating_add(1)),
        (max.x.saturating_add(1), max.y.saturating_add(1), min.z),
        (max.x.saturating_add(1), min.y, max.z.saturating_add(1)),
        (min.x, max.y.saturating_add(1), max.z.saturating_add(1)),
        (
            max.x.saturating_add(1),
            max.y.saturating_add(1),
            max.z.saturating_add(1),
        ),
    ]
    .map(|(x, y, z)| project(x as f32, y as f32, z as f32));
    let edges = [
        (0, 1),
        (0, 2),
        (0, 3),
        (1, 4),
        (1, 5),
        (2, 4),
        (2, 6),
        (3, 5),
        (3, 6),
        (4, 7),
        (5, 7),
        (6, 7),
    ];
    let mut vertices = Vec::with_capacity(edges.len() * 4);
    let mut indices = Vec::with_capacity(edges.len() * 6);
    for (a, b) in edges {
        append_line_quad(
            &mut vertices,
            &mut indices,
            corners[a],
            corners[b],
            color,
            thickness,
        );
    }
    ctx.paint
        .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
}

fn point_in_quad(point: Vec2, quad: [Vec2; 4]) -> bool {
    fn in_triangle(point: Vec2, a: Vec2, b: Vec2, c: Vec2) -> bool {
        let ab = (b - a).perp_dot(point - a);
        let bc = (c - b).perp_dot(point - b);
        let ca = (a - c).perp_dot(point - c);
        (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
    }

    in_triangle(point, quad[0], quad[1], quad[2]) || in_triangle(point, quad[0], quad[2], quad[3])
}

fn append_line_quad(
    vertices: &mut Vec<yakui::paint::Vertex>,
    indices: &mut Vec<u16>,
    start: Vec2,
    end: Vec2,
    color: Vec4,
    thickness: f32,
) {
    let direction = end - start;
    let length = direction.length();
    if length <= f32::EPSILON {
        return;
    }
    let offset = Vec2::new(-direction.y, direction.x) * (thickness * 0.5 / length);
    let base = vertices.len() as u16;
    vertices.extend([
        yakui::paint::Vertex::new(start - offset, Vec2::ZERO, color),
        yakui::paint::Vertex::new(end - offset, Vec2::ZERO, color),
        yakui::paint::Vertex::new(end + offset, Vec2::ZERO, color),
        yakui::paint::Vertex::new(start + offset, Vec2::ZERO, color),
    ]);
    indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn clamp_preview_pan(pan: Vec2, content: Vec2, viewport: Vec2) -> Vec2 {
    let limit = Vec2::new(
        ((content.x - viewport.x + 32.0) * 0.5).max(0.0),
        ((content.y - viewport.y + 32.0) * 0.5).max(0.0),
    );
    Vec2::new(
        pan.x.clamp(-limit.x, limit.x),
        pan.y.clamp(-limit.y, limit.y),
    )
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
                        .with_inner_size(PhysicalSize::new(1920, 1080))
                        .with_maximized(true),
                )
                .expect("create editor window"),
        );
        let mut input = yakui_winit::YakuiWinit::new(&window);
        input.set_automatic_scale_factor(false);
        self.yakui_input = Some(input);
        self.gpu = EngineGpu::new(window.clone()).ok();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.sync_ui_surface(window.inner_size(), window.scale_factor() as f32);
        }
        self.window = Some(window);
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((last_x, last_y)) = self.preview_cursor
                    && let Some(drag) = self.preview_drag
                {
                    let dx = (position.x - last_x) as f32;
                    let dy = (position.y - last_y) as f32;
                    match drag {
                        PreviewDrag::Orbit => {
                            self.preview_camera.yaw += dx * 0.008;
                            let pitch = self.preview_camera.pitch + dy * 0.008;
                            // The scene's perspective orbit stops at the poles;
                            // the asset preview keeps wrapping.
                            self.preview_camera.pitch = if self.workspace == Workspace::Scene {
                                pitch.clamp(
                                    -scene_viewport::ORBIT_PITCH_LIMIT,
                                    scene_viewport::ORBIT_PITCH_LIMIT,
                                )
                            } else {
                                pitch.rem_euclid(std::f32::consts::TAU)
                            };
                        }
                        PreviewDrag::Pan => {
                            self.preview_camera.pan += Vec2::new(dx, dy);
                        }
                    }
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                if let (Some(drag), Some(window)) = (self.panel_drag, self.window.as_ref()) {
                    let dpi = window.scale_factor() as f32;
                    let size = window.inner_size();
                    match drag {
                        PanelDrag::Left => {
                            self.left_panel_width =
                                (position.x as f32 / dpi).clamp(theme::PANEL_MIN_WIDTH, 520.0)
                        }
                        PanelDrag::Right => {
                            self.right_panel_width = ((size.width as f32 - position.x as f32) / dpi)
                                .clamp(theme::PANEL_MIN_WIDTH, 560.0)
                        }
                        PanelDrag::Bottom => {
                            self.bottom_panel_height = ((size.height as f32 - position.y as f32)
                                / dpi
                                - theme::STATUS_BAR_HEIGHT)
                                .clamp(140.0, 560.0)
                        }
                    }
                    window.request_redraw();
                }
                let hover = self.panel_edge_at(position.x, position.y);
                if hover != self.panel_hover {
                    self.panel_hover = hover;
                    if let Some(window) = &self.window {
                        window.set_cursor(match hover {
                            Some(PanelDrag::Left | PanelDrag::Right) => {
                                winit::window::CursorIcon::EwResize
                            }
                            Some(PanelDrag::Bottom) => winit::window::CursorIcon::NsResize,
                            None => winit::window::CursorIcon::Default,
                        });
                        window.request_redraw();
                    }
                }
                self.preview_cursor = Some((position.x, position.y));
            }
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                state,
                ..
            } => {
                if *state == ElementState::Released {
                    self.panel_drag = None;
                } else if let Some((x, y)) = self.preview_cursor {
                    if let Some(edge) = self.panel_edge_at(x, y) {
                        self.panel_drag = Some(edge);
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    } else if self.pointer_in_viewport() {
                        self.viewport_click();
                    }
                }
            }
            WindowEvent::MouseInput {
                button: MouseButton::Middle,
                state,
                ..
            } => {
                self.preview_drag = if *state == ElementState::Pressed && self.pointer_in_viewport()
                {
                    Some(if self.shift_held {
                        PreviewDrag::Pan
                    } else {
                        PreviewDrag::Orbit
                    })
                } else {
                    None
                };
            }
            WindowEvent::MouseWheel { delta, .. } if self.pointer_in_viewport() => {
                let steps = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(position) => position.y as f32 / 40.0,
                };
                self.preview_camera.zoom =
                    (self.preview_camera.zoom * 1.12_f32.powf(steps)).clamp(0.12, 8.0);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.shift_held = modifiers.state().shift_key();
                self.ctrl_held = modifiers.state().control_key();
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && self.ctrl_held
                    && matches!(
                        event.physical_key,
                        winit::keyboard::PhysicalKey::Code(
                            winit::keyboard::KeyCode::KeyS
                                | winit::keyboard::KeyCode::KeyZ
                                | winit::keyboard::KeyCode::KeyY
                        )
                    ) =>
            {
                // Leave Ctrl+Z / Ctrl+Y to the focused text box; Ctrl+S always saves.
                let typing = self
                    .gpu
                    .as_ref()
                    .is_some_and(|gpu| gpu.state.text_input_enabled());
                match event.physical_key {
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyS) => {
                        if self.model.is_some() {
                            self.save();
                        }
                    }
                    _ if typing => {}
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyZ)
                        if !self.shift_held =>
                    {
                        self.undo_edit()
                    }
                    _ => self.redo_edit(),
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.physical_key
                        == winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape) =>
            {
                self.close_menus();
                self.modal = None;
            }
            WindowEvent::KeyboardInput { event, .. }
                if self.modal.is_none()
                    && event.state == ElementState::Pressed
                    && !self
                        .gpu
                        .as_ref()
                        .is_some_and(|gpu| gpu.state.text_input_enabled())
                    && matches!(
                        event.physical_key,
                        winit::keyboard::PhysicalKey::Code(
                            winit::keyboard::KeyCode::KeyW
                                | winit::keyboard::KeyCode::KeyA
                                | winit::keyboard::KeyCode::KeyS
                                | winit::keyboard::KeyCode::KeyD
                                | winit::keyboard::KeyCode::KeyQ
                                | winit::keyboard::KeyCode::KeyE
                        )
                    ) =>
            {
                let delta = match event.physical_key {
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyW) => {
                        Some((0, 0, -1))
                    }
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyS) => {
                        Some((0, 0, 1))
                    }
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyA) => {
                        Some((-1, 0, 0))
                    }
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyD) => {
                        Some((1, 0, 0))
                    }
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyQ) => {
                        Some((0, -1, 0))
                    }
                    winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyE) => {
                        Some((0, 1, 0))
                    }
                    _ => None,
                };
                if let Some((dx, dy, dz)) = delta {
                    let mut cell = self.selected_voxel.unwrap_or_else(|| {
                        int3(&self.coord).map_or(VoxelCoord { x: 0, y: 0, z: 0 }, |[x, y, z]| {
                            VoxelCoord { x, y, z }
                        })
                    });
                    cell.x = cell.x.saturating_add(dx);
                    cell.y = cell.y.saturating_add(dy);
                    cell.z = cell.z.saturating_add(dz);
                    if let Some(bounds) = self
                        .selected_asset
                        .filter(|_| self.workspace == Workspace::Assets)
                        .and_then(|id| {
                            self.model
                                .as_ref()?
                                .project
                                .asset_database
                                .assets
                                .get(&id)?
                                .authoring_bounds
                        })
                    {
                        cell.x = cell.x.clamp(bounds.min.x, bounds.max.x);
                        cell.y = cell.y.clamp(bounds.min.y, bounds.max.y);
                        cell.z = cell.z.clamp(bounds.min.z, bounds.max.z);
                    }
                    self.selected_voxel = Some(cell);
                    self.coord = [cell.x.to_string(), cell.y.to_string(), cell.z.to_string()];
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.physical_key
                        == winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Home) =>
            {
                self.preview_camera = PreviewCamera::default();
            }
            _ => {}
        }
        if let (Some(input), Some(gpu)) = (self.yakui_input.as_mut(), self.gpu.as_mut()) {
            let _ = input.handle_window_event(&mut gpu.state, &event);
        }
        if let (Some(window), Some(gpu)) = (self.window.as_ref(), self.gpu.as_mut()) {
            gpu.sync_ui_surface(window.inner_size(), window.scale_factor() as f32);
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gpu.as_mut()
                    && g.resize(size)
                {
                    self.status = format!(
                        "Window exceeds adapter texture limit; rendering at {}×{}",
                        g.config.width, g.config.height
                    );
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.exit_requested {
            event_loop.exit();
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::*;

    /// A quad-soup mesh of `quads` separate quads, like one batch of voxel faces.
    fn quads(quads: usize) -> (Vec<yakui::paint::Vertex>, Vec<u16>) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for _ in 0..quads {
            let base = vertices.len() as u16;
            vertices.extend(
                (0..4).map(|_| yakui::paint::Vertex::new(Vec2::ZERO, Vec2::ZERO, Vec4::ONE)),
            );
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        (vertices, indices)
    }

    /// Paints eight 30k-vertex batches (240k vertices, like a scene with ~60k
    /// visible faces) and returns the largest draw call yakui produced.
    fn largest_call_after_painting(through_budget: bool) -> (usize, usize) {
        let mut yak = yakui::Yakui::new();
        yak.set_surface_size(Vec2::new(800.0, 600.0));
        yak.set_unscaled_viewport(yakui::geometry::Rect::from_pos_size(
            Vec2::ZERO,
            Vec2::new(800.0, 600.0),
        ));
        yak.start();
        yakui::canvas(move |ctx| {
            let mut budget = MeshBudget::default();
            for _ in 0..8 {
                let (vertices, indices) = quads(7_500);
                if through_budget {
                    budget.add(ctx, vertices, indices);
                } else {
                    ctx.paint
                        .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
                }
            }
        });
        yak.finish();
        let paint = yak.paint();
        let calls: Vec<_> = paint
            .layers()
            .iter()
            .flat_map(|layer| layer.calls.iter())
            .collect();
        (
            calls
                .iter()
                .map(|call| call.vertices.len())
                .max()
                .unwrap_or(0),
            calls.len(),
        )
    }

    #[test]
    #[should_panic(expected = "overflow")]
    fn unsplit_geometry_overflows_yakuis_u16_indices() {
        // Documents the failure MeshBudget prevents; if yakui ever fixes it this
        // test (and the workaround) can go.
        largest_call_after_painting(false);
    }

    #[test]
    fn mesh_budget_keeps_every_draw_call_within_u16_indices() {
        let (largest, calls) = largest_call_after_painting(true);
        assert!(
            largest <= usize::from(u16::MAX),
            "largest call has {largest} vertices"
        );
        assert!(calls >= 4, "240k vertices need several calls, got {calls}");
    }

    #[test]
    fn preview_pan_cannot_move_a_fitted_asset_out_of_the_viewport() {
        assert_eq!(
            clamp_preview_pan(
                Vec2::new(900.0, -900.0),
                Vec2::new(300.0, 200.0),
                Vec2::new(800.0, 600.0)
            ),
            Vec2::ZERO
        );
    }

    #[test]
    fn preview_pan_is_bounded_by_overflow_when_zoomed_in() {
        assert_eq!(
            clamp_preview_pan(
                Vec2::new(900.0, -900.0),
                Vec2::new(1200.0, 900.0),
                Vec2::new(800.0, 600.0)
            ),
            Vec2::new(216.0, -166.0)
        );
    }
}
