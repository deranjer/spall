//! Editor-owned documents and commands. This crate is deliberately a leaf of
//! the workspace: Spall's runtime and renderer never import egui or editor
//! persistence types.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod spvox;

pub const PROJECT_FORMAT_VERSION: u32 = 1;
pub const SCENE_FORMAT_VERSION: u32 = 1;
pub const VOXEL_FORMAT_VERSION: u32 = 1;

/// A stable project-local identity. Scene files refer to assets only by this
/// value; moving an asset's on-disk file does not rewrite a scene.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub u64);

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
    pub entities: BTreeMap<EditorEntityId, SceneEntity>,
}

impl SceneFile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            format_version: SCENE_FORMAT_VERSION,
            name: name.into(),
            next_entity_id: 1,
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
        }
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
        let root = root.into();
        let project: ProjectFile = read_ron(&root.join("project.ron"))?;
        ensure_version("project", project.format_version, PROJECT_FORMAT_VERSION)?;
        ensure_version(
            "asset database",
            project.asset_database.format_version,
            PROJECT_FORMAT_VERSION,
        )?;
        let (_, scene_path) = project
            .scenes
            .iter()
            .next()
            .ok_or_else(|| EditorError::Invalid("project has no scenes".into()))?;
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
        Ok(EditorCommand::CreateVoxelAsset {
            record: AssetRecord {
                id,
                name: asset.name.clone(),
                kind: AssetKind::Voxel,
                storage: PathBuf::from(format!("assets/{id}.spvox")),
                authoring_bounds: None,
            },
            asset,
        })
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
}
