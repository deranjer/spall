//! Plays a scene authored in Spall Editor.
//!
//! Reads an editor project directory (`project.ron`, the scene `.ron` it names,
//! and the `.spvox` assets the scene places), flattens the placed assets into
//! one 0.25 m runtime terrain volume, and packages it as a
//! [`spall_server::CustomWorld`]. The editor crate itself is not linked: it
//! pulls in the window/GPU stack, and this module runs in the headless server.
//! Only the fields of the editor's RON files that playback needs are read.
//!
//! Runtime cells carry a material and no tint, so authored display tints are
//! dropped and appearance comes from [`material_mapping`].

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use spall_core::{CellSizeCode, GlobalCell, MaterialId, VolumeId};
use spall_server::CustomWorld;
use spall_sim::WorldSetup;
use spall_structure::AnchorPlane;
use spall_voxel::EditPlan;
use thiserror::Error;

use crate::content::{self, AssetId};
use crate::game::materials;

/// Resident air kept beyond the solid bounds. Character queries build a
/// +-16-cell window around the feet, and any cell in it that was never made
/// resident degrades that query (see `spall_voxel::fixtures::playground_scene`).
const AIR_MARGIN_XZ: i64 = 24;
const AIR_BELOW: i64 = 20;
const AIR_ABOVE: i64 = 24;
/// Refuses a single placement larger than this many target cells.
const MAX_PLACED_CELLS: i64 = 16_000_000;
const RUNTIME_CELL_M: f32 = 0.25;
/// Editor translations are stored in metres.
const MAX_PLAYER_SPAWNS: usize = 4;
/// Clear cells above a spawn surface: 2.25 m of headroom for the capsule.
const SPAWN_HEADROOM_CELLS: i64 = 9;

#[derive(Debug, Error)]
pub enum EditorSceneError {
    #[error("{path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("project has no scene named `{0}`")]
    UnknownScene(String),
    #[error("entity `{entity}` references asset {asset} which is not in the project")]
    MissingAsset { entity: String, asset: u64 },
    #[error("asset `{asset}`: {message}")]
    Asset { asset: String, message: String },
    #[error("entity `{entity}` has an unsupported transform: {message}")]
    Transform { entity: String, message: String },
    #[error("scene has no solid voxels to play")]
    Empty,
    #[error(
        "no spawn point with {SPAWN_HEADROOM_CELLS} clear cells of headroom found in the scene"
    )]
    NoSpawn,
}

/// The editor writes ids as newtype tuples (`(1)`), including as map keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct Id(u64);

#[derive(Debug, Deserialize)]
struct Project {
    scenes: BTreeMap<String, String>,
    asset_database: AssetDatabase,
}

#[derive(Debug, Deserialize)]
struct AssetDatabase {
    assets: BTreeMap<Id, AssetRecord>,
}

#[derive(Debug, Deserialize)]
struct AssetRecord {
    name: String,
    storage: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Scene {
    entities: BTreeMap<Id, Entity>,
}

#[derive(Debug, Deserialize)]
struct Entity {
    name: String,
    transform: Transform,
    voxel_asset: Option<Id>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Transform {
    translation: [f32; 3],
    rotation_degrees: [f32; 3],
    scale: [f32; 3],
}

/// Editor portable material keys -> sandbox runtime materials. The sandbox
/// manifest has no leaf or lawn material of its own, so `foliage.oak` reuses
/// the playground palette's green until it does.
pub fn material_mapping() -> BTreeMap<String, MaterialId> {
    [
        ("stone.granite", materials::STONE),
        ("wood.oak", materials::WOOD),
        ("foliage.oak", MaterialId(18)),
        ("sandstone", MaterialId(13)),
        ("grass", MaterialId(10)),
        ("dirt", materials::DIRT),
        ("emissive.lamp", materials::LAMP),
    ]
    .into_iter()
    .map(|(key, id)| (key.to_owned(), id))
    .collect()
}

/// A scene flattened to runtime cells, ready to build a world from.
#[derive(Debug)]
pub struct LoadedScene {
    cells: BTreeMap<(i64, i64, i64), MaterialId>, // (z, y, x) -> material
    min: [i64; 3],
    max: [i64; 3],
    pub player_spawns: Vec<[f64; 3]>,
}

impl LoadedScene {
    pub fn solid_cell_count(&self) -> usize {
        self.cells.len()
    }

