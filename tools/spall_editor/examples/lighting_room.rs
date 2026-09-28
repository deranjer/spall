//! Lighting test scene: three identical stone houses on a lawn, differing only in
//! how they open to the sky -- one sealed, one with a doorway, one with no roof.
//! The player spawns at the lawn centre facing -Z, the houses' doorways face +Z.
//!
//! Writes an openable project to `fixtures/lighting-room` (or the first argument):
//! `cargo run -p spall_editor --example lighting_room`
//! Play it: `cargo xtask play --editor-scene fixtures/lighting-room`
use std::path::PathBuf;

use spall_editor::{
    AssetId, EditorCommand, EditorModel, SCENE_VOXELS_NAME, Transform, VoxelAssetFile, VoxelCoord,
};

const GRASS: u16 = 5;
const DIRT: u16 = 6;
const STONE: u16 = 1;
const ROOF: u16 = 2;
const LAMP: u16 = 7;
/// Lawn edge in 0.25 m cells: 160 cells = 40 m square.
const SIZE: i32 = 160;
const GROUND_TOP_M: f32 = 0.5;

/// House footprint in cells (6.5 m outside, 6 m inside with one-voxel walls).
const FOOTPRINT: i32 = 26;
const WALL_HEIGHT: i32 = 12;

#[derive(Clone, Copy, PartialEq)]
enum Opening {
    Sealed,
    Doorway,
    Roofless,
}

fn ground() -> VoxelAssetFile {
    let mut g = VoxelAssetFile::new(AssetId(0), SCENE_VOXELS_NAME);
    g.cell_size_code = 0;
    g.material_keys.insert(GRASS, "grass".into());
    g.material_keys.insert(DIRT, "dirt".into());
    for z in 0..SIZE {
        for x in 0..SIZE {
            let top = VoxelCoord { x, y: 1, z };
            g.voxels.insert(top, GRASS);
            g.colors.insert(top, [78, 128, 52]);
            let under = VoxelCoord { x, y: 0, z };
            g.voxels.insert(under, DIRT);
            g.colors.insert(under, [104, 78, 52]);
        }
    }
    g
}

fn house(name: &str, opening: Opening) -> VoxelAssetFile {
    let mut h = VoxelAssetFile::new(AssetId(0), name);
    h.cell_size_code = 0;
    h.material_keys.insert(STONE, "stone.granite".into());
    h.material_keys.insert(ROOF, "sandstone".into());
    let last = FOOTPRINT - 1;
    for y in 0..WALL_HEIGHT {
        for z in 0..FOOTPRINT {
            for x in 0..FOOTPRINT {
                let on_wall = x == 0 || x == last || z == 0 || z == last;
                if !on_wall {
                    continue;
                }
                // A 1.5 m x 2.5 m doorway centred in the +Z wall.
                let doorway =
                    opening == Opening::Doorway && z == last && (10..16).contains(&x) && y < 10;
                if doorway {
                    continue;
                }
                h.voxels.insert(VoxelCoord { x, y, z }, STONE);
            }
        }
    }
    if opening == Opening::Doorway {
        // A glowing lamp block (1 m cube) in the middle of the room: emissive
        // light and its bounce, in a room whose only other light is the doorway.
        h.material_keys.insert(LAMP, "emissive.lamp".into());
        for y in 0..4 {
            for z in 11..15 {
                for x in 11..15 {
                    let cell = VoxelCoord { x, y, z };
                    h.voxels.insert(cell, LAMP);
                    h.colors.insert(cell, [255, 170, 60]);
                }
            }
        }
    }
    if opening != Opening::Roofless {
        for z in 0..FOOTPRINT {
            for x in 0..FOOTPRINT {
                h.voxels.insert(
                    VoxelCoord {
                        x,
                        y: WALL_HEIGHT,
                        z,
                    },
                    ROOF,
                );
            }
        }
    }
    h
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

fn place(model: &mut EditorModel, name: &str, asset: AssetId, at: [f32; 3]) {
    let mut entity = model.scene.new_entity(name);
    entity.voxel_asset = Some(asset);
    entity.transform = Transform {
        translation: at,
        ..Transform::default()
    };
    model.scene.entities.insert(entity.id, entity);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/lighting-room"));
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Lighting Room");
    model.project.material_mapping.insert("grass".into(), GRASS);
    model.project.material_mapping.insert("dirt".into(), DIRT);
    model
        .project
        .material_mapping
        .insert("stone.granite".into(), STONE);
    model
        .project
        .material_mapping
        .insert("sandstone".into(), ROOF);
    model
        .project
        .material_mapping
        .insert("emissive.lamp".into(), LAMP);
    model.scene.environment = "daylight".into();

    let ground_id = add_asset(&mut model, ground(), "scene_voxels.spvox")?;
    let mut terrain = model.scene.new_entity(SCENE_VOXELS_NAME);
    terrain.voxel_asset = Some(ground_id);
    model.scene.entities.insert(terrain.id, terrain);

    // Doorways face +Z, toward the player at the lawn centre (20, 20).
    let variants = [
        ("House (sealed)", Opening::Sealed, "house_sealed.spvox", 4.0),
        (
            "House (doorway)",
            Opening::Doorway,
            "house_doorway.spvox",
            16.75,
        ),
        (
            "House (roofless)",
            Opening::Roofless,
            "house_roofless.spvox",
            29.5,
        ),
    ];
    for (name, opening, file, x) in variants {
        let id = add_asset(&mut model, house(name, opening), file)?;
        place(&mut model, name, id, [x, GROUND_TOP_M, 6.0]);
    }

    model.save_all()?;
    let reopened = EditorModel::load(&out)?;
    assert_eq!(reopened.voxel_assets.len(), 4);
    println!("wrote {}", out.display());
    Ok(())
}
