//! Editor-owned documents and commands. This crate is deliberately a leaf of
//! the workspace: Spall's runtime and renderer never import Yakui or editor
//! persistence types.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod scene_mesh;
mod spvox;

pub const PROJECT_FORMAT_VERSION: u32 = 1;
pub const SCENE_FORMAT_VERSION: u32 = 1;
pub const VOXEL_FORMAT_VERSION: u32 = 1;

/// A stable project-local identity. Scene files refer to assets only by this
/// value; moving an asset's on-disk file does not rewrite a scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub u64);

/// Name of the scene entity that carries the scene's freehand voxels.
pub const SCENE_VOXELS_NAME: &str = "Scene Voxels";

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "asset-{:016x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EditorEntityId(pub u64);

impl std::fmt::Display for EditorEntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "entity-{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VoxelCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// Inclusive local-cell bounds used by the editor while authoring an asset.
/// They belong to the project AssetDatabase, not the portable SPVX topology:
/// the same tree can be authored with a convenient work volume in one project
/// and placed unchanged in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoxelBounds {
    pub min: VoxelCoord,
    pub max: VoxelCoord,
}

impl VoxelBounds {
    pub fn new(a: VoxelCoord, b: VoxelCoord) -> Self {
        Self {
            min: VoxelCoord {
                x: a.x.min(b.x),
                y: a.y.min(b.y),
                z: a.z.min(b.z),
            },
            max: VoxelCoord {
                x: a.x.max(b.x),
                y: a.y.max(b.y),
                z: a.z.max(b.z),
            },
        }
    }

    pub fn contains(&self, cell: VoxelCoord) -> bool {
        (self.min.x..=self.max.x).contains(&cell.x)
            && (self.min.y..=self.max.y).contains(&cell.y)
            && (self.min.z..=self.max.z).contains(&cell.z)
    }

    pub fn cell_count(&self) -> Option<u64> {
        let width = u64::try_from(i64::from(self.max.x) - i64::from(self.min.x) + 1).ok()?;
        let height = u64::try_from(i64::from(self.max.y) - i64::from(self.min.y) + 1).ok()?;
        let depth = u64::try_from(i64::from(self.max.z) - i64::from(self.min.z) + 1).ok()?;
        width.checked_mul(height)?.checked_mul(depth)
    }
}

/// The authored state of one occupied voxel. The material keeps the future
/// engine-facing assignment stable; colour is an asset-side tint selected by
/// the voxel artist, so a tree can vary foliage without needing a material
/// editor in this MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoxelState {
    pub material: u16,
    pub color: [u8; 3],
}

