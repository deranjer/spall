//! Simple grass forest test scene using all four tree assets (palm, willow and
//! their `_2` revisions). Reads the two existing tree projects and writes an
//! openable project to `fixtures/terrain-trees-forest` (or the first argument).
//!
//! Run from the repository root after `terrain_trees` and `terrain_trees_v2`:
//! `cargo run -p spall_editor --example forest_scene`
use std::path::PathBuf;

use spall_editor::{
    AssetId, EditorCommand, EditorModel, SCENE_VOXELS_NAME, Transform, VoxelAssetFile, VoxelCoord,
};

// Project-local numeric ids for the ground's portable material keys.
const GRASS: u16 = 5;
const DIRT: u16 = 6;
/// Ground half-extent in 0.25 m cells: 160 cells = 40 m square.
const SIZE: i32 = 160;
const GROUND_TOP_M: f32 = 0.5; // two 0.25 m cells

fn hash(a: i32, b: i32, seed: u32) -> f32 {
    let mut h = seed as u64 ^ 0x9E37_79B9_7F4A_7C15;
    for v in [a, b] {
        h = (h ^ (v as u32 as u64)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 31;
    }
    ((h >> 11) as f32) / ((1u64 << 53) as f32)
}

fn ground() -> VoxelAssetFile {
    let mut g = VoxelAssetFile::new(AssetId(0), SCENE_VOXELS_NAME);
    g.cell_size_code = 0;
    g.material_keys.insert(GRASS, "grass".into());
    g.material_keys.insert(DIRT, "dirt".into());
    for z in 0..SIZE {
        for x in 0..SIZE {
            // Broad patches of lighter/darker grass plus fine speckle.
            let patch = hash(x / 12, z / 12, 1);
            let speckle = hash(x, z, 2);
            let base: [i32; 3] = [78, 128, 52];
            let d = ((patch - 0.5) * 30.0 + (speckle - 0.5) * 12.0) as i32;
            let grass = [
                (base[0] + d).clamp(0, 255) as u8,
                (base[1] + d).clamp(0, 255) as u8,
                (base[2] + d / 2).clamp(0, 255) as u8,
            ];
            let top = VoxelCoord { x, y: 1, z };
            g.voxels.insert(top, GRASS);
            g.colors.insert(top, grass);
            let under = VoxelCoord { x, y: 0, z };
            g.voxels.insert(under, DIRT);
            g.colors.insert(under, [104, 78, 52]);
        }
    }
    g
}

fn load_tree(project: &str, index: usize) -> Result<VoxelAssetFile, Box<dyn std::error::Error>> {
    let model = EditorModel::load(project)?;
    Ok(model
        .voxel_assets
        .into_values()
        .nth(index)
        .ok_or("missing tree asset")?)
}

fn add_asset(
    model: &mut EditorModel,
    mut asset: VoxelAssetFile,
    file_name: &str,
) -> Result<AssetId, Box<dyn std::error::Error>> {
    let command = model.new_voxel_asset_command(asset.name.clone());
    let EditorCommand::CreateVoxelAsset { mut record, .. } = command else {
        unreachable!()
    };
    record.storage = PathBuf::from("assets").join(file_name);
    asset.id = record.id;
    let id = record.id;
    EditorCommand::CreateVoxelAsset { record, asset }.apply(model)?;
    Ok(id)
}

fn place(model: &mut EditorModel, name: &str, asset: AssetId, at: [f32; 3], yaw: f32) {
    let mut entity = model.scene.new_entity(name);
    entity.voxel_asset = Some(asset);
    entity.transform = Transform {
        translation: at,
        rotation_degrees: [0.0, yaw, 0.0],
        ..Transform::default()
    };
    model.scene.entities.insert(entity.id, entity);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/terrain-trees-forest"));
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Terrain Forest");
    // The ground uses its own keys rather than tinted foliage/sandstone.
    model.project.material_mapping.insert("grass".into(), GRASS);
    model.project.material_mapping.insert("dirt".into(), DIRT);

    let palm = add_asset(
        &mut model,
        load_tree("fixtures/terrain-trees", 0)?,
        "palm_tree.spvox",
    )?;
    let willow = add_asset(
        &mut model,
        load_tree("fixtures/terrain-trees", 1)?,
        "weeping_willow.spvox",
    )?;
    let palm2 = add_asset(
        &mut model,
        load_tree("fixtures/terrain-trees-v2", 0)?,
        "palm_tree_2.spvox",
    )?;
    let willow2 = add_asset(
        &mut model,
        load_tree("fixtures/terrain-trees-v2", 1)?,
        "weeping_willow_2.spvox",
    )?;
    let ground_id = add_asset(&mut model, ground(), "scene_voxels.spvox")?;
    let mut terrain = model.scene.new_entity(SCENE_VOXELS_NAME);
    terrain.voxel_asset = Some(ground_id);
    model.scene.entities.insert(terrain.id, terrain);

    // Jittered 9.3 m grid kept ~4 m inside the ground edge (crowns reach ~3 m), leaving a clearing at the middle of the 40 m square
    // for the player to spawn in. Types cycle so all four appear throughout. The tree
    // count is bounded by the engine's exact terrain-collider budget (4096 greedy
    // boxes for the whole terrain, `spall_physics::MERGED_CUBOID_PRIMITIVE_BUDGET`):
    // leafy trees fragment heavily, and ~33 of them exceed it.
    let kinds = [
        (palm, "Palm"),
        (willow, "Willow"),
        (palm2, "Palm 2"),
        (willow2, "Willow 2"),
    ];
    let mut n = 0usize;
    for gz in 0..4 {
        for gx in 0..4 {
            let x = 6.0 + gx as f32 * 9.3 + (hash(gx, gz, 10) - 0.5) * 4.0;
            let z = 6.0 + gz as f32 * 9.3 + (hash(gx, gz, 11) - 0.5) * 4.0;
            if (x - 20.0).hypot(z - 20.0) < 5.0 {
                continue;
            }
            // Snap to the 0.25 m cell grid so trunks align with the ground.
            let (x, z) = ((x * 4.0).round() / 4.0, (z * 4.0).round() / 4.0);
            let (asset, name) = kinds[(n + (hash(gx, gz, 12) * 2.0) as usize) % 4];
            let yaw = (hash(gx, gz, 13) * 4.0).floor() * 90.0;
            place(
                &mut model,
                &format!("{name} {n}"),
                asset,
                [x, GROUND_TOP_M, z],
                yaw,
            );
            n += 1;
        }
    }

    model.save_all()?;
    let reopened = EditorModel::load(&out)?;
    assert_eq!(reopened.voxel_assets.len(), 5);
    println!("wrote {} with {n} trees", out.display());
    Ok(())
}
