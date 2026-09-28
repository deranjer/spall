//! Editor screens composed from the widgets in `theme`: top bar with menus,
//! start screen, the asset "Editor" workspace and the "Scene" workspace.

use super::*;
use crate::theme::{
    self, ButtonKind, ButtonSize, DropdownSide, Icon, MenuEntry, STATUS_BAR_HEIGHT, TOP_BAR_HEIGHT,
};
use crate::thumbnail::{ThumbMesh, thumbnail};
use spall_voxel::builtin_assets::BuiltinVoxelAsset;
use yakui::widgets::{List, Pad, RoundRect};
use yakui::{Alignment, Color, Constraints, CrossAxisAlignment};

const CARD_WIDTH: f32 = 120.0;
const CARD_GAP: f32 = 14.0;
const CARD_THUMB: f32 = 60.0;

/// Dialogs opened from the File menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modal {
    NewAsset,
    ImportExport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SceneTab {
    #[default]
    Object,
    Voxels,
}

/// The scene flattened for drawing, plus which entity owns each cell.
pub(crate) type SceneView = (
    spall_editor::VoxelAssetFile,
    std::collections::BTreeMap<VoxelCoord, EditorEntityId>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RightTab {
    #[default]
    Layer,
    Material,
    Bounds,
}

#[derive(Debug, Clone, Copy)]
enum MenuAction {
    NewProject,
    OpenProject,
    NewAsset,
    ImportExport,
    Save,
    RunScene,
    Exit,
    Undo,
    Redo,
    ToggleLeftPanel,
    ToggleRightPanel,
    ToggleLibrary,
    ToggleVoxelBoundaries,
    ToggleAssetBounds,
    ViewportHelp,
}

/// Everything the asset viewport needs, gathered before the UI closures run.
struct PreviewState {
    asset: Option<spall_editor::VoxelAssetFile>,
    bounds: Option<VoxelBounds>,
    cursor: Option<Vec2>,
    hover: Option<VoxelCoord>,
}

/// Library entry shown as a card.
enum LibraryItem {
    Builtin(BuiltinVoxelAsset, Arc<ThumbMesh>),
    Project(AssetId, String, Option<Arc<ThumbMesh>>),
}

impl EditorApp {
    pub(crate) fn undo_edit(&mut self) {
        if !self.undo.can_undo() {
            return;
        }
        self.scene_view = None;
        if let Some(model) = self.model.as_mut()
            && let Err(error) = self.undo.undo(model)
        {
            self.status = error.to_string();
        }
        self.refresh_project_thumbnails();
    }

    pub(crate) fn redo_edit(&mut self) {
        if !self.undo.can_redo() {
            return;
        }
        self.scene_view = None;
        if let Some(model) = self.model.as_mut()
            && let Err(error) = self.undo.redo(model)
        {
            self.status = error.to_string();
        }
        self.refresh_project_thumbnails();
    }

    /// Whether a panel divider should be highlighted (hovered or dragged).
    fn handle_hot(&self, edge: PanelDrag) -> bool {
        self.panel_hover == Some(edge) || self.panel_drag == Some(edge)
    }

    pub(crate) fn close_menus(&mut self) {
        self.open_menu = None;
        self.environment_menu_open = false;
        self.view_menu_open = false;
    }

    pub(crate) fn ui(&mut self) {
        yakui::expanded(|| {
            yakui::colored_box_container(theme::BG_APP, || {
                List::column()
                    .cross_axis_alignment(CrossAxisAlignment::Stretch)
                    .show(|| {
                        self.top_bar();
                        if self.model.is_some() {
                            self.workspace_ui();
                        } else {
                            self.start_ui();
                        }
                        self.status_bar();
                    });
            });
        });
        self.modal_ui();
    }

    // ---- chrome -----------------------------------------------------------

    fn top_bar(&mut self) {
        let has_model = self.model.is_some();
        let height = TOP_BAR_HEIGHT - 1.0;
        yakui::constrained(Constraints::height(height, height), || {
            yakui::colored_box_container(theme::BG_TOPBAR, || {
                yakui::pad(Pad::horizontal(16.0), || {
                    yakui::align(Alignment::CENTER_LEFT, || {
                        List::row()
                            .item_spacing(18.0)
                            .cross_axis_alignment(CrossAxisAlignment::Center)
                            .show(|| {
                                theme::label(14.0, theme::TEXT, "Spall Editor");
                                self.menu_strip(has_model);
                                yakui::expanded(|| {});
                                if has_model {
                                    self.quick_actions();
                                }
                            });
                    });
                });
            });
        });
        theme::hline(theme::LINE);
    }

    fn menu_strip(&mut self, has_model: bool) {
        List::row()
            .item_spacing(2.0)
            .main_axis_size(yakui::MainAxisSize::Min)
            .cross_axis_alignment(CrossAxisAlignment::Center)
            .show(|| {
                for (menu, name) in [
                    (Menu::File, "File"),
                    (Menu::Edit, "Edit"),
                    (Menu::View, "View"),
                ] {
                    self.menu_header(menu, name);
                }
                self.menu_header(Menu::Help, "Help");
                if has_model {
                    self.workspace_tabs();
                }
            });
    }

    /// Transient workspace tabs, placed after the permanent menus and styled
    /// as pills so they read as document tabs rather than menus.
    fn workspace_tabs(&mut self) {
        yakui::pad(Pad::horizontal(10.0), || {
            theme::hstack(6.0, || {
                if !self.engine_asset_session
                    && theme::workspace_tab("Scene", self.workspace == Workspace::Scene)
                {
                    self.workspace = Workspace::Scene;
                    self.left_panel_open = true;
                    self.close_menus();
                }
                if theme::workspace_tab("Assets", self.workspace == Workspace::Assets) {
                    self.workspace = Workspace::Assets;
                    self.left_panel_open = true;
                    self.bottom_panel_open = true;
                    self.close_menus();
                }
            });
        });
    }

    fn menu_header(&mut self, menu: Menu, name: &str) {
        List::column()
            .main_axis_size(yakui::MainAxisSize::Min)
            .show(|| {
                let open = self.open_menu == Some(menu);
                if theme::menu_button(name, open) {
                    self.environment_menu_open = false;
                    self.view_menu_open = false;
                    self.open_menu = (!open).then_some(menu);
                }
                if open {
                    let items = self.menu_entries(menu);
                    let entries: Vec<_> = items.iter().map(|(entry, _)| entry).collect();
                    if let Some(index) = theme::dropdown_from(DropdownSide::Left, 240.0, &entries) {
                        let action = items[index].1;
                        self.open_menu = None;
                        self.apply_menu_action(action);
                    }
                }
            });
    }

    fn menu_entries(&self, menu: Menu) -> Vec<(MenuEntry<'static>, MenuAction)> {
        let has_model = self.model.is_some();
        match menu {
            Menu::File => vec![
                (MenuEntry::new("New Project…"), MenuAction::NewProject),
                (
                    MenuEntry::new("Open Project or Scene…"),
                    MenuAction::OpenProject,
                ),
                (
                    MenuEntry::new("New Asset…").enabled(has_model).separated(),
                    MenuAction::NewAsset,
                ),
                (
                    MenuEntry::new("Import / Export…").enabled(has_model),
                    MenuAction::ImportExport,
                ),
                (
                    MenuEntry::new("Save")
                        .shortcut("Ctrl+S")
                        .enabled(has_model)
                        .separated(),
                    MenuAction::Save,
                ),
                (
                    MenuEntry::new("Run Scene").enabled(has_model && !self.engine_asset_session),
                    MenuAction::RunScene,
                ),
                (
                    MenuEntry::new("Exit").danger().separated(),
                    MenuAction::Exit,
                ),
            ],
            Menu::Edit => vec![
                (
                    MenuEntry::new("Undo")
                        .shortcut("Ctrl+Z")
                        .enabled(self.undo.can_undo()),
                    MenuAction::Undo,
                ),
                (
                    MenuEntry::new("Redo")
                        .shortcut("Ctrl+Y")
                        .enabled(self.undo.can_redo()),
                    MenuAction::Redo,
                ),
            ],
            Menu::View => vec![
                (
                    MenuEntry::new("Left panel")
                        .toggle(self.left_panel_open)
                        .enabled(has_model),
                    MenuAction::ToggleLeftPanel,
                ),
                (
                    MenuEntry::new("Right panel")
                        .toggle(self.right_panel_open)
                        .enabled(has_model),
                    MenuAction::ToggleRightPanel,
                ),
                (
                    MenuEntry::new("Asset library")
                        .toggle(self.bottom_panel_open)
                        .enabled(has_model),
                    MenuAction::ToggleLibrary,
                ),
                (
                    MenuEntry::new("Voxel boundaries")
                        .toggle(self.show_voxel_boundaries)
                        .separated(),
                    MenuAction::ToggleVoxelBoundaries,
                ),
                (
                    MenuEntry::new("Asset bounds").toggle(self.show_asset_bounds),
                    MenuAction::ToggleAssetBounds,
                ),
            ],
            Menu::Help => vec![(
                MenuEntry::new("Viewport controls"),
                MenuAction::ViewportHelp,
            )],
        }
    }

    fn apply_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::NewProject => self.new_project_dialog(),
            MenuAction::OpenProject => self.open_project_dialog(),
            MenuAction::NewAsset => self.modal = Some(Modal::NewAsset),
            MenuAction::ImportExport => self.modal = Some(Modal::ImportExport),
            MenuAction::Save => self.save(),
            MenuAction::RunScene => self.run_scene(),
            MenuAction::Exit => self.exit_requested = true,
            MenuAction::Undo => self.undo_edit(),
            MenuAction::Redo => self.redo_edit(),
            MenuAction::ToggleLeftPanel => self.left_panel_open = !self.left_panel_open,
            MenuAction::ToggleRightPanel => self.right_panel_open = !self.right_panel_open,
            MenuAction::ToggleLibrary => self.bottom_panel_open = !self.bottom_panel_open,
            MenuAction::ToggleVoxelBoundaries => {
                self.show_voxel_boundaries = !self.show_voxel_boundaries;
            }
            MenuAction::ToggleAssetBounds => self.show_asset_bounds = !self.show_asset_bounds,
            MenuAction::ViewportHelp => {
                self.status = "Click: inspect cell · Orbit: middle-drag · Pan: Shift + middle-drag · Zoom: wheel · Reset: Home · Move pointer: W/S/A/D/Q/E".into();
            }
        }
    }

    fn quick_actions(&mut self) {
        theme::hstack(6.0, || {
            if theme::button(ButtonKind::Secondary, ButtonSize::Small, "Save") {
                self.save();
            }
            if theme::button(ButtonKind::Secondary, ButtonSize::Small, "Undo") {
                self.undo_edit();
            }
            if theme::button(ButtonKind::Secondary, ButtonSize::Small, "Redo") {
                self.redo_edit();
            }
            if self.workspace == Workspace::Scene
                && !self.engine_asset_session
                && theme::button(ButtonKind::Primary, ButtonSize::Small, "Run")
            {
                self.run_scene();
            }
        });
    }

    fn status_bar(&self) {
        yakui::constrained(
            Constraints::height(STATUS_BAR_HEIGHT, STATUS_BAR_HEIGHT),
            || {
                yakui::colored_box_container(theme::BG_TOPBAR, || {
                    yakui::pad(Pad::horizontal(16.0), || {
                        yakui::align(Alignment::CENTER_LEFT, || {
                            theme::label(11.0, theme::TEXT_MUTED, self.status.clone());
                        });
                    });
                });
            },
        );
    }

    // ---- start screen -----------------------------------------------------

    fn start_ui(&mut self) {
        yakui::expanded(|| {
            List::row()
                .cross_axis_alignment(CrossAxisAlignment::Stretch)
                .show(|| {
                    yakui::constrained(Constraints::width(460.0, 460.0), || {
                        yakui::colored_box_container(theme::BG_LIBRARY, || {
                            yakui::scroll_vertical(|| {
                                yakui::pad(Pad::balanced(40.0, 48.0), || {
                                    theme::vstack(14.0, || self.start_panel());
                                });
                            });
                        });
                    });
                    theme::vline(theme::LINE);
                    yakui::expanded(|| {
                        yakui::colored_box_container(theme::BG_APP, || {
                            yakui::align(Alignment::CENTER, || {
                                theme::label(
                                    13.0,
                                    theme::TEXT_DIM,
                                    "Create or open a project to start authoring.",
                                );
                            });
                        });
                    });
                });
        });
    }

    fn start_panel(&mut self) {
        theme::vstack(4.0, || {
            theme::hstack(10.0, || {
                yakui::constrained(Constraints::tight(Vec2::splat(30.0)), || {
                    RoundRect::new(6.0).color(theme::ACCENT).show_children(|| {
                        yakui::pad(Pad::all(8.0), || {
                            RoundRect::new(2.0)
                                .color(theme::ACCENT_TEXT)
                                .show_children(|| {
                                    yakui::pad(Pad::all(2.0), || {
                                        RoundRect::new(1.0)
                                            .color(theme::ACCENT)
                                            .min_size(Vec2::splat(10.0))
                                            .show();
                                    });
                                });
                        });
                    });
                });
                theme::label(19.0, theme::TEXT, "Spall Editor");
            });
            theme::label(
                13.0,
                theme::TEXT_MUTED,
                "Build worlds, one voxel at a time.",
            );
        });
        yakui::pad(
            Pad {
                top: 20.0,
                ..Pad::ZERO
            },
            || {
                theme::vstack(12.0, || {
                    theme::vstack(5.0, || {
                        theme::label(11.0, theme::TEXT_LABEL, "Project folder");
                        List::row()
                            .item_spacing(8.0)
                            .cross_axis_alignment(CrossAxisAlignment::Center)
                            .show(|| {
                                yakui::expanded(|| {
                                    if let Some(text) =
                                        theme::input(&self.project_path, ".local/editor-project")
                                    {
                                        self.project_path = text;
                                    }
                                });
                                if theme::button(
                                    ButtonKind::Secondary,
                                    ButtonSize::Small,
                                    "Browse…",
                                ) {
                                    self.pick_project_folder("Choose a project folder");
                                }
                            });
                    });
                    theme::field_with_placeholder(
                        "Project name",
                        &mut self.project_name,
                        "Untitled",
                    );
                });
            },
        );
        if theme::button(ButtonKind::Primary, ButtonSize::Regular, "+  New Project") {
            self.new_project();
        }
        if theme::secondary("Open Project or Scene…") {
            self.open_project_dialog();
        }
        if theme::secondary("Browse Engine Assets") {
            self.open_engine_assets();
        }
        if !self.recents.is_empty() {
            yakui::pad(
                Pad {
                    top: 18.0,
                    ..Pad::ZERO
                },
                || {
                    theme::section_title("Recent projects");
                },
            );
            for root in self.recents.clone() {
                if theme::button(
                    ButtonKind::Ghost,
                    ButtonSize::Regular,
                    &root.display().to_string(),
                ) {
                    self.project_path = root.display().to_string();
                    self.open_project();
                }
            }
        }
    }

    // ---- workspaces -------------------------------------------------------

    fn workspace_ui(&mut self) {
        match self.workspace {
            Workspace::Scene => self.scene_ui(),
            Workspace::Assets => self.assets_ui(),
        }
    }

    // ---- asset editor -----------------------------------------------------

    /// Pointer position in surface pixels, and the voxel cell under it as
    /// found by the last paint. Both are `None` unless the pointer is over
    /// the viewport.
    fn viewport_pointer(&self) -> (Option<Vec2>, Option<VoxelCoord>) {
        let cursor = self
            .preview_cursor
            .filter(|_| self.pointer_in_viewport())
            .and_then(|(x, y)| {
                let window_size = self.window.as_ref()?.inner_size();
                let surface = &self.gpu.as_ref()?.config;
                Some(Vec2::new(
                    x as f32 * surface.width as f32 / window_size.width.max(1) as f32,
                    y as f32 * surface.height as f32 / window_size.height.max(1) as f32,
                ))
            });
        if cursor.is_none()
            && let Ok(mut cell) = self.preview_hover_cell.lock()
        {
            *cell = None;
        }
        let hover = self.preview_hover_cell.lock().ok().and_then(|cell| *cell);
        (cursor, hover)
    }

    /// Left click inside the viewport: pick the hovered cell and, with the
    /// scene's Select tool, the entity that owns it.
    pub(crate) fn viewport_click(&mut self) {
        let cell = self.preview_hover_cell.lock().ok().and_then(|cell| *cell);
        self.selected_voxel = cell;
        let Some(cell) = cell else {
            return;
        };
        self.coord = [cell.x.to_string(), cell.y.to_string(), cell.z.to_string()];
        if self.workspace == Workspace::Scene
            && self.scene_select
            && let Some(owner) = self
                .scene_view
                .as_ref()
                .and_then(|(_, owners)| owners.get(&cell).copied())
        {
            self.select_entity(owner);
        }
    }

    fn select_entity(&mut self, id: EditorEntityId) {
        let Some(entity) = self
            .model
            .as_ref()
            .and_then(|model| model.scene.entities.get(&id))
        else {
            return;
        };
        self.translation = entity.transform.translation.map(|v| v.to_string());
        self.rotation = entity.transform.rotation_degrees.map(|v| v.to_string());
        self.scale = entity.transform.scale.map(|v| v.to_string());
        self.selected_entity = Some(id);
    }

    fn preview_state(&self) -> PreviewState {
        let asset = self
            .selected_asset
            .and_then(|id| self.model.as_ref().and_then(|m| m.voxel_assets.get(&id)))
            .cloned()
            .or_else(|| self.builtin_preview.clone());
        let bounds = self
            .selected_asset
            .and_then(|id| {
                self.model
                    .as_ref()?
                    .project
                    .asset_database
                    .assets
                    .get(&id)?
                    .authoring_bounds
            })
            .or_else(|| {
                asset
                    .as_ref()
                    .and_then(voxel_extents)
                    .map(|(min, max)| VoxelBounds::new(min, max))
            });
        let (cursor, hover) = self.viewport_pointer();
        PreviewState {
            asset,
            bounds,
            cursor,
            hover,
        }
    }

    fn assets_ui(&mut self) {
        let preview = self.preview_state();
        yakui::expanded(|| {
            List::row()
                .cross_axis_alignment(CrossAxisAlignment::Stretch)
                .show(|| {
                    if self.left_panel_open {
                        self.asset_tools_panel();
                        theme::resize_handle(true, self.handle_hot(PanelDrag::Left));
                    }
                    yakui::expanded(|| self.asset_viewport(&preview));
                    if self.right_panel_open {
                        theme::resize_handle(true, self.handle_hot(PanelDrag::Right));
                        self.asset_right_panel();
                    }
                });
        });
        self.library_panel(false);
    }

    fn asset_tools_panel(&mut self) {
        let width = self.left_panel_width;
        yakui::constrained(Constraints::width(width, width), || {
            yakui::colored_box_container(theme::BG_PANEL, || {
                yakui::scroll_vertical(|| {
                    theme::vstack(0.0, || {
                        theme::section(|| {
                            theme::section_title("Voxel tool");
                            self.voxel_tool_buttons(false);
                            theme::hint(if self.chip_mode {
                                "Erase: click a cell, then Chip Voxel."
                            } else {
                                "Add: click a cell, then Add Voxel."
                            });
                        });
                        if let Some(id) = self.selected_asset {
                            theme::section(|| self.box_section(id));
                        } else {
                            theme::section(|| {
                                theme::section_title("Box fill");
                                theme::hint("Select a project asset to fill or chip boxes.");
                            });
                        }
                        theme::section(|| {
                            theme::section_title("View");
                            if theme::secondary("Reset View") {
                                self.preview_camera = PreviewCamera::default();
                            }
                        });
                    });
                });
            });
        });
    }

    /// Add / Chip tool buttons, plus a leading Select tool in the scene.
    fn voxel_tool_buttons(&mut self, with_select: bool) {
        let kind = |active: bool| {
            if active {
                ButtonKind::Primary
            } else {
                ButtonKind::Secondary
            }
        };
        let select = with_select && self.scene_select;
        List::row().item_spacing(6.0).show(|| {
            if with_select {
                yakui::expanded(|| {
                    if theme::button(kind(select), ButtonSize::Small, "Select") {
                        self.scene_select = true;
                    }
                });
            }
            yakui::expanded(|| {
                if theme::button(kind(!select && !self.chip_mode), ButtonSize::Small, "Add") {
                    self.scene_select = false;
                    self.chip_mode = false;
                }
            });
            yakui::expanded(|| {
                if theme::button(kind(!select && self.chip_mode), ButtonSize::Small, "Chip") {
                    self.scene_select = false;
                    self.chip_mode = true;
                }
            });
        });
    }

    fn box_section(&mut self, id: AssetId) {
        theme::section_title("Box fill");
        theme::triple("Box min", &mut self.box_min);
        theme::triple("Box max", &mut self.box_max);
        List::row().item_spacing(8.0).show(|| {
            yakui::expanded(|| {
                if theme::secondary("Add Box") {
                    self.box_operation(id, false);
                }
            });
            yakui::expanded(|| {
                if theme::secondary("Chip Box") {
                    self.box_operation(id, true);
                }
            });
        });
    }

    // ---- File-menu dialogs ------------------------------------------------

    fn modal_ui(&mut self) {
        let Some(modal) = self.modal else {
            return;
        };
        let viewport = self
            .window
            .as_ref()
            .map_or(Vec2::new(1280.0, 720.0), |window| {
                let size = window.inner_size();
                let scale = window.scale_factor() as f32;
                Vec2::new(size.width as f32 / scale, size.height as f32 / scale)
            });
        let close = match modal {
            Modal::NewAsset => {
                theme::modal(viewport, "New Asset", 400.0, || self.new_asset_dialog())
            }
            Modal::ImportExport => theme::modal(viewport, "Import / Export", 460.0, || {
                self.import_export_dialog()
            }),
        };
        if close {
            self.modal = None;
        }
    }

    fn new_asset_dialog(&mut self) {
        theme::hint("Create an empty voxel asset in this project.");
        theme::field_with_placeholder("Asset name", &mut self.asset_name, "e.g. Oak Crate");
        let has_name = !self.asset_name.trim().is_empty();
        if theme::button_enabled(
            ButtonKind::Primary,
            ButtonSize::Regular,
            "+  New Asset",
            has_name,
        ) {
            let name = self.asset_name.trim().to_owned();
            if let Some(command) = self
                .model
                .as_ref()
                .map(|model| model.new_voxel_asset_command(&name))
            {
                if let EditorCommand::CreateVoxelAsset { record, .. } = &command {
                    self.selected_asset = Some(record.id);
                    self.selected_builtin = None;
                    self.selected_voxel = None;
                }
                self.execute(command);
                self.asset_name.clear();
                self.workspace = Workspace::Assets;
                self.modal = None;
            }
        }
    }

    fn import_export_dialog(&mut self) {
        theme::hint("Read or write .spvox voxel files.");
        if theme::secondary("Import .spvox…") {
            let picked = self
                .file_dialog("Import .spvox")
                .add_filter("Spall voxel asset", &["spvox"])
                .pick_file();
            if let Some(path) = picked {
                let result = self
                    .model
                    .as_ref()
                    .map(|model| model.import_spvox_command(&path));
                match result {
                    Some(Ok(command)) => {
                        self.execute(command);
                        self.status = format!("Imported {}", path.display());
                        self.modal = None;
                    }
                    Some(Err(error)) => self.status = error.to_string(),
                    None => {}
                }
            }
        }
        let selected = self.selected_asset.and_then(|id| {
            let name = &self.model.as_ref()?.voxel_assets.get(&id)?.name;
            Some((id, name.clone()))
        });
        if let Some((id, name)) = selected {
            if theme::secondary("Export selected asset…") {
                let picked = self
                    .file_dialog("Export .spvox")
                    .add_filter("Spall voxel asset", &["spvox"])
                    .set_file_name(format!("{name}.spvox"))
                    .save_file();
                if let (Some(path), Some(model)) = (picked, self.model.as_ref()) {
                    match model.export_spvox_asset(id, &path) {
                        Ok(()) => {
                            self.status = format!("Exported {}", path.display());
                            self.modal = None;
                        }
                        Err(error) => self.status = error.to_string(),
                    }
                }
            }
        } else {
            theme::hint("Select a project asset to export it.");
        }
    }

    fn asset_viewport(&mut self, preview: &PreviewState) {
        let background = self.preview_environment.environment().background;
        yakui::stack(|| {
            if let Some(asset) = &preview.asset {
                draw_asset_3d_preview(
                    asset.clone(),
                    self.preview_camera,
                    self.preview_environment,
                    preview.cursor,
                    Some(self.preview_hover_cell.clone()),
                    self.selected_voxel,
                    self.show_voxel_boundaries,
                    preview.bounds,
                    self.show_asset_bounds,
                );
            } else {
                yakui::colored_box_container(
                    Color::rgb(background[0], background[1], background[2]),
                    || {
                        yakui::align(Alignment::CENTER, || {
                            theme::label(
                                13.0,
                                theme::TEXT_MUTED,
                                "Choose an asset from the library below.",
                            );
                        });
                    },
                );
            }

            if let Some(asset) = &preview.asset {
                let mut info = format!("{} · {} voxels", asset.name, asset.voxels.len());
                if let Some(cell) = preview.hover {
                    info.push_str(&format!("   Hover {}, {}, {}", cell.x, cell.y, cell.z));
                }
                if let Some(cell) = self.selected_voxel {
                    info.push_str(&format!("   Selected {}, {}, {}", cell.x, cell.y, cell.z));
                }
                yakui::align(Alignment::TOP_LEFT, || {
                    yakui::pad(
                        Pad {
                            left: 18.0,
                            top: 14.0,
                            ..Pad::ZERO
                        },
                        || {
                            theme::label(12.0, theme::TEXT_LABEL, info);
                        },
                    );
                });
            }

            yakui::align(Alignment::TOP_RIGHT, || {
                yakui::pad(
                    Pad {
                        right: 18.0,
                        top: 12.0,
                        ..Pad::ZERO
                    },
                    || {
                        theme::hstack(8.0, || {
                            self.environment_dropdown();
                            self.view_dropdown();
                        });
                    },
                );
            });

            yakui::align(Alignment::BOTTOM_LEFT, || {
                yakui::pad(
                    Pad {
                        left: 18.0,
                        bottom: 10.0,
                        ..Pad::ZERO
                    },
                    || {
                        List::column()
                        .main_axis_size(yakui::MainAxisSize::Min)
                        .show(|| {
                            theme::label(
                                11.0,
                                theme::TEXT_DIM,
                                "Click: inspect cell · Orbit: middle-drag · Pan: Shift + middle-drag · Zoom: wheel · Reset: Home",
                            );
                            theme::label(
                                11.0,
                                theme::TEXT_DIM,
                                "Move pointer: W/S = Z−/+ · A/D = X−/+ · Q/E = Y−/+",
                            );
                        });
                    },
                );
            });
        });
    }

    fn environment_dropdown(&mut self) {
        List::column()
            .main_axis_size(yakui::MainAxisSize::Min)
            .show(|| {
                let title = format!("Environment: {}", self.preview_environment.label());
                if theme::dropdown_button(&title) {
                    self.open_menu = None;
                    self.view_menu_open = false;
                    self.environment_menu_open = !self.environment_menu_open;
                }
                if self.environment_menu_open {
                    let entries: Vec<_> = PreviewEnvironment::ALL
                        .iter()
                        .map(|environment| {
                            let entry = MenuEntry::new(environment.label());
                            if *environment == self.preview_environment {
                                entry.shortcut("Current")
                            } else {
                                entry
                            }
                        })
                        .collect();
                    let refs: Vec<_> = entries.iter().collect();
                    if let Some(index) = theme::dropdown_from(DropdownSide::Right, 190.0, &refs) {
                        self.preview_environment = PreviewEnvironment::ALL[index];
                        // Saved with the scene so playing it lights the game
                        // the same way.
                        if let Some(model) = self.model.as_mut() {
                            model.scene.environment = self.preview_environment.key().to_owned();
                        }
                        self.environment_menu_open = false;
                    }
                }
            });
    }

    fn view_dropdown(&mut self) {
        List::column()
            .main_axis_size(yakui::MainAxisSize::Min)
            .show(|| {
                if theme::dropdown_button("View") {
                    self.open_menu = None;
                    self.environment_menu_open = false;
                    self.view_menu_open = !self.view_menu_open;
                }
                if self.view_menu_open {
                    let entries = [
                        MenuEntry::new("Voxel boundaries").toggle(self.show_voxel_boundaries),
                        MenuEntry::new("Asset bounds").toggle(self.show_asset_bounds),
                    ];
                    let refs: Vec<_> = entries.iter().collect();
                    match theme::dropdown_from(DropdownSide::Right, 220.0, &refs) {
                        Some(0) => self.show_voxel_boundaries = !self.show_voxel_boundaries,
                        Some(1) => self.show_asset_bounds = !self.show_asset_bounds,
                        _ => {}
                    }
                }
            });
    }

    fn asset_right_panel(&mut self) {
        let width = self.right_panel_width;
        yakui::constrained(Constraints::width(width, width), || {
            yakui::colored_box_container(theme::BG_PANEL, || {
                List::column()
                    .cross_axis_alignment(CrossAxisAlignment::Stretch)
                    .show(|| {
                        if let Some(tab) =
                            theme::tabs(&["Layer", "Material", "Bounds"], self.right_tab as usize)
                        {
                            self.right_tab = match tab {
                                0 => RightTab::Layer,
                                1 => RightTab::Material,
                                _ => RightTab::Bounds,
                            };
                        }
                        let Some(id) = self.selected_asset else {
                            yakui::expanded(|| {
                                theme::panel_body(10.0, || {
                                    theme::hint(
                                        "Engine assets are read-only. Choose Edit Project Copy in the library to edit one.",
                                    );
                                });
                            });
                            return;
                        };
                        yakui::expanded(|| {
                            yakui::scroll_vertical(|| {
                                theme::vstack(0.0, || match self.right_tab {
                                    RightTab::Layer => self.layer_tab(id),
                                    RightTab::Material => self.material_tab(),
                                    RightTab::Bounds => self.bounds_tab(),
                                });
                            });
                        });
                        theme::hline(theme::LINE);
                        theme::panel_body(8.0, || {
                            let label = if self.chip_mode {
                                "Chip Voxel"
                            } else {
                                "+  Add Voxel"
                            };
                            if theme::primary(label) {
                                self.add_or_chip_voxel(id);
                            }
                            List::row().item_spacing(8.0).show(|| {
                                yakui::expanded(|| {
                                    if theme::secondary("Set Bounds") {
                                        self.set_asset_bounds(id, false);
                                    }
                                });
                                yakui::expanded(|| {
                                    if theme::secondary("Clear") {
                                        self.set_asset_bounds(id, true);
                                    }
                                });
                            });
                        });
                    });
            });
        });
    }

    fn layer_tab(&mut self, id: AssetId) {
        theme::section(|| {
            theme::section_title("Authoring layers");
            if let Some(text) = theme::input(&self.layer_name, "New layer name") {
                self.layer_name = text;
            }
            if theme::secondary("+  Add Layer") {
                self.add_layer(id);
            }
            let layers = self
                .model
                .as_ref()
                .and_then(|model| model.voxel_assets.get(&id))
                .map(|doc| doc.layers.clone())
                .unwrap_or_default();
            if layers.is_empty() {
                theme::hint("No layers yet. Voxels are edited on the base asset.");
            }
            theme::vstack(1.0, || {
                for layer in &layers {
                    self.layer_row(id, layer);
                }
            });
        });
    }

    fn layer_row(&mut self, id: AssetId, layer: &VoxelLayer) {
        let active = self.selected_layer == Some(layer.id);
        let swatch = layer_swatch(layer);
        List::row()
            .cross_axis_alignment(CrossAxisAlignment::Center)
            .show(|| {
                yakui::expanded(|| {
                    yakui::stack(|| {
                        if theme::select_row(active) {
                            self.selected_layer = Some(layer.id);
                        }
                        yakui::align(Alignment::CENTER_LEFT, || {
                            yakui::pad(Pad::horizontal(8.0), || {
                                theme::hstack(8.0, || {
                                    RoundRect::new(2.0)
                                        .color(swatch)
                                        .min_size(Vec2::splat(11.0))
                                        .show();
                                    theme::label(
                                        12.0,
                                        if active {
                                            theme::TEXT
                                        } else {
                                            theme::TEXT_MENU
                                        },
                                        layer.name.clone(),
                                    );
                                    theme::label(
                                        11.0,
                                        theme::TEXT_DIM,
                                        format!("{} cells", layer.cells.len()),
                                    );
                                });
                            });
                        });
                    });
                });
                let (icon, ink) = if layer.visible {
                    (Icon::Eye, theme::TEXT_LABEL)
                } else {
                    (Icon::EyeHidden, theme::TEXT_DIM)
                };
                if theme::icon_toggle(icon, ink) {
                    self.toggle_layer(id, layer.id);
                }
            });
    }

    fn material_tab(&mut self) {
        theme::section(|| {
            theme::section_title("Material");
            theme::field("Material ID", &mut self.material);
            theme::triple("RGB", &mut self.color);
            theme::field("Jitter ± RGB", &mut self.jitter);
        });
        theme::section(|| {
            theme::section_title("Voxel");
            theme::triple("Cell X/Y/Z", &mut self.coord);
            theme::hint(if self.chip_mode {
                "Tool: Erase. Click a cell in the viewport, then chip it."
            } else {
                "Tool: Add. Click a cell in the viewport, then add it."
            });
        });
    }

    fn bounds_tab(&mut self) {
        if self.selected_asset.is_none() {
            return;
        }
        theme::section(|| {
            theme::section_title("Build bounds");
            theme::triple("Bounds min", &mut self.bounds_min);
            theme::triple("Bounds max", &mut self.bounds_max);
        });
    }

    // ---- asset editing actions -------------------------------------------

    fn add_layer(&mut self, id: AssetId) {
        let Some(doc) = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&id))
        else {
            return;
        };
        let before = doc.layers.clone();
        let mut after = before.clone();
        if after.is_empty() {
            let cells = doc
                .voxels
                .iter()
                .map(|(&cell, &material)| {
                    (
                        cell,
                        VoxelLayerCell {
                            material,
                            tint: doc.colors.get(&cell).copied(),
                        },
                    )
                })
                .collect();
            after.push(VoxelLayer {
                id: 1,
                name: "Base".into(),
                visible: true,
                cells,
            });
        }
        let Some(next) = after
            .iter()
            .map(|layer| layer.id)
            .max()
            .and_then(|id| id.checked_add(1))
        else {
            self.status = "Layer ID limit reached".into();
            return;
        };
        let name = if self.layer_name.trim().is_empty() {
            format!("Layer {next}")
        } else {
            self.layer_name.trim().to_owned()
        };
        after.push(VoxelLayer {
            id: next,
            name,
            visible: true,
            cells: Default::default(),
        });
        self.execute(EditorCommand::SetAssetLayers {
            asset: id,
            before,
            after,
        });
        self.selected_layer = Some(next);
    }

    fn toggle_layer(&mut self, id: AssetId, layer_id: u32) {
        let Some(before) = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&id))
            .map(|doc| doc.layers.clone())
        else {
            return;
        };
        let mut after = before.clone();
        if let Some(item) = after.iter_mut().find(|item| item.id == layer_id) {
            item.visible = !item.visible;
        }
        self.execute(EditorCommand::SetAssetLayers {
            asset: id,
            before,
            after,
        });
    }

    fn add_or_chip_voxel(&mut self, id: AssetId) {
        let Some([x, y, z]) = int3(&self.coord) else {
            self.status = "Cell X/Y/Z must be whole numbers".into();
            return;
        };
        let cell = VoxelCoord { x, y, z };
        let Some(doc) = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&id))
        else {
            return;
        };
        let before = doc.state_at(cell);
        let after = if self.chip_mode {
            None
        } else {
            let material = self.material.parse::<u16>().unwrap_or(1).max(1);
            let color = [0, 1, 2].map(|i| self.color[i].parse::<u8>().unwrap_or(128));
            Some(VoxelState::new(material, color))
        };
        if before != after {
            self.edit_voxels(
                id,
                vec![VoxelChange {
                    cell,
                    before,
                    after,
                }],
            );
        }
    }

    fn set_asset_bounds(&mut self, id: AssetId, clear: bool) {
        let after = if clear {
            None
        } else {
            let (Some(lo), Some(hi)) = (int3(&self.bounds_min), int3(&self.bounds_max)) else {
                self.status = "Bounds must be whole numbers".into();
                return;
            };
            Some(VoxelBounds::new(
                VoxelCoord {
                    x: lo[0],
                    y: lo[1],
                    z: lo[2],
                },
                VoxelCoord {
                    x: hi[0],
                    y: hi[1],
                    z: hi[2],
                },
            ))
        };
        let Some(before) = self
            .model
            .as_ref()
            .and_then(|model| model.project.asset_database.assets.get(&id))
            .map(|record| record.authoring_bounds)
        else {
            return;
        };
        self.execute(EditorCommand::SetAssetBounds {
            asset: id,
            before,
            after,
        });
    }

    fn box_operation(&mut self, id: AssetId, remove: bool) {
        let (Some(lo), Some(hi)) = (int3(&self.box_min), int3(&self.box_max)) else {
            self.status = "Box min/max must be whole numbers".into();
            return;
        };
        let bounds = VoxelBounds::new(
            VoxelCoord {
                x: lo[0],
                y: lo[1],
                z: lo[2],
            },
            VoxelCoord {
                x: hi[0],
                y: hi[1],
                z: hi[2],
            },
        );
        if !bounds.cell_count().is_some_and(|count| count <= 16384) {
            self.status = "Box operation is limited to 16,384 cells".into();
            return;
        }
        let Some(doc) = self
            .model
            .as_ref()
            .and_then(|model| model.voxel_assets.get(&id))
        else {
            return;
        };
        let material = self.material.parse::<u16>().unwrap_or(1).max(1);
        let base = [0, 1, 2].map(|i| self.color[i].parse::<u8>().unwrap_or(128));
        let jitter = self.jitter.parse::<u8>().unwrap_or(0).min(64);
        let changes: Vec<_> = bounds_cells(bounds)
            .filter_map(|cell| {
                let before = doc.state_at(cell);
                let after = if remove {
                    None
                } else {
                    Some(VoxelState::new(material, jittered(base, cell, jitter)))
                };
                (before != after).then_some(VoxelChange {
                    cell,
                    before,
                    after,
                })
            })
            .collect();
        self.edit_voxels(id, changes);
    }

    // ---- asset library ----------------------------------------------------

    fn library_columns(&self) -> usize {
        let width = self.window.as_ref().map_or(1280.0, |window| {
            window.inner_size().width as f32 / window.scale_factor() as f32
        });
        // Horizontal padding plus room for the scrollbar.
        let usable = width - 40.0 - 14.0;
        (((usable + CARD_GAP) / (CARD_WIDTH + CARD_GAP)).floor() as usize).max(1)
    }

    /// Bottom library panel. In the asset workspace it lists built-in and
    /// project assets for viewing; in the scene workspace it is the toolbox
    /// for placing project assets.
    fn library_panel(&mut self, toolbox: bool) {
        if !self.bottom_panel_open {
            return;
        }
        theme::resize_handle(false, self.handle_hot(PanelDrag::Bottom));
        let height = self.bottom_panel_height;
        let (title, subtitle) = if toolbox {
            ("Asset Toolbox", "Place assets into the scene")
        } else {
            ("Voxel Asset Library", "Built-ins and project assets")
        };
        let filter = self.search.to_lowercase();
        let columns = self.library_columns();
        let mut items = Vec::new();
        if !toolbox || self.show_engine_assets {
            for (builtin, _, mesh) in &self.builtin_cards {
                if filter.is_empty()
                    || builtin.name.to_lowercase().contains(&filter)
                    || builtin.file_name.to_lowercase().contains(&filter)
                {
                    items.push(LibraryItem::Builtin(*builtin, mesh.clone()));
                }
            }
        }
        if let Some(model) = self.model.as_ref() {
            let scene_asset = model.scene_voxels_asset().map(|(_, asset)| asset);
            for record in model.project.asset_database.assets.values() {
                if toolbox && Some(record.id) == scene_asset {
                    continue;
                }
                if filter.is_empty() || record.name.to_lowercase().contains(&filter) {
                    let mesh = self
                        .project_thumbnails
                        .iter()
                        .find(|(id, _)| *id == record.id)
                        .map(|(_, mesh)| mesh.clone());
                    items.push(LibraryItem::Project(record.id, record.name.clone(), mesh));
                }
            }
        }

        yakui::constrained(Constraints::height(height, height), || {
            yakui::colored_box_container(theme::BG_LIBRARY, || {
                yakui::pad(Pad::balanced(20.0, 14.0), || {
                    List::column().show(|| {
                        List::row()
                            .cross_axis_alignment(CrossAxisAlignment::Center)
                            .show(|| {
                                theme::vstack(2.0, || {
                                    theme::label(12.5, theme::TEXT, title);
                                    theme::label(11.0, theme::TEXT_DIM, subtitle);
                                });
                                yakui::expanded(|| {});
                                if toolbox
                                    && let Some(on) = theme::checkbox(
                                        "Show engine assets",
                                        self.show_engine_assets,
                                    )
                                {
                                    self.show_engine_assets = on;
                                }
                                yakui::pad(Pad::horizontal(8.0), || {});
                                yakui::constrained(Constraints::width(220.0, 220.0), || {
                                    if let Some(text) = theme::input(&self.search, "Search assets")
                                    {
                                        self.search = text;
                                    }
                                });
                            });
                        yakui::pad(Pad::balanced(0.0, 6.0), || {});
                        yakui::expanded(|| {
                            yakui::scroll_vertical(|| {
                                theme::vstack(CARD_GAP, || {
                                    for row in items.chunks(columns) {
                                        List::row().item_spacing(CARD_GAP).show(|| {
                                            for item in row {
                                                self.library_card(item, toolbox);
                                            }
                                        });
                                    }
                                });
                            });
                        });
                    });
                });
            });
        });
    }

    fn library_card(&mut self, item: &LibraryItem, toolbox: bool) {
        match item {
            LibraryItem::Builtin(builtin, mesh) if toolbox => {
                asset_card(Some(mesh), builtin.name, false, || {
                    if theme::button(ButtonKind::Secondary, ButtonSize::Tiny, "Place") {
                        self.place_builtin(*builtin);
                    }
                });
            }
            LibraryItem::Builtin(builtin, mesh) => {
                let selected = self.selected_builtin == Some(builtin.key);
                asset_card(Some(mesh), builtin.name, selected, || {
                    let kind = if selected {
                        ButtonKind::Primary
                    } else {
                        ButtonKind::Secondary
                    };
                    if theme::button(
                        kind,
                        ButtonSize::Tiny,
                        if selected { "Selected" } else { "View" },
                    ) {
                        self.view_builtin(*builtin);
                    }
                    if selected
                        && theme::button(ButtonKind::Secondary, ButtonSize::Tiny, "Edit Copy")
                    {
                        self.edit_builtin_copy(*builtin);
                    }
                });
            }
            LibraryItem::Project(id, name, mesh) => {
                let selected = !toolbox && self.selected_asset == Some(*id);
                asset_card(mesh.as_ref(), name, selected, || {
                    if toolbox {
                        if theme::button(ButtonKind::Secondary, ButtonSize::Tiny, "Place") {
                            self.place_asset(*id, name);
                        }
                    } else {
                        let kind = if selected {
                            ButtonKind::Primary
                        } else {
                            ButtonKind::Secondary
                        };
                        if theme::button(
                            kind,
                            ButtonSize::Tiny,
                            if selected { "Selected" } else { "View" },
                        ) {
                            self.selected_asset = Some(*id);
                            self.selected_builtin = None;
                            self.selected_voxel = None;
                        }
                    }
                });
            }
        }
    }

    fn view_builtin(&mut self, builtin: BuiltinVoxelAsset) {
        self.selected_builtin = Some(builtin.key);
        self.selected_asset = None;
        self.selected_voxel = None;
        self.builtin_preview = self
            .model
            .as_ref()
            .and_then(|model| model.preview_builtin_spvox(builtin.bytes).ok());
    }

    fn edit_builtin_copy(&mut self, builtin: BuiltinVoxelAsset) {
        let command = self.model.as_ref().and_then(|model| {
            model
                .import_builtin_spvox_command(builtin.bytes, builtin.name)
                .ok()
        });
        if let Some(command) = command {
            if let EditorCommand::CreateVoxelAsset { record, .. } = &command {
                self.selected_asset = Some(record.id);
            }
            self.selected_builtin = None;
            self.selected_voxel = None;
            self.execute(command);
        }
    }

    /// Place an engine asset: the first time it is used a project copy is
    /// imported, so the scene never references read-only engine data.
    fn place_builtin(&mut self, builtin: BuiltinVoxelAsset) {
        let known = self
            .imported_builtins
            .get(builtin.key)
            .copied()
            .filter(|id| {
                self.model
                    .as_ref()
                    .is_some_and(|model| model.voxel_assets.contains_key(id))
            });
        let id = match known {
            Some(id) => id,
            None => {
                let command = self.model.as_ref().and_then(|model| {
                    model
                        .import_builtin_spvox_command(builtin.bytes, builtin.name)
                        .ok()
                });
                let Some(command) = command else {
                    self.status = format!("Could not import engine asset {}", builtin.name);
                    return;
                };
                let EditorCommand::CreateVoxelAsset { record, .. } = &command else {
                    return;
                };
                let id = record.id;
                self.execute(command);
                self.imported_builtins.insert(builtin.key, id);
                id
            }
        };
        self.place_asset(id, builtin.name);
    }

    fn place_asset(&mut self, id: AssetId, name: &str) {
        let Some(model) = self.model.as_mut() else {
            return;
        };
        let mut entity = model.scene.new_entity(name);
        entity.voxel_asset = Some(id);
        self.selected_entity = Some(entity.id);
        self.execute(EditorCommand::CreateEntity { entity });
    }

    // ---- scene workspace --------------------------------------------------

    fn scene_ui(&mut self) {
        let entities: Vec<_> = self
            .model
            .as_ref()
            .map(|model| model.scene.entities.values().cloned().collect())
            .unwrap_or_default();
        if self.scene_view.is_none() {
            self.scene_view = self.model.as_ref().map(|model| model.scene_composite());
            self.scene_revision += 1;
        }
        let (cursor, hover) = self.viewport_pointer();
        yakui::expanded(|| {
            List::row()
                .cross_axis_alignment(CrossAxisAlignment::Stretch)
                .show(|| {
                    if self.left_panel_open {
                        self.hierarchy_panel(&entities);
                        theme::resize_handle(true, self.handle_hot(PanelDrag::Left));
                    }
                    yakui::expanded(|| self.scene_viewport(&entities, cursor, hover));
                    if self.right_panel_open {
                        theme::resize_handle(true, self.handle_hot(PanelDrag::Right));
                        self.inspector_panel(&entities);
                    }
                });
        });
        self.library_panel(true);
    }

    fn hierarchy_panel(&mut self, entities: &[spall_editor::SceneEntity]) {
        let width = self.left_panel_width;
        yakui::constrained(Constraints::width(width, width), || {
            yakui::colored_box_container(theme::BG_PANEL, || {
                yakui::scroll_vertical(|| {
                    theme::vstack(0.0, || {
                        theme::section(|| {
                            theme::section_title("Tool");
                            self.voxel_tool_buttons(true);
                            theme::hint(if self.scene_select {
                                "Select: click an object in the viewport."
                            } else if self.chip_mode {
                                "Chip: click a cell, then Chip Voxel (Voxels tab)."
                            } else {
                                "Add: click a cell, then Add Voxel (Voxels tab)."
                            });
                            if theme::secondary("Reset View") {
                                self.preview_camera = PreviewCamera::default();
                            }
                        });
                        theme::panel_body(10.0, || {
                            theme::section_title("Hierarchy");
                            if entities.is_empty() {
                                theme::hint("Empty scene.");
                            }
                            theme::vstack(1.0, || {
                                for entity in entities {
                                    let active = self.selected_entity == Some(entity.id);
                                    let mut clicked = false;
                                    yakui::stack(|| {
                                        clicked = theme::select_row(active);
                                        yakui::align(Alignment::CENTER_LEFT, || {
                                            yakui::pad(Pad::horizontal(8.0), || {
                                                theme::hstack(8.0, || {
                                                    let ink = if entity.voxel_asset.is_some() {
                                                        theme::ACCENT
                                                    } else {
                                                        theme::TEXT_DIM
                                                    };
                                                    RoundRect::new(2.0)
                                                        .color(ink)
                                                        .min_size(Vec2::splat(10.0))
                                                        .show();
                                                    theme::label(
                                                        12.0,
                                                        if active {
                                                            theme::TEXT
                                                        } else {
                                                            theme::TEXT_MENU
                                                        },
                                                        entity.name.clone(),
                                                    );
                                                });
                                            });
                                        });
                                    });
                                    if clicked {
                                        self.select_entity(entity.id);
                                    }
                                }
                            });
                            if theme::button(
                                ButtonKind::Ghost,
                                ButtonSize::Regular,
                                "+  Empty Object",
                            ) && let Some(model) = self.model.as_mut()
                            {
                                let entity = model.scene.new_entity("Object");
                                self.selected_entity = Some(entity.id);
                                self.execute(EditorCommand::CreateEntity { entity });
                            }
                        });
                    });
                });
            });
        });
    }

    fn scene_viewport(
        &mut self,
        entities: &[spall_editor::SceneEntity],
        cursor: Option<Vec2>,
        hover: Option<VoxelCoord>,
    ) {
        let selected = self
            .selected_entity
            .and_then(|id| entities.iter().find(|entity| entity.id == id));
        let background = self.preview_environment.environment().background;
        // The engine renderer draws the scene into a texture; the selected
        // object's cells are tinted in the mesh's material table, and the
        // mesh is rebuilt only when the scene or selection changes.
        let mesh = self
            .current_scene_mesh()
            .filter(|mesh| mesh.bounds.is_some());
        let voxel_count = self
            .scene_view
            .as_ref()
            .map_or(0, |(asset, _)| asset.voxels.len());
        yakui::stack(|| {
            if let Some(mesh) = mesh {
                let Some(gpu) = self.gpu.as_ref() else {
                    return;
                };
                crate::scene_viewport::draw_scene_canvas(crate::scene_viewport::SceneCanvas {
                    mesh,
                    texture: gpu.viewport_texture,
                    camera: self.preview_camera,
                    cursor,
                    hover_cell: self.preview_hover_cell.clone(),
                    rect_out: self.scene_viewport_rect.clone(),
                    selected_voxel: self.selected_voxel,
                    background,
                });
            } else {
                yakui::colored_box_container(
                    Color::rgb(background[0], background[1], background[2]),
                    || {
                        yakui::align(Alignment::CENTER, || {
                            theme::label_centered(
                                12.0,
                                theme::TEXT_MUTED,
                                "Empty scene. Place an asset from the toolbox below, or paint voxels from the Voxels tab.",
                            );
                        });
                    },
                );
            }
            yakui::align(Alignment::TOP_LEFT, || {
                yakui::pad(
                    Pad {
                        left: 18.0,
                        top: 14.0,
                        ..Pad::ZERO
                    },
                    || {
                        let mut info = format!(
                            "{} · {} objects · {} voxels",
                            self.model
                                .as_ref()
                                .map_or("", |model| model.scene.name.as_str()),
                            entities.len(),
                            voxel_count
                        );
                        if let Some(cell) = hover {
                            info.push_str(&format!("   Hover {}, {}, {}", cell.x, cell.y, cell.z));
                        }
                        if let Some(entity) = selected {
                            info.push_str(&format!("   Selected {}", entity.name));
                        }
                        theme::label(12.0, theme::TEXT_LABEL, info);
                    },
                );
            });
            yakui::align(Alignment::TOP_RIGHT, || {
                yakui::pad(
                    Pad {
                        right: 18.0,
                        top: 12.0,
                        ..Pad::ZERO
                    },
                    || {
                        theme::hstack(8.0, || {
                            self.environment_dropdown();
                            self.view_dropdown();
                        });
                    },
                );
            });
            yakui::align(Alignment::BOTTOM_LEFT, || {
                yakui::pad(
                    Pad {
                        left: 18.0,
                        bottom: 10.0,
                        ..Pad::ZERO
                    },
                    || {
                        theme::label(
                            11.0,
                            theme::TEXT_DIM,
                            "Click: select · Orbit: middle-drag · Pan: Shift + middle-drag · Zoom: wheel · Reset: Home",
                        );
                    },
                );
            });
        });
    }

    fn inspector_panel(&mut self, entities: &[spall_editor::SceneEntity]) {
        let width = self.right_panel_width;
        yakui::constrained(Constraints::width(width, width), || {
            yakui::colored_box_container(theme::BG_PANEL, || {
                List::column()
                    .cross_axis_alignment(CrossAxisAlignment::Stretch)
                    .show(|| {
                        if let Some(tab) =
                            theme::tabs(&["Object", "Voxels"], self.scene_tab as usize)
                        {
                            self.scene_tab = if tab == 0 {
                                SceneTab::Object
                            } else {
                                SceneTab::Voxels
                            };
                        }
                        match self.scene_tab {
                            SceneTab::Object => {
                                yakui::expanded(|| {
                                    yakui::scroll_vertical(|| self.object_tab(entities));
                                });
                            }
                            SceneTab::Voxels => self.scene_voxels_tab(),
                        }
                    });
            });
        });
    }

    fn object_tab(&mut self, entities: &[spall_editor::SceneEntity]) {
        theme::panel_body(12.0, || {
            let Some(entity) = self
                .selected_entity
                .and_then(|id| entities.iter().find(|entity| entity.id == id))
            else {
                theme::hint("Select an object to edit its transform.");
                return;
            };
            theme::label(13.0, theme::TEXT, entity.name.clone());
            theme::triple("Position (m)", &mut self.translation);
            theme::triple("Rotation (degrees)", &mut self.rotation);
            theme::triple("Scale", &mut self.scale);
            if theme::primary("Apply Transform") {
                self.apply_transform(entity);
            }
            if entity.voxel_asset.is_some() {
                if theme::secondary("Unassign Asset") {
                    self.execute(EditorCommand::SetEntityAsset {
                        entity: entity.id,
                        before: entity.voxel_asset,
                        after: None,
                    });
                }
                if theme::secondary("Edit Voxels in Assets")
                    && let Some(asset) = entity.voxel_asset
                {
                    self.selected_asset = Some(asset);
                    self.selected_builtin = None;
                    self.selected_voxel = None;
                    self.workspace = Workspace::Assets;
                }
            }
            if theme::button(ButtonKind::Ghost, ButtonSize::Regular, "Delete Object") {
                self.selected_entity = None;
                self.execute(EditorCommand::DeleteEntity {
                    entity: entity.clone(),
                });
            }
        });
    }

    /// Freehand voxel tools for the scene's own voxel layer.
    fn scene_voxels_tab(&mut self) {
        let Some((_, asset)) = self
            .model
            .as_ref()
            .and_then(|model| model.scene_voxels_asset())
        else {
            yakui::expanded(|| {
                theme::panel_body(10.0, || {
                    theme::hint(
                        "Scene voxels are terrain and structures built directly in the scene, alongside placed assets.",
                    );
                    if theme::primary("+  Create Scene Voxels") {
                        self.create_scene_voxels();
                    }
                });
            });
            return;
        };
        yakui::expanded(|| {
            yakui::scroll_vertical(|| {
                theme::vstack(0.0, || {
                    self.material_tab();
                    theme::section(|| self.box_section(asset));
                });
            });
        });
        theme::hline(theme::LINE);
        theme::panel_body(8.0, || {
            let label = if self.chip_mode {
                "Chip Voxel"
            } else {
                "+  Add Voxel"
            };
            if theme::primary(label) {
                self.add_or_chip_voxel(asset);
            }
        });
    }

    fn create_scene_voxels(&mut self) {
        let Some(command) = self
            .model
            .as_ref()
            .map(|model| model.new_voxel_asset_command(spall_editor::SCENE_VOXELS_NAME))
        else {
            return;
        };
        let EditorCommand::CreateVoxelAsset { record, .. } = &command else {
            return;
        };
        let asset = record.id;
        self.execute(command);
        let Some(model) = self.model.as_mut() else {
            return;
        };
        let mut entity = model.scene.new_entity(spall_editor::SCENE_VOXELS_NAME);
        entity.voxel_asset = Some(asset);
        self.selected_entity = Some(entity.id);
        self.execute(EditorCommand::CreateEntity { entity });
        self.scene_select = false;
    }

    fn apply_transform(&mut self, entity: &spall_editor::SceneEntity) {
        let parse = |v: &[String; 3]| -> Option<[f32; 3]> {
            Some([v[0].parse().ok()?, v[1].parse().ok()?, v[2].parse().ok()?])
        };
        let (Some(translation), Some(rotation), Some(scale)) = (
            parse(&self.translation),
            parse(&self.rotation),
            parse(&self.scale),
        ) else {
            self.status = "Transform values must be numbers".into();
            return;
        };
        self.execute(EditorCommand::SetTransform {
            entity: entity.id,
            before: entity.transform,
            after: spall_editor::Transform {
                translation,
                rotation_degrees: rotation,
                scale,
            },
        });
    }
}