impl VoxelState {
    pub const fn new(material: u16, color: [u8; 3]) -> Self {
        Self { material, color }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoxelChange {
    pub cell: VoxelCoord,
    pub before: Option<VoxelState>,
    pub after: Option<VoxelState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoxelLayerChange {
    pub cell: VoxelCoord,
    pub before: Option<VoxelLayerCell>,
    pub after: Option<VoxelLayerCell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub translation: [f32; 3],
    pub rotation_degrees: [f32; 3],
    pub scale: [f32; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translation: [0.0, 0.0, 0.0],
            rotation_degrees: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetKind {
    Voxel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRecord {
    pub id: AssetId,
    pub name: String,
    pub kind: AssetKind,
    /// Project-relative storage location, owned by the database rather than
    /// copied into scenes or entities.
    pub storage: PathBuf,
    /// Editor-only work volume. It controls fill/chip/add tools but does not
    /// alter the portable SPVX asset's sparse topology.
    #[serde(default)]
    pub authoring_bounds: Option<VoxelBounds>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDatabase {
    pub format_version: u32,
    pub next_asset_id: u64,
    pub assets: BTreeMap<AssetId, AssetRecord>,
}

impl Default for AssetDatabase {
    fn default() -> Self {
        Self {
            format_version: PROJECT_FORMAT_VERSION,
            next_asset_id: 1,
            assets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub format_version: u32,
    pub name: String,
    /// Explicit project-local mapping used when a portable SPVX material key
    /// is imported. Files never serialize these numeric IDs themselves.
    #[serde(default = "default_material_mapping")]
    pub material_mapping: BTreeMap<String, u16>,
    pub asset_database: AssetDatabase,
    pub scenes: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneEntity {
    pub id: EditorEntityId,
    pub name: String,
    pub transform: Transform,
    pub voxel_asset: Option<AssetId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneFile {
    pub format_version: u32,
    pub name: String,
    pub next_entity_id: u64,
    /// Lighting environment key (`spall_render::EnvironmentPreset::key`), used
    /// by both the editor viewport and the game when this scene is played.
    /// Older scene files without it load as `"studio"`.
    #[serde(default = "default_environment")]
    pub environment: String,
    pub entities: BTreeMap<EditorEntityId, SceneEntity>,
}

fn default_environment() -> String {
    spall_render::EnvironmentPreset::default().key().to_owned()
}

impl SceneFile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            format_version: SCENE_FORMAT_VERSION,
            name: name.into(),
            next_entity_id: 1,
            environment: default_environment(),
            entities: BTreeMap::new(),
        }
    }

    pub fn new_entity(&mut self, name: impl Into<String>) -> SceneEntity {
        let id = EditorEntityId(self.next_entity_id);
        self.next_entity_id = self
            .next_entity_id
            .checked_add(1)
            .expect("entity id exhausted");
        SceneEntity {
            id,
            name: name.into(),
            transform: Transform::default(),
            voxel_asset: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoxelAssetFile {
    /// Kept in memory for editor migrations; SPVX has its own major/minor
    /// container version and is the persisted asset format.
    pub format_version: u32,
    pub id: AssetId,
    /// Portable, project-independent asset identity from SPVX `META`.
    pub portable_id: [u8; 16],
    pub name: String,
    /// One of Spall's stable cell-size codes: 0 = 0.25 m, 1 = 0.0625 m.
    pub cell_size_code: u8,
    pub pivot_subcells: [i32; 3],
    /// Portable material keys retained for deterministic SPVX re-export.
    /// Numeric IDs are editor/runtime-local and are never written to SPVX.
    #[serde(default)]
    pub material_keys: BTreeMap<u16, String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Sparse, authored local cells. Material zero is represented by absence.
    pub voxels: BTreeMap<VoxelCoord, u16>,
    /// Asset-authored display tint per occupied cell. This is optional on disk
    /// for compatibility with the first MVP RON files; missing entries use the
    /// material's simple fallback swatch.
    #[serde(default)]
    pub colors: BTreeMap<VoxelCoord, [u8; 3]>,
    /// Optional editable source. Visible layers are composited in order;
    /// `voxels` and `colors` are the verified flattened runtime result.
    #[serde(default)]
    pub layers: Vec<VoxelLayer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoxelLayerCell {
    pub material: u16,
    /// None uses the mapped material's ordinary appearance.
    pub tint: Option<[u8; 3]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoxelLayer {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub cells: BTreeMap<VoxelCoord, VoxelLayerCell>,
}

impl VoxelAssetFile {
    pub fn new(id: AssetId, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            format_version: VOXEL_FORMAT_VERSION,
            id,
            portable_id: portable_id_for(id, &name),
            name,
            cell_size_code: 1,
            pivot_subcells: [0; 3],
            material_keys: default_material_mapping()
                .into_iter()
                .map(|(key, id)| (id, key))
                .collect(),
            tags: BTreeMap::new(),
            voxels: BTreeMap::new(),
            colors: BTreeMap::new(),
            layers: Vec::new(),
        }
    }

    pub fn flattened_layers(&self) -> (BTreeMap<VoxelCoord, u16>, BTreeMap<VoxelCoord, [u8; 3]>) {
        let mut voxels = BTreeMap::new();
        let mut colors = BTreeMap::new();
        for layer in self.layers.iter().filter(|layer| layer.visible) {
            for (&cell, state) in &layer.cells {
                voxels.insert(cell, state.material);
                if let Some(tint) = state.tint {
                    colors.insert(cell, tint);
                } else {
                    colors.remove(&cell);
                }
            }
        }
        (voxels, colors)
    }

    pub fn state_at(&self, cell: VoxelCoord) -> Option<VoxelState> {
        let material = self.voxels.get(&cell).copied()?;
        Some(VoxelState::new(
            material,
            self.colors
                .get(&cell)
                .copied()
                .unwrap_or_else(|| default_voxel_color(material)),
        ))
    }

    fn set_state(&mut self, cell: VoxelCoord, state: Option<VoxelState>) {
        match state.filter(|state| state.material != 0) {
            Some(state) => {
                self.voxels.insert(cell, state.material);
                self.colors.insert(cell, state.color);
            }
            None => {
                self.voxels.remove(&cell);
                self.colors.remove(&cell);
            }
        }
    }
}

/// The MVP's deliberately small, explicit project mapping. Import rejects a
/// portable key outside this map instead of guessing from a palette colour.
pub fn default_material_mapping() -> BTreeMap<String, u16> {
    BTreeMap::from([
        ("stone.granite".to_owned(), 1),
        ("wood.oak".to_owned(), 2),
        ("foliage.oak".to_owned(), 3),
        ("sandstone".to_owned(), 4),
    ])
}

fn portable_id_for(id: AssetId, name: &str) -> [u8; 16] {
    // A portable ID is generated once and stored in the asset. The entropy is
    // local to creation; it is never recomputed during export or import.
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"spall.editor.portable-asset-id.v1");
    hasher.update(&id.0.to_le_bytes());
    hasher.update(name.as_bytes());
    hasher.update(&std::process::id().to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    hasher.update(&now.to_le_bytes());
    let mut result = [0_u8; 16];
    result.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    if result == [0; 16] {
        result[0] = 1;
    }
    result
}

pub const fn default_voxel_color(material: u16) -> [u8; 3] {
    match material {
        1 => [108, 112, 120], // stone
        2 => [104, 72, 45],   // wood/dirt
        3 => [62, 143, 57],   // foliage/grass
        4 => [190, 166, 112],
        _ => [170, 170, 170],
    }
}

#[derive(Debug, Clone)]
pub struct EditorModel {
    pub root: PathBuf,
    pub project: ProjectFile,
    pub scene: SceneFile,
    pub voxel_assets: BTreeMap<AssetId, VoxelAssetFile>,
}

impl EditorModel {
    pub fn new(root: impl Into<PathBuf>, project_name: impl Into<String>) -> Self {
        let project_name = project_name.into();
        let mut scenes = BTreeMap::new();
        scenes.insert("Main".to_owned(), PathBuf::from("scenes/main.ron"));
        Self {
            root: root.into(),
            project: ProjectFile {
                format_version: PROJECT_FORMAT_VERSION,
                name: project_name,
                material_mapping: default_material_mapping(),
                asset_database: AssetDatabase::default(),
                scenes,
            },
            scene: SceneFile::new("Main"),
            voxel_assets: BTreeMap::new(),
        }
    }

    pub fn new_voxel_asset_command(&self, name: impl Into<String>) -> EditorCommand {
        let id = AssetId(self.project.asset_database.next_asset_id);
        let name = name.into();
        EditorCommand::CreateVoxelAsset {
            record: AssetRecord {
                id,
                name: name.clone(),
                kind: AssetKind::Voxel,
                storage: PathBuf::from(format!("assets/{id}.spvox")),
                authoring_bounds: None,
            },
            asset: VoxelAssetFile::new(id, name),
        }
    }

    /// The scene's freehand voxel asset: the asset referenced by the entity
    /// named [`SCENE_VOXELS_NAME`], if the scene has one.
    pub fn scene_voxels_asset(&self) -> Option<(EditorEntityId, AssetId)> {
        self.scene.entities.values().find_map(|entity| {
            (entity.name == SCENE_VOXELS_NAME)
                .then_some(entity.voxel_asset)
                .flatten()
                .filter(|asset| self.voxel_assets.contains_key(asset))
                .map(|asset| (entity.id, asset))
        })
    }

    /// Flatten every placed entity into one voxel grid for the scene viewport.
    ///
    /// The grid uses the finest cell size (0.0625 m). Each entity's asset is
    /// scaled, rotated (X, then Y, then Z, in degrees) and translated (metres)
    /// about the asset origin. The map records which entity owns each composed
    /// cell so a viewport click can select it. Later entities win where cells
    /// overlap.
    pub fn scene_composite(&self) -> (VoxelAssetFile, BTreeMap<VoxelCoord, EditorEntityId>) {
        let mut composite = VoxelAssetFile::new(AssetId(0), self.scene.name.clone());
        let mut owners = BTreeMap::new();
        let placed: Vec<_> = self
            .scene
            .entities
            .values()
            .filter_map(|entity| {
                let asset = entity
                    .voxel_asset
                    .and_then(|id| self.voxel_assets.get(&id))?;
                Some((entity, asset))
            })
            .collect();
        // A scene made only of 0.25 m assets is composed on a 0.25 m grid: a
        // 0.0625 m grid would turn every cell into 64 and make an ordinary
        // outdoor scene millions of cells, far more than the viewport can draw.
        let coarse =
            !placed.is_empty() && placed.iter().all(|(_, asset)| asset.cell_size_code == 0);
        let scene_cell_m = if coarse { 0.25 } else { SCENE_CELL_METRES };
        composite.cell_size_code = if coarse { 0 } else { 1 };
        for (entity, asset) in placed {
            place_asset_at(asset, &entity.transform, scene_cell_m, |cell, state| {
                composite.voxels.insert(cell, state.material);
                composite.colors.insert(cell, state.color);
                owners.insert(cell, entity.id);
            });
        }
        (composite, owners)
    }

    pub fn save_all(&self) -> Result<(), EditorError> {
        write_ron(&self.root.join("project.ron"), &self.project)?;
        let scene_path = self.project.scenes.get(&self.scene.name).ok_or_else(|| {
            EditorError::Invalid("active scene has no project database entry".into())
        })?;
        write_ron(&self.root.join(scene_path), &self.scene)?;
        for (id, asset) in &self.voxel_assets {
            let record = self.project.asset_database.assets.get(id).ok_or_else(|| {
                EditorError::Invalid(format!("voxel {id} is absent from AssetDatabase"))
            })?;
            write_spvox(&self.root.join(&record.storage), asset)?;
        }
        Ok(())
    }

    pub fn load(root: impl Into<PathBuf>) -> Result<Self, EditorError> {
        Self::load_scene(root.into(), None)
    }

    /// Opens whatever a user pointed at: a project folder, a project's
    /// `project.ron`, or one of its scene files (which becomes the active
    /// scene). The project root is the nearest folder holding `project.ron`.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, EditorError> {
        let path = path.as_ref();
        if path.is_dir() {
            return Self::load(path);
        }
        let mut first_error = None;
        for root in path
            .ancestors()
            .skip(1)
            .filter(|dir| dir.join("project.ron").is_file())
        {
            let attempt = if path.file_name().is_some_and(|name| name == "project.ron") {
                Self::load(root)
            } else {
                Self::load_scene(root.to_path_buf(), Some(path))
            };
            match attempt {
                Ok(model) => return Ok(model),
                // A stray project.ron nested closer than the real one must not
                // hide it, so keep looking outward and report the first failure.
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| {
            EditorError::Invalid(format!(
                "{} is not inside a Spall project (no project.ron found in any parent folder)",
                path.display()
            ))
        }))
    }

    fn load_scene(root: PathBuf, wanted_scene: Option<&Path>) -> Result<Self, EditorError> {
        let project: ProjectFile = read_ron(&root.join("project.ron"))?;
        ensure_version("project", project.format_version, PROJECT_FORMAT_VERSION)?;
        ensure_version(
            "asset database",
            project.asset_database.format_version,
            PROJECT_FORMAT_VERSION,
        )?;
        let scene_path = match wanted_scene {
            None => project
                .scenes
                .values()
                .next()
                .ok_or_else(|| EditorError::Invalid("project has no scenes".into()))?,
            Some(wanted) => {
                let wanted = fs::canonicalize(wanted)?;
                project
                    .scenes
                    .values()
                    .find(|relative| {
                        fs::canonicalize(root.join(relative)).is_ok_and(|path| path == wanted)
                    })
                    .ok_or_else(|| {
                        EditorError::Invalid(format!(
                            "{} is not a scene listed in this project's project.ron",
                            wanted.display()
                        ))
                    })?
            }
        };
        let scene: SceneFile = read_ron(&root.join(scene_path))?;
        ensure_version("scene", scene.format_version, SCENE_FORMAT_VERSION)?;
        let mut voxel_assets = BTreeMap::new();
        for record in project.asset_database.assets.values() {
            if record.kind != AssetKind::Voxel {
                continue;
            }
            let asset = read_spvox(
                &root.join(&record.storage),
                record.id,
                &project.material_mapping,
            )?;
            voxel_assets.insert(asset.id, asset);
        }
        for entity in scene.entities.values() {
            if let Some(asset) = entity.voxel_asset
                && !project.asset_database.assets.contains_key(&asset)
            {
                return Err(EditorError::Invalid(format!(
                    "{} references missing {asset}",
                    entity.id
                )));
            }
        }
        Ok(Self {
            root,
            project,
            scene,
            voxel_assets,
        })
    }

    /// Builds an undoable command which imports a portable SPVX asset into
    /// this project. The project mapping is deliberately supplied by the
    /// project document, so an unknown material key fails clearly.
    pub fn import_spvox_command(&self, source: &Path) -> Result<EditorCommand, EditorError> {
        let id = AssetId(self.project.asset_database.next_asset_id);
        let asset = read_spvox(source, id, &self.project.material_mapping)?;
        Ok(self.create_import_command(id, asset))
    }

    /// Copies immutable engine-owned SPVX bytes into this project as a new,
    /// editable project asset. The engine catalog itself is never mutated.
    pub fn import_builtin_spvox_command(
        &self,
        bytes: &[u8],
        name: &str,
    ) -> Result<EditorCommand, EditorError> {
        let id = AssetId(self.project.asset_database.next_asset_id);
        let mut asset = spvox::decode(bytes, id, &self.project.material_mapping)?;
        asset.name = name.to_owned();
        Ok(self.create_import_command(id, asset))
    }

    /// Decodes an immutable bundled asset for read-only preview.
    pub fn preview_builtin_spvox(&self, bytes: &[u8]) -> Result<VoxelAssetFile, EditorError> {
        spvox::decode(bytes, AssetId(u64::MAX), &self.project.material_mapping)
    }

    fn create_import_command(&self, id: AssetId, asset: VoxelAssetFile) -> EditorCommand {
        let record = AssetRecord {
            id,
            name: asset.name.clone(),
            kind: AssetKind::Voxel,
            storage: PathBuf::from(format!("assets/{id}.spvox")),
            authoring_bounds: None,
        };
        EditorCommand::CreateVoxelAsset { record, asset }
    }

    /// Exports a selected project asset without exposing its project-local
    /// AssetId. The destination always receives canonical SPVX bytes.
    pub fn export_spvox_asset(
        &self,
        asset: AssetId,
        destination: &Path,
    ) -> Result<(), EditorError> {
        let asset = self
            .voxel_assets
            .get(&asset)
            .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
        write_spvox(destination, asset)
    }
}

/// Largest number of grid cells scanned when placing one transformed asset.
const MAX_PLACED_VOLUME: i64 = 4_000_000;
/// Metres per cell of the scene grid.
const SCENE_CELL_METRES: f32 = 0.0625;

type Mat3 = [[f32; 3]; 3];

fn rotation_matrix(degrees: [f32; 3]) -> Mat3 {
    let [ax, ay, az] = degrees.map(f32::to_radians);
    let (sx, cx) = ax.sin_cos();
    let (sy, cy) = ay.sin_cos();
    let (sz, cz) = az.sin_cos();
    // Each turn follows the editor's viewport convention: a positive Y turn
    // takes +X towards +Z.
    let rx = [[1.0, 0.0, 0.0], [0.0, cx, -sx], [0.0, sx, cx]];
    let ry = [[cy, 0.0, -sy], [0.0, 1.0, 0.0], [sy, 0.0, cy]];
    let rz = [[cz, -sz, 0.0], [sz, cz, 0.0], [0.0, 0.0, 1.0]];
    let mul = |a: Mat3, b: Mat3| {
        let mut out = [[0.0; 3]; 3];
        for (r, row) in out.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|k| a[r][k] * b[k][c]).sum();
            }
        }
        out
    };
    mul(rz, mul(ry, rx))
}

/// Emit every cell of `asset` after `transform`, in scene-grid coordinates.
///
/// Works backwards from each target cell centre to a source cell so scaled or
/// arbitrarily rotated assets come out solid rather than speckled.
#[cfg(test)]
fn place_asset(
    asset: &VoxelAssetFile,
    transform: &Transform,
    emit: impl FnMut(VoxelCoord, VoxelState),
) {
    place_asset_at(asset, transform, SCENE_CELL_METRES, emit);
}

/// [`place_asset`] onto a scene grid of `scene_cell_m` metres per cell.
fn place_asset_at(
    asset: &VoxelAssetFile,
    transform: &Transform,
    scene_cell_m: f32,
    mut emit: impl FnMut(VoxelCoord, VoxelState),
) {
    let (Some(min), Some(max)) = (
        asset.voxels.keys().copied().reduce(|a, b| VoxelCoord {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            z: a.z.min(b.z),
        }),
        asset.voxels.keys().copied().reduce(|a, b| VoxelCoord {
            x: a.x.max(b.x),
            y: a.y.max(b.y),
            z: a.z.max(b.z),
        }),
    ) else {
        return;
    };
    let asset_metres = if asset.cell_size_code == 0 {
        0.25
    } else {
        SCENE_CELL_METRES
    };
    let ratio = asset_metres / scene_cell_m;
    let scale = transform.scale.map(|s| s * ratio);
    if scale.iter().any(|s| *s == 0.0 || !s.is_finite()) {
        return;
    }
    let rotation = rotation_matrix(transform.rotation_degrees);
    let shift = transform.translation.map(|t| t / scene_cell_m);

    // Bounds of the transformed source box, in scene cells.
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for corner in 0..8 {
        let source = [
            if corner & 1 == 0 { min.x } else { max.x + 1 } as f32,
            if corner & 2 == 0 { min.y } else { max.y + 1 } as f32,
            if corner & 4 == 0 { min.z } else { max.z + 1 } as f32,
        ];
        for axis in 0..3 {
            let placed: f32 = (0..3)
                .map(|k| rotation[axis][k] * source[k] * scale[k])
                .sum::<f32>()
                + shift[axis];
            lo[axis] = lo[axis].min(placed);
            hi[axis] = hi[axis].max(placed);
        }
    }
    let first = lo.map(|v| v.floor() as i64);
    let last = hi.map(|v| v.ceil() as i64);
    let volume: i64 = (0..3)
        .map(|axis| (last[axis] - first[axis]).max(0))
        .product();
    if volume > MAX_PLACED_VOLUME {
        return;
    }
    for x in first[0]..last[0] {
        for y in first[1]..last[1] {
            for z in first[2]..last[2] {
                let centre = [x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5];
                let relative = [0, 1, 2].map(|axis| centre[axis] - shift[axis]);
                // Inverse rotation is the transpose.
                let source = [0, 1, 2].map(|axis| {
                    (0..3).map(|k| rotation[k][axis] * relative[k]).sum::<f32>() / scale[axis]
                });
                let cell = VoxelCoord {
                    x: source[0].floor() as i32,
                    y: source[1].floor() as i32,
                    z: source[2].floor() as i32,
                };
                if let Some(state) = asset.state_at(cell) {
                    emit(
                        VoxelCoord {
                            x: x as i32,
                            y: y as i32,
                            z: z as i32,
                        },
                        state,
                    );
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditorCommand {
    CreateVoxelAsset {
        record: AssetRecord,
        asset: VoxelAssetFile,
    },
    CreateEntity {
        entity: SceneEntity,
    },
    DeleteEntity {
        entity: SceneEntity,
    },
    SetTransform {
        entity: EditorEntityId,
        before: Transform,
        after: Transform,
    },
    SetEntityAsset {
        entity: EditorEntityId,
        before: Option<AssetId>,
        after: Option<AssetId>,
    },
    SetAssetBounds {
        asset: AssetId,
        before: Option<VoxelBounds>,
        after: Option<VoxelBounds>,
    },
    SetVoxel {
        asset: AssetId,
        cell: VoxelCoord,
        before: Option<VoxelState>,
        after: Option<VoxelState>,
    },
    /// One atomic authoring stroke, used for box fills. Every before-state is
    /// captured so undo restores a mixed selection exactly.
    SetVoxels {
        asset: AssetId,
        changes: Vec<VoxelChange>,
    },
    SetAssetLayers {
        asset: AssetId,
        before: Vec<VoxelLayer>,
        after: Vec<VoxelLayer>,
    },
    SetLayerVoxels {
        asset: AssetId,
        layer_id: u32,
        changes: Vec<VoxelLayerChange>,
    },
}

impl EditorCommand {
    pub fn apply(&self, model: &mut EditorModel) -> Result<(), EditorError> {
        match self {
            Self::CreateVoxelAsset { record, asset } => {
                if record.id != asset.id || record.kind != AssetKind::Voxel {
                    return Err(EditorError::Invalid(
                        "voxel asset command has mismatched database record".into(),
                    ));
                }
                if model.project.asset_database.assets.contains_key(&record.id)
                    || model.voxel_assets.contains_key(&record.id)
                {
                    return Err(EditorError::Invalid(format!(
                        "{0} already exists",
                        record.id
                    )));
                }
                if record.id.0 > model.project.asset_database.next_asset_id {
                    return Err(EditorError::Invalid(format!(
                        "{0} skips an asset id",
                        record.id
                    )));
                }
                model.project.asset_database.next_asset_id = model
                    .project
                    .asset_database
                    .next_asset_id
                    .max(record.id.0.saturating_add(1));
                model
                    .project
                    .asset_database
                    .assets
                    .insert(record.id, record.clone());
                model.voxel_assets.insert(asset.id, asset.clone());
            }
            Self::CreateEntity { entity } => {
                if model
                    .scene
                    .entities
                    .insert(entity.id, entity.clone())
                    .is_some()
                {
                    return Err(EditorError::Invalid(format!(
                        "{} already exists",
                        entity.id
                    )));
                }
            }
            Self::DeleteEntity { entity } => {
                let removed = model.scene.entities.remove(&entity.id);
                if removed.as_ref() != Some(entity) {
                    return Err(EditorError::Invalid(format!(
                        "{} changed before delete",
                        entity.id
                    )));
                }
            }
            Self::SetTransform {
                entity,
                before,
                after,
            } => {
                let item = model
                    .scene
                    .entities
                    .get_mut(entity)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {entity}")))?;
                if item.transform != *before {
                    return Err(EditorError::Invalid(format!(
                        "{entity} transform changed before command"
                    )));
                }
                item.transform = *after;
            }
            Self::SetEntityAsset {
                entity,
                before,
                after,
            } => {
                if let Some(asset) = after
                    && !model.project.asset_database.assets.contains_key(asset)
                {
                    return Err(EditorError::Invalid(format!(
                        "cannot assign missing {asset}"
                    )));
                }
                let item = model
                    .scene
                    .entities
                    .get_mut(entity)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {entity}")))?;
                if item.voxel_asset != *before {
                    return Err(EditorError::Invalid(format!(
                        "{entity} asset changed before command"
                    )));
                }
                item.voxel_asset = *after;
            }
            Self::SetAssetBounds {
                asset,
                before,
                after,
            } => {
                let record = model
                    .project
                    .asset_database
                    .assets
                    .get_mut(asset)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
                if record.authoring_bounds != *before {
                    return Err(EditorError::Invalid(format!(
                        "{asset} bounds changed before command"
                    )));
                }
                record.authoring_bounds = *after;
            }
            Self::SetVoxel {
                asset,
                cell,
                before,
                after,
            } => {
                let doc = model
                    .voxel_assets
                    .get_mut(asset)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
                if !doc.layers.is_empty() {
                    return Err(EditorError::Invalid(
                        "layered assets require a layer-targeted voxel command".into(),
                    ));
                }
                if doc.state_at(*cell) != *before {
                    return Err(EditorError::Invalid(format!(
                        "{asset} cell changed before command"
                    )));
                }
                doc.set_state(*cell, *after);
            }
            Self::SetVoxels { asset, changes } => {
                let doc = model
                    .voxel_assets
                    .get_mut(asset)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
                if !doc.layers.is_empty() {
                    return Err(EditorError::Invalid(
                        "layered assets require a layer-targeted voxel command".into(),
                    ));
                }
                for change in changes {
                    if doc.state_at(change.cell) != change.before {
                        return Err(EditorError::Invalid(format!(
                            "{asset} box selection changed before command"
                        )));
                    }
                }
                for change in changes {
                    doc.set_state(change.cell, change.after);
                }
            }
            Self::SetAssetLayers {
                asset,
                before,
                after,
            } => {
                let doc = model
                    .voxel_assets
                    .get_mut(asset)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
                if &doc.layers != before {
                    return Err(EditorError::Invalid(
                        "asset layers changed before command".into(),
                    ));
                }
                if !after.is_empty() {
                    let mut candidate = doc.clone();
                    candidate.layers = after.clone();
                    let (voxels, colors) = candidate.flattened_layers();
                    candidate.voxels = voxels;
                    candidate.colors = colors;
                    spvox::validate_layered_asset(&candidate)?;
                    *doc = candidate;
                } else {
                    doc.layers.clear();
                }
            }
            Self::SetLayerVoxels {
                asset,
                layer_id,
                changes,
            } => {
                let doc = model
                    .voxel_assets
                    .get_mut(asset)
                    .ok_or_else(|| EditorError::Invalid(format!("missing {asset}")))?;
                let mut candidate = doc.clone();
                let layer = candidate
                    .layers
                    .iter_mut()
                    .find(|layer| layer.id == *layer_id)
                    .ok_or_else(|| EditorError::Invalid(format!("missing layer {layer_id}")))?;
                let mut seen = BTreeSet::new();
                for change in changes {
                    if !seen.insert(change.cell) {
                        return Err(EditorError::Invalid(
                            "duplicate cell in layer stroke".into(),
                        ));
                    }
                    if layer.cells.get(&change.cell).copied() != change.before {
                        return Err(EditorError::Invalid(
                            "layer cells changed before command".into(),
                        ));
                    }
                    if change.after.is_some_and(|state| state.material == 0) {
                        return Err(EditorError::Invalid("layer cell cannot be air".into()));
                    }
                }
                for change in changes {
                    if let Some(state) = change.after {
                        layer.cells.insert(change.cell, state);
                    } else {
                        layer.cells.remove(&change.cell);
                    }
                }
                (candidate.voxels, candidate.colors) = candidate.flattened_layers();
                spvox::validate_layered_asset(&candidate)?;
                *doc = candidate;
            }
        }
        Ok(())
    }

    pub fn undo(&self, model: &mut EditorModel) -> Result<(), EditorError> {
        match self {
            Self::CreateVoxelAsset { record, asset } => {
                model.project.asset_database.assets.remove(&record.id);
                if model.voxel_assets.remove(&asset.id).as_ref() != Some(asset) {
                    return Err(EditorError::Invalid(format!(
                        "{0} changed before undo",
                        record.id
                    )));
                }
                Ok(())
            }
            Self::CreateEntity { entity } => Self::DeleteEntity {
                entity: entity.clone(),
            }
            .apply(model),
            Self::DeleteEntity { entity } => Self::CreateEntity {
                entity: entity.clone(),
            }
            .apply(model),
            Self::SetTransform {
                entity,
                before,
                after,
            } => Self::SetTransform {
                entity: *entity,
                before: *after,
                after: *before,
            }
            .apply(model),
            Self::SetEntityAsset {
                entity,
                before,
                after,
            } => Self::SetEntityAsset {
                entity: *entity,
                before: *after,
                after: *before,
            }
            .apply(model),
            Self::SetAssetBounds {
                asset,
                before,
                after,
            } => Self::SetAssetBounds {
                asset: *asset,
                before: *after,
                after: *before,
            }
            .apply(model),
            Self::SetVoxel {
                asset,
                cell,
                before,
                after,
            } => Self::SetVoxel {
                asset: *asset,
                cell: *cell,
                before: *after,
                after: *before,
            }
            .apply(model),
            Self::SetVoxels { asset, changes } => Self::SetVoxels {
                asset: *asset,
                changes: changes
                    .iter()
                    .map(|change| VoxelChange {
                        cell: change.cell,
                        before: change.after,
                        after: change.before,
                    })
                    .collect(),
            }
            .apply(model),
            Self::SetAssetLayers {
                asset,
                before,
                after,
            } => Self::SetAssetLayers {
                asset: *asset,
                before: after.clone(),
                after: before.clone(),
            }
            .apply(model),
            Self::SetLayerVoxels {
                asset,
                layer_id,
                changes,
            } => Self::SetLayerVoxels {
                asset: *asset,
                layer_id: *layer_id,
                changes: changes
                    .iter()
                    .map(|change| VoxelLayerChange {
                        cell: change.cell,
                        before: change.after,
                        after: change.before,
                    })
                    .collect(),
            }
            .apply(model),
        }
    }
}

#[derive(Debug, Default)]
pub struct UndoStack {
    undo: Vec<EditorCommand>,
    redo: Vec<EditorCommand>,
}

impl UndoStack {
    pub fn execute(
        &mut self,
        model: &mut EditorModel,
        command: EditorCommand,
    ) -> Result<(), EditorError> {
        command.apply(model)?;
        self.undo.push(command);
        self.redo.clear();
        Ok(())
    }
    pub fn undo(&mut self, model: &mut EditorModel) -> Result<bool, EditorError> {
        let Some(command) = self.undo.pop() else {
            return Ok(false);
        };
        command.undo(model)?;
        self.redo.push(command);
        Ok(true)
    }
    pub fn redo(&mut self, model: &mut EditorModel) -> Result<bool, EditorError> {
        let Some(command) = self.redo.pop() else {
            return Ok(false);
        };
        command.apply(model)?;
        self.undo.push(command);
        Ok(true)
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum EditorError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("RON error: {0}")]
    Ron(String),
    #[error("invalid editor document: {0}")]
    Invalid(String),
}

fn ensure_version(kind: &str, got: u32, expected: u32) -> Result<(), EditorError> {
    if got == expected {
        Ok(())
    } else {
        Err(EditorError::Invalid(format!(
            "{kind} format {got}; expected {expected}"
        )))
    }
}

fn write_ron<T: Serialize>(path: &Path, value: &T) -> Result<(), EditorError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = ron::ser::to_string_pretty(value, ron::ser::PrettyConfig::default())
        .map_err(|error| EditorError::Ron(error.to_string()))?;
    fs::write(path, text)?;
    Ok(())
}

fn read_ron<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, EditorError> {
    let text = fs::read_to_string(path)?;
    ron::from_str(&text).map_err(|error| EditorError::Ron(error.to_string()))
}

fn write_spvox(path: &Path, asset: &VoxelAssetFile) -> Result<(), EditorError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, spvox::encode(asset)?)?;
    Ok(())
}

fn read_spvox(
    path: &Path,
    id: AssetId,
    material_mapping: &BTreeMap<String, u16>,
) -> Result<VoxelAssetFile, EditorError> {
    let bytes = fs::read(path)?;
    spvox::decode(&bytes, id, material_mapping)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_voxel_asset(cell: VoxelCoord) -> VoxelAssetFile {
        let mut asset = VoxelAssetFile::new(AssetId(1), "one");
        asset.voxels.insert(cell, 1);
        asset.colors.insert(cell, [1, 2, 3]);
        asset
    }

    fn placed_cells(asset: &VoxelAssetFile, transform: Transform) -> Vec<VoxelCoord> {
        let mut cells = Vec::new();
        place_asset(asset, &transform, |cell, _| cells.push(cell));
        cells
    }

    #[test]
    fn placement_scales_solidly_and_rotates_about_any_axis() {
        let asset = one_voxel_asset(VoxelCoord { x: 0, y: 0, z: 0 });
        // Doubling the scale turns one cell into a 2x2x2 block.
        let scaled = placed_cells(
            &asset,
            Transform {
                scale: [2.0, 2.0, 2.0],
                ..Transform::default()
            },
        );
        assert_eq!(scaled.len(), 8);
        // A cell at +X turned 90 degrees about Z ends up along Y, not X.
        let off_axis = one_voxel_asset(VoxelCoord { x: 3, y: 0, z: 0 });
        let turned = placed_cells(
            &off_axis,
            Transform {
                rotation_degrees: [0.0, 0.0, 90.0],
                ..Transform::default()
            },
        );
        assert_eq!(turned, vec![VoxelCoord { x: -1, y: 3, z: 0 }]);
        // Coarse (0.25 m) cells fill 4x4x4 scene cells.
        let mut coarse = one_voxel_asset(VoxelCoord { x: 0, y: 0, z: 0 });
        coarse.cell_size_code = 0;
        assert_eq!(placed_cells(&coarse, Transform::default()).len(), 64);
    }

    #[test]
    fn scene_composite_places_assets_and_freehand_voxels_with_owners() {
        let mut model = EditorModel::new("unused", "test");
        let mut undo = UndoStack::default();
        // Freehand scene voxels.
        let scene_cmd = model.new_voxel_asset_command(SCENE_VOXELS_NAME);
        undo.execute(&mut model, scene_cmd).unwrap();
        let terrain = model.scene.new_entity(SCENE_VOXELS_NAME);
        let terrain_id = terrain.id;
        undo.execute(&mut model, EditorCommand::CreateEntity { entity: terrain })
            .unwrap();
        undo.execute(
            &mut model,
            EditorCommand::SetEntityAsset {
                entity: terrain_id,
                before: None,
                after: Some(AssetId(1)),
            },
        )
        .unwrap();
        let ground = VoxelCoord { x: 0, y: 0, z: 0 };
        undo.execute(
            &mut model,
            EditorCommand::SetVoxel {
                asset: AssetId(1),
                cell: ground,
                before: None,
                after: Some(VoxelState::new(1, [9, 9, 9])),
            },
        )
        .unwrap();
        assert_eq!(model.scene_voxels_asset(), Some((terrain_id, AssetId(1))));

        // A placed asset, moved 4 cells along X and turned a quarter.
        let crate_cmd = model.new_voxel_asset_command("crate");
        undo.execute(&mut model, crate_cmd).unwrap();
        let mut placed = model.scene.new_entity("Crate");
        let placed_id = placed.id;
        placed.voxel_asset = Some(AssetId(2));
        placed.transform.translation = [4.0 * 0.0625, 0.0, 0.0];
        placed.transform.rotation_degrees = [0.0, 90.0, 0.0];
        undo.execute(&mut model, EditorCommand::CreateEntity { entity: placed })
            .unwrap();
        undo.execute(
            &mut model,
            EditorCommand::SetVoxel {
                asset: AssetId(2),
                cell: VoxelCoord { x: 1, y: 0, z: 0 },
                before: None,
                after: Some(VoxelState::new(2, [1, 2, 3])),
            },
        )
        .unwrap();

        let (composite, owners) = model.scene_composite();
        assert_eq!(composite.voxels.len(), 2);
        assert_eq!(owners[&ground], terrain_id);
        // Cell (1,0,0) turned a quarter about Y lands on (-1,0,1); moving +4 in X gives (3,0,1).
        assert_eq!(owners[&VoxelCoord { x: 3, y: 0, z: 1 }], placed_id);
    }

    #[test]
    fn command_undo_redo_restores_voxels_and_stable_asset_references() {
        let mut model = EditorModel::new("unused", "test");
        let asset = AssetId(1);
        let entity = model.scene.new_entity("Crate");
        let entity_id = entity.id;
        let mut commands = UndoStack::default();
        let create_asset = model.new_voxel_asset_command("crate");
        commands.execute(&mut model, create_asset).unwrap();
        commands
            .execute(&mut model, EditorCommand::CreateEntity { entity })
            .unwrap();
        commands
            .execute(
                &mut model,
                EditorCommand::SetEntityAsset {
                    entity: entity_id,
                    before: None,
                    after: Some(asset),
                },
            )
            .unwrap();
        let cell = VoxelCoord { x: 1, y: 2, z: 3 };
        commands
            .execute(
                &mut model,
                EditorCommand::SetVoxel {
                    asset,
                    cell,
                    before: None,
                    after: Some(VoxelState::new(4, [1, 2, 3])),
                },
            )
            .unwrap();
        assert_eq!(model.scene.entities[&entity_id].voxel_asset, Some(asset));
        assert_eq!(
            model.voxel_assets[&asset].state_at(cell),
            Some(VoxelState::new(4, [1, 2, 3]))
        );
        commands.undo(&mut model).unwrap();
        assert!(!model.voxel_assets[&asset].voxels.contains_key(&cell));
        commands.redo(&mut model).unwrap();
        assert_eq!(
            model.voxel_assets[&asset].state_at(cell),
            Some(VoxelState::new(4, [1, 2, 3]))
        );
    }

    #[test]
    fn layer_commands_compose_and_undo_without_losing_source_cells() {
        let mut model = EditorModel::new(std::env::temp_dir(), "layers");
        let mut undo = UndoStack::default();
        let create = model.new_voxel_asset_command("tree");
        undo.execute(&mut model, create).unwrap();
        let asset = AssetId(1);
        let cell = VoxelCoord { x: 0, y: 0, z: 0 };
        let base = VoxelState::new(2, [80, 50, 20]);
        undo.execute(
            &mut model,
            EditorCommand::SetVoxel {
                asset,
                cell,
                before: None,
                after: Some(base),
            },
        )
        .unwrap();
        let layers = vec![
            VoxelLayer {
                id: 1,
                name: "Base".into(),
                visible: true,
                cells: BTreeMap::from([(
                    cell,
                    VoxelLayerCell {
                        material: 2,
                        tint: Some(base.color),
                    },
                )]),
            },
            VoxelLayer {
                id: 2,
                name: "Leaves".into(),
                visible: true,
                cells: BTreeMap::new(),
            },
        ];
        undo.execute(
            &mut model,
            EditorCommand::SetAssetLayers {
                asset,
                before: vec![],
                after: layers.clone(),
            },
        )
        .unwrap();
        let leaf = VoxelLayerCell {
            material: 3,
            tint: Some([20, 160, 30]),
        };
        undo.execute(
            &mut model,
            EditorCommand::SetLayerVoxels {
                asset,
                layer_id: 2,
                changes: vec![VoxelLayerChange {
                    cell,
                    before: None,
                    after: Some(leaf),
                }],
            },
        )
        .unwrap();
        assert_eq!(
            model.voxel_assets[&asset].state_at(cell),
            Some(VoxelState::new(3, [20, 160, 30]))
        );
        let mut hidden = model.voxel_assets[&asset].layers.clone();
        let before_hide = hidden.clone();
        hidden[1].visible = false;
        undo.execute(
            &mut model,
            EditorCommand::SetAssetLayers {
                asset,
                before: before_hide,
                after: hidden,
            },
        )
        .unwrap();
        assert_eq!(model.voxel_assets[&asset].state_at(cell), Some(base));
        undo.undo(&mut model).unwrap();
        assert_eq!(
            model.voxel_assets[&asset].state_at(cell),
            Some(VoxelState::new(3, [20, 160, 30]))
        );
        undo.undo(&mut model).unwrap();
        assert_eq!(model.voxel_assets[&asset].layers, layers);
        assert_eq!(model.voxel_assets[&asset].state_at(cell), Some(base));
        undo.redo(&mut model).unwrap();
        assert_eq!(
            model.voxel_assets[&asset].state_at(cell),
            Some(VoxelState::new(3, [20, 160, 30]))
        );
    }

    #[test]
    fn save_and_load_keep_scene_asset_ids() {
        let root = std::env::temp_dir().join(format!("spall-editor-{}", std::process::id()));
        let mut model = EditorModel::new(&root, "roundtrip");
        let asset = AssetId(1);
        let entity = model.scene.new_entity("Placed brick");
        let id = entity.id;
        let mut commands = UndoStack::default();
        let create_asset = model.new_voxel_asset_command("brick");
        commands.execute(&mut model, create_asset).unwrap();
        commands
            .execute(&mut model, EditorCommand::CreateEntity { entity })
            .unwrap();
        commands
            .execute(
                &mut model,
                EditorCommand::SetEntityAsset {
                    entity: id,
                    before: None,
                    after: Some(asset),
                },
            )
            .unwrap();
        let colored = VoxelCoord { x: 2, y: 1, z: -1 };
        commands
            .execute(
                &mut model,
                EditorCommand::SetVoxel {
                    asset,
                    cell: colored,
                    before: None,
                    after: Some(VoxelState::new(3, [34, 150, 61])),
                },
            )
            .unwrap();
        model.save_all().unwrap();
        let loaded = EditorModel::load(&root).unwrap();
        assert_eq!(loaded.scene.entities[&id].voxel_asset, Some(asset));
        assert!(loaded.project.asset_database.assets.contains_key(&asset));
        assert_eq!(
            loaded.voxel_assets[&asset].state_at(colored),
            Some(VoxelState::new(3, [34, 150, 61]))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn box_stroke_restores_each_prior_colored_voxel_on_undo() {
        let mut model = EditorModel::new("unused", "test");
        let asset = AssetId(1);
        let mut commands = UndoStack::default();
        let create_asset = model.new_voxel_asset_command("tree");
        commands.execute(&mut model, create_asset).unwrap();
        let first = VoxelCoord { x: 0, y: 0, z: 0 };
        let second = VoxelCoord { x: 1, y: 0, z: 0 };
        commands
            .execute(
                &mut model,
                EditorCommand::SetVoxel {
                    asset,
                    cell: first,
                    before: None,
                    after: Some(VoxelState::new(3, [20, 120, 30])),
                },
            )
            .unwrap();
        commands
            .execute(
                &mut model,
                EditorCommand::SetVoxels {
                    asset,
                    changes: vec![
                        VoxelChange {
                            cell: first,
                            before: Some(VoxelState::new(3, [20, 120, 30])),
                            after: Some(VoxelState::new(3, [40, 140, 50])),
                        },
                        VoxelChange {
                            cell: second,
                            before: None,
                            after: Some(VoxelState::new(2, [90, 55, 30])),
                        },
                    ],
                },
            )
            .unwrap();
        commands.undo(&mut model).unwrap();
        let voxels = &model.voxel_assets[&asset];
        assert_eq!(
            voxels.state_at(first),
            Some(VoxelState::new(3, [20, 120, 30]))
        );
        assert_eq!(voxels.state_at(second), None);
    }

    #[test]
    fn build_bounds_are_undoable_project_metadata() {
        let root = std::env::temp_dir().join(format!("spall-editor-bounds-{}", std::process::id()));
        let mut model = EditorModel::new(&root, "test");
        let asset = AssetId(1);
        let mut commands = UndoStack::default();
        let create_asset = model.new_voxel_asset_command("tree");
        commands.execute(&mut model, create_asset).unwrap();
        let bounds = VoxelBounds::new(
            VoxelCoord { x: -2, y: 0, z: -2 },
            VoxelCoord { x: 2, y: 6, z: 2 },
        );
        assert_eq!(bounds.cell_count(), Some(175));
        commands
            .execute(
                &mut model,
                EditorCommand::SetAssetBounds {
                    asset,
                    before: None,
                    after: Some(bounds),
                },
            )
            .unwrap();
        assert_eq!(
            model.project.asset_database.assets[&asset].authoring_bounds,
            Some(bounds)
        );
        commands.undo(&mut model).unwrap();
        assert_eq!(
            model.project.asset_database.assets[&asset].authoring_bounds,
            None
        );
        commands.redo(&mut model).unwrap();
        assert!(
            model.project.asset_database.assets[&asset]
                .authoring_bounds
                .unwrap()
                .contains(VoxelCoord { x: 2, y: 6, z: 2 })
        );
        model.save_all().unwrap();
        let loaded = EditorModel::load(&root).unwrap();
        assert_eq!(
            loaded.project.asset_database.assets[&asset].authoring_bounds,
            Some(bounds)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builtin_copy_is_undoable_saved_to_project_and_keeps_bundle_unchanged() {
        let root =
            std::env::temp_dir().join(format!("spall-editor-builtin-{}", std::process::id()));
        let bundled = spall_voxel::builtin_assets::builtin_voxel_assets()[0];
        let source_digest = blake3::hash(bundled.bytes);
        let mut model = EditorModel::new(&root, "builtin-copy");
        let command = model
            .import_builtin_spvox_command(bundled.bytes, bundled.name)
            .unwrap();
        let asset = match &command {
            EditorCommand::CreateVoxelAsset { record, .. } => record.id,
            _ => unreachable!(),
        };
        let mut undo = UndoStack::default();
        undo.execute(&mut model, command).unwrap();
        let source_voxel_count = spvox::decode(
            bundled.bytes,
            AssetId(u64::MAX),
            &model.project.material_mapping,
        )
        .unwrap()
        .voxels
        .len();
        assert!(source_voxel_count > 0);
        assert_eq!(model.voxel_assets[&asset].voxels.len(), source_voxel_count);
        model.save_all().unwrap();
        let loaded = EditorModel::load(&root).unwrap();
        assert!(loaded.project.asset_database.assets.contains_key(&asset));
        assert_eq!(blake3::hash(bundled.bytes), source_digest);
        undo.undo(&mut model).unwrap();
        assert!(!model.voxel_assets.contains_key(&asset));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_file_opens_a_folder_project_ron_or_scene_ron() {
        let root = std::env::temp_dir().join(format!("spall-open-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut model = EditorModel::new(&root, "Open Test");
        let marker = model.scene.new_entity("Marker");
        model.scene.entities.insert(marker.id, marker);
        model.save_all().unwrap();
        for path in [
            root.clone(),
            root.join("project.ron"),
            root.join("scenes").join("main.ron"),
        ] {
            let loaded = EditorModel::load_file(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            assert_eq!(loaded.root, root, "{path:?}");
            assert_eq!(loaded.scene.entities.len(), 1, "{path:?}");
        }
        // A stray project.ron nested next to the scene must not hide the real one.
        let nested = root.join("scenes").join("inner");
        fs::create_dir_all(&nested).unwrap();
        EditorModel::new(&nested, "Stray").save_all().unwrap();
        let loaded = EditorModel::load_file(root.join("scenes").join("main.ron")).unwrap();
        assert_eq!(loaded.root, root);
        // A .ron that is not a listed scene, and a file outside any project, fail clearly.
        fs::write(root.join("scenes").join("stray.ron"), "()").unwrap();
        let stray = EditorModel::load_file(root.join("scenes").join("stray.ron")).unwrap_err();
        assert!(stray.to_string().contains("not a scene listed"), "{stray}");
        let outside =
            std::env::temp_dir().join(format!("spall-outside-{}.ron", std::process::id()));
        fs::write(&outside, "()").unwrap();
        let error = EditorModel::load_file(&outside).unwrap_err();
        assert!(
            error.to_string().contains("not inside a Spall project"),
            "{error}"
        );
        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&root);
    }
}