    pub fn bounds(&self) -> ([i64; 3], [i64; 3]) {
        (self.min, self.max)
    }

    /// Builds the terrain, materials and physics config for this scene.
    pub fn world_setup(&self) -> WorldSetup {
        let id = VolumeId::new(1).expect("nonzero volume id");
        let lo = GlobalCell::new(
            self.min[0] - AIR_MARGIN_XZ,
            self.min[1] - AIR_BELOW,
            self.min[2] - AIR_MARGIN_XZ,
        );
        let hi = GlobalCell::new(
            self.max[0] + AIR_MARGIN_XZ,
            self.max[1] + AIR_ABOVE,
            self.max[2] + AIR_MARGIN_XZ,
        );
        let mut terrain = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
        terrain
            .apply_edit(&EditPlan::filled_box(id, lo, hi, MaterialId::AIR))
            .expect("resident air envelope");
        let mut solid = EditPlan::new(id);
        for (&(z, y, x), &material) in &self.cells {
            solid.set(GlobalCell::new(x, y, z), material);
        }
        terrain.apply_edit(&solid).expect("scene voxels");
        // Same as every other server scene: a committed cut rebuilds the
        // terrain collider, and CCD's stale proxy can panic mid-sweep.
        let physics = spall_physics::PhysicsConfig {
            disable_ccd: true,
            ..Default::default()
        };
        WorldSetup {
            terrain,
            terrain_collider_region: (lo, hi),
            materials: crate::game::manifest(),
            // The scene's lowest layer is the declared support plane.
            anchor: AnchorPlane::at(self.min[1]),
            physics,
        }
    }