/// Asset card: 60px thumbnail, name, and action buttons. The selected card
/// gets the accent border.
fn asset_card(mesh: Option<&Arc<ThumbMesh>>, name: &str, selected: bool, actions: impl FnOnce()) {
    let (edge, fill) = if selected {
        (theme::ACCENT, theme::BG_RAISED)
    } else {
        (theme::LINE_CARD, theme::BG_PANEL)
    };
    yakui::constrained(Constraints::width(CARD_WIDTH, CARD_WIDTH), || {
        RoundRect::new(6.0).color(edge).show_children(|| {
            yakui::pad(Pad::all(1.0), || {
                RoundRect::new(5.0).color(fill).show_children(|| {
                    yakui::pad(Pad::all(10.0), || {
                        theme::vstack(8.0, || {
                            yakui::align(Alignment::TOP_CENTER, || thumbnail(mesh, CARD_THUMB));
                            yakui::align(Alignment::TOP_CENTER, || {
                                theme::label_centered(11.5, theme::TEXT, name.to_owned());
                            });
                            theme::vstack(4.0, actions);
                        });
                    });
                });
            });
        });
    });
}

/// Representative color for a layer: the average tint of its first cells.
fn layer_swatch(layer: &VoxelLayer) -> Color {
    let mut total = [0_u32; 3];
    let mut count = 0_u32;
    for tint in layer.cells.values().filter_map(|cell| cell.tint).take(64) {
        for channel in 0..3 {
            total[channel] += u32::from(tint[channel]);
        }
        count += 1;
    }
    if count == 0 {
        return theme::TEXT_DIM;
    }
    Color::rgb(
        (total[0] / count) as u8,
        (total[1] / count) as u8,
        (total[2] / count) as u8,
    )
}