    /// Packages the scene for [`spall_server::ServeConfig::custom_world`].
    pub fn into_custom_world(self) -> CustomWorld {
        let spawns = self.player_spawns.clone();
        let scene = Arc::new(self);
        CustomWorld::new(spawns, move || scene.world_setup())
    }
}

/// Resolves `path` to a project directory: either the directory itself, or a
/// scene file inside it (`<project>/scenes/<name>.ron`, what the editor's Run
/// passes). Returns the directory and, for a scene file, its path.
fn resolve(path: &Path) -> (PathBuf, Option<PathBuf>) {
    if path.is_dir() {
        return (path.to_owned(), None);
    }
    let root = path
        .ancestors()
        .skip(1)
        .find(|dir| dir.join("project.ron").is_file())
        .unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")))
        .to_owned();
    (root, Some(path.to_owned()))
}

fn read_ron<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, EditorSceneError> {
    let read = |message: String| EditorSceneError::Read {
        path: path.to_owned(),
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|e| read(e.to_string()))?;
    ron::from_str(&text).map_err(|e| read(e.to_string()))
}

/// Loads an editor scene. `path` is a project directory (plays its `Main`
/// scene, or its only scene) or a scene `.ron` file.
pub fn load(path: &Path) -> Result<LoadedScene, EditorSceneError> {
    let (root, scene_file) = resolve(path);
    let project: Project = read_ron(&root.join("project.ron"))?;
    let scene_file = match scene_file {
        Some(file) => file,
        None => {
            let relative = project
                .scenes
                .get("Main")
                .or_else(|| {
                    (project.scenes.len() == 1)
                        .then(|| project.scenes.values().next())
                        .flatten()
                })
                .ok_or_else(|| EditorSceneError::UnknownScene("Main".into()))?;
            root.join(relative)
        }
    };
    let scene: Scene = read_ron(&scene_file)?;
    let mapping = material_mapping();

    let mut decoded: BTreeMap<Id, content::LoadedVoxelAsset> = BTreeMap::new();
    let mut cells: BTreeMap<(i64, i64, i64), MaterialId> = BTreeMap::new();
    // BTreeMap order is entity id order, matching the editor's "later wins".
    for entity in scene.entities.values() {
        let Some(asset_id) = entity.voxel_asset else {
            continue;
        };
        let asset = match decoded.entry(asset_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let record = project
                    .asset_database
                    .assets
                    .get(&asset_id)
                    .ok_or_else(|| EditorSceneError::MissingAsset {
                        entity: entity.name.clone(),
                        asset: asset_id.0,
                    })?;
                entry.insert(load_asset(&root, asset_id, record, &mapping)?)
            }
        };
        place(asset, entity, &mut cells)?;
    }
    let (min, max) = cells
        .keys()
        .fold(None, |acc: Option<([i64; 3], [i64; 3])>, &(z, y, x)| {
            let c = [x, y, z];
            Some(match acc {
                None => (c, c),
                Some((lo, hi)) => (
                    [0, 1, 2].map(|i| lo[i].min(c[i])),
                    [0, 1, 2].map(|i| hi[i].max(c[i])),
                ),
            })
        })
        .ok_or(EditorSceneError::Empty)?;
    let player_spawns = find_spawns(&cells, min, max)?;
    Ok(LoadedScene {
        cells,
        min,
        max,
        player_spawns,
    })
}

fn load_asset(
    root: &Path,
    id: Id,
    record: &AssetRecord,
    mapping: &BTreeMap<String, MaterialId>,
) -> Result<content::LoadedVoxelAsset, EditorSceneError> {
    let file = root.join(&record.storage);
    let asset_error = |message: String| EditorSceneError::Asset {
        asset: record.name.clone(),
        message,
    };
    let bytes =
        std::fs::read(&file).map_err(|e| asset_error(format!("{}: {e}", file.display())))?;
    content::decode_editor_voxels(AssetId(id.0), &bytes, mapping)
        .map_err(|e| asset_error(e.to_string()))
}

type Mat3 = [[f32; 3]; 3];

/// Mirrors the editor's `rotation_matrix`: a positive Y turn takes +X toward +Z.
fn rotation_matrix(degrees: [f32; 3]) -> Mat3 {
    let [ax, ay, az] = degrees.map(f32::to_radians);
    let (sx, cx) = ax.sin_cos();
    let (sy, cy) = ay.sin_cos();
    let (sz, cz) = az.sin_cos();
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

/// Places one asset at the runtime cell size. Works backwards from each target
/// cell centre to a source cell (as the editor does) so rotated or scaled
/// placements come out solid rather than speckled.
fn place(
    asset: &content::LoadedVoxelAsset,
    entity: &Entity,
    out: &mut BTreeMap<(i64, i64, i64), MaterialId>,
) -> Result<(), EditorSceneError> {
    let bad = |message: &str| EditorSceneError::Transform {
        entity: entity.name.clone(),
        message: message.into(),
    };
    let t = entity.transform;
    // Editor assets are 0.25 m (code 0) or 0.0625 m (code 1) cells.
    let source_m = asset.cell_size.metres() as f32;
    let ratio = source_m / RUNTIME_CELL_M;
    let scale = t.scale.map(|s| s * ratio);
    if scale.iter().any(|s| *s <= 0.0 || !s.is_finite()) {
        return Err(bad("scale must be positive and finite"));
    }
    if t.translation.iter().any(|v| !v.is_finite()) {
        return Err(bad("translation must be finite"));
    }
    let rotation = rotation_matrix(t.rotation_degrees);
    let shift = t.translation.map(|v| v / RUNTIME_CELL_M);

    // A tinted cell becomes the nearest appearance variant of its material,
    // so the authored colour is a material id the engine already carries
    // through edits, splits, replication and saves.
    let resolver = crate::appearance::Resolver::default();
    let material_at: BTreeMap<(i32, i32, i32), MaterialId> = asset
        .cells
        .iter()
        .map(|c| {
            (
                (c.cell.x as i32, c.cell.y as i32, c.cell.z as i32),
                resolver.resolve(c.material, c.tint),
            )
        })
        .collect();
    let mut lo = [i32::MAX; 3];
    let mut hi = [i32::MIN; 3];
    for &(x, y, z) in material_at.keys() {
        for (axis, v) in [x, y, z].into_iter().enumerate() {
            lo[axis] = lo[axis].min(v);
            hi[axis] = hi[axis].max(v);
        }
    }
    // Bounds of the transformed source box, in runtime cells.
    let mut tlo = [f32::INFINITY; 3];
    let mut thi = [f32::NEG_INFINITY; 3];
    for corner in 0..8 {
        let src = [
            if corner & 1 == 0 { lo[0] } else { hi[0] + 1 } as f32,
            if corner & 2 == 0 { lo[1] } else { hi[1] + 1 } as f32,
            if corner & 4 == 0 { lo[2] } else { hi[2] + 1 } as f32,
        ];
        for axis in 0..3 {
            let placed: f32 = (0..3)
                .map(|k| rotation[axis][k] * src[k] * scale[k])
                .sum::<f32>()
                + shift[axis];
            tlo[axis] = tlo[axis].min(placed);
            thi[axis] = thi[axis].max(placed);
        }
    }
    let first = tlo.map(|v| v.floor() as i64);
    let last = thi.map(|v| v.ceil() as i64);
    let volume: i64 = (0..3).map(|a| (last[a] - first[a]).max(0)).product();
    if volume > MAX_PLACED_CELLS {
        return Err(bad("placed volume is too large"));
    }
    for x in first[0]..last[0] {
        for y in first[1]..last[1] {
            for z in first[2]..last[2] {
                let centre = [x as f32 + 0.5, y as f32 + 0.5, z as f32 + 0.5];
                let relative = [0, 1, 2].map(|a| centre[a] - shift[a]);
                // The inverse rotation is the transpose.
                let src = [0, 1, 2]
                    .map(|a| (0..3).map(|k| rotation[k][a] * relative[k]).sum::<f32>() / scale[a]);
                let key = (
                    src[0].floor() as i32,
                    src[1].floor() as i32,
                    src[2].floor() as i32,
                );
                if let Some(&material) = material_at.get(&key) {
                    out.insert((z, y, x), material);
                }
            }
        }
    }
    Ok(())
}

/// Picks up to [`MAX_PLAYER_SPAWNS`] standing spots on the scene's surface,
/// nearest the horizontal centre first, each with clear headroom and spaced
/// apart. The surface is the highest solid cell of a column; feet rest on it.
fn find_spawns(
    cells: &BTreeMap<(i64, i64, i64), MaterialId>,
    min: [i64; 3],
    max: [i64; 3],
) -> Result<Vec<[f64; 3]>, EditorSceneError> {
    let mut top: BTreeMap<(i64, i64), i64> = BTreeMap::new(); // (x, z) -> highest y
    for &(z, y, x) in cells.keys() {
        let slot = top.entry((x, z)).or_insert(y);
        *slot = (*slot).max(y);
    }
    let solid = |x: i64, y: i64, z: i64| cells.contains_key(&(z, y, x));
    let (cx, cz) = ((min[0] + max[0]) / 2, (min[2] + max[2]) / 2);
    let mut candidates: Vec<(i64, i64, i64)> = top.iter().map(|(&(x, z), &y)| (x, y, z)).collect();
    candidates.sort_by_key(|&(x, _, z)| ((x - cx).pow(2) + (z - cz).pow(2), x, z));
    let mut spawns: Vec<(i64, i64, i64)> = Vec::new();
    for (x, y, z) in candidates {
        if spawns.len() == MAX_PLAYER_SPAWNS {
            break;
        }
        // A 3x3 footprint (0.75 m) with clear headroom above the surface.
        let clear = (-1..=1).all(|dx| {
            (-1..=1).all(|dz| {
                let column_top = top.get(&(x + dx, z + dz)).copied();
                column_top.is_some_and(|t| (t - y).abs() <= 1)
                    && (y + 1..=y + SPAWN_HEADROOM_CELLS).all(|yy| !solid(x + dx, yy, z + dz))
            })
        });
        let spaced = spawns
            .iter()
            .all(|&(sx, _, sz)| (sx - x).abs().max((sz - z).abs()) >= 3);
        if clear && spaced {
            spawns.push((x, y, z));
        }
    }
    if spawns.is_empty() {
        return Err(EditorSceneError::NoSpawn);
    }
    Ok(spawns
        .into_iter()
        .map(|(x, y, z)| {
            let m = f64::from(RUNTIME_CELL_M);
            [
                (x as f64 + 0.5) * m,
                (y + 1) as f64 * m,
                (z as f64 + 0.5) * m,
            ]
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset_with(cells: &[(i64, i64, i64)]) -> content::LoadedVoxelAsset {
        content::LoadedVoxelAsset {
            id: AssetId(1),
            name: "t".into(),
            cell_size: CellSizeCode::Quarter,
            pivot_subcells: [0; 3],
            cells: cells
                .iter()
                .map(|&(x, y, z)| content::LoadedVoxelCell {
                    cell: GlobalCell::new(x, y, z),
                    material: materials::WOOD,
                    tint: None,
                })
                .collect(),
        }
    }

    fn entity(translation: [f32; 3], yaw: f32) -> Entity {
        Entity {
            name: "e".into(),
            transform: Transform {
                translation,
                rotation_degrees: [0.0, yaw, 0.0],
                scale: [1.0; 3],
            },
            voxel_asset: None,
        }
    }

    #[test]
    fn placement_translates_and_quarter_turns_like_the_editor() {
        let asset = asset_with(&[(1, 0, 0)]);
        let mut out = BTreeMap::new();
        // 1 m = 4 runtime cells along +X.
        place(&asset, &entity([1.0, 0.0, 0.0], 0.0), &mut out).unwrap();
        assert_eq!(out.keys().copied().collect::<Vec<_>>(), vec![(0, 0, 5)]);
        // Same as the editor's own test: cell (1,0,0) turned a quarter about Y
        // lands on x = -1, z = 1 (keys are (z, y, x)).
        let mut turned = BTreeMap::new();
        place(&asset, &entity([0.0; 3], 90.0), &mut turned).unwrap();
        assert_eq!(turned.keys().copied().collect::<Vec<_>>(), vec![(1, 0, -1)]);
    }

    #[test]
    fn spawn_avoids_columns_with_overhead_geometry() {
        let mut cells = BTreeMap::new();
        for x in 0..21 {
            for z in 0..21 {
                cells.insert((z, 0, x), materials::DIRT);
            }
        }
        // A canopy directly over the centre blocks that column's headroom.
        cells.insert((10, 5, 10), materials::WOOD);
        let spawns = find_spawns(&cells, [0, 0, 0], [20, 5, 20]).unwrap();
        let blocked = spawns
            .iter()
            .any(|s| (s[0] - 2.625).abs() < 0.5 && (s[2] - 2.625).abs() < 0.5);
        assert!(!blocked, "spawned under the canopy: {spawns:?}");
        // Feet rest on the surface (top cell 0 -> y = 0.25 m).
        assert!(spawns.iter().all(|s| (s[1] - 0.25).abs() < 1e-9));
    }
}
