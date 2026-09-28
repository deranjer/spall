use std::path::PathBuf;

use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};

fn put(asset: &mut VoxelAssetFile, x: i32, y: i32, z: i32, material: u16, tint: [u8; 3]) {
    let cell = VoxelCoord { x, y, z };
    asset.voxels.insert(cell, material);
    asset.colors.insert(cell, tint);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/terrain-trees"));
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Terrain Trees");

    let mut palm = VoxelAssetFile::new(AssetId(1), "Terrain Palm Tree");
    palm.portable_id
        .copy_from_slice(&blake3::hash(b"spall.fixture.terrain-palm.v1").as_bytes()[..16]);
    palm.cell_size_code = 0;
    palm.tags
        .insert("fixture".into(), "terrain-generation".into());
    palm.tags.insert("species".into(), "palm".into());
    // A slim, slightly bending trunk and eight long, downward-curving fronds.
    for y in 0..14 {
        let x = if y > 8 { (y - 8) / 4 } else { 0 };
        put(&mut palm, x, y, 0, 2, [132, 102, 62]);
    }
    for (dx, dz) in [
        (-1_i32, -1_i32),
        (-1, 0),
        (-1, 1),
        (0, -1),
        (0, 1),
        (1, -1),
        (1, 0),
        (1, 1),
    ] {
        for d in 1_i32..=7 {
            let x = 1 + dx * d;
            let z = dz * d;
            let y = 14 - (d * d / 9);
            let tint = if (d + dx + dz).rem_euclid(3) == 0 {
                [117, 155, 65]
            } else {
                [83, 133, 56]
            };
            put(&mut palm, x, y, z, 3, tint);
            // Paired leaflets give each frond a segmented, feathered profile.
            if dx == 0 {
                put(&mut palm, x + 1, y, z, 3, tint);
            }
            if dz == 0 {
                put(&mut palm, x, y, z + 1, 3, tint);
            }
        }
    }
    save_asset(&mut model, palm, "palm_tree.spvox", [0.0, 0.0, 0.0])?;

    let mut willow = VoxelAssetFile::new(AssetId(2), "Terrain Weeping Willow");
    willow
        .portable_id
        .copy_from_slice(&blake3::hash(b"spall.fixture.weeping-willow.v1").as_bytes()[..16]);
    willow.cell_size_code = 0;
    willow
        .tags
        .insert("fixture".into(), "terrain-generation".into());
    willow
        .tags
        .insert("species".into(), "weeping-willow".into());
    // Tall central trunk, broad crown, and hanging leafy curtains to y=0.
    for y in 0..14 {
        put(&mut willow, 0, y, 0, 2, [112, 79, 51]);
    }
    for y in 10..16 {
        let radius: i32 = if y < 12 {
            2
        } else if y < 15 {
            5
        } else {
            4
        };
        for z in -radius..=radius {
            for x in -radius..=radius {
                if x * x + z * z <= radius * radius + 2 && ((x * 5 + z * 3 + y) % 8 != 0) {
                    let tint = if (x - z + y) % 5 == 0 {
                        [124, 157, 69]
                    } else {
                        [94, 133, 57]
                    };
                    put(&mut willow, x, y, z, 3, tint);
                }
            }
        }
    }
    for (x, z) in [
        (-4_i32, -3_i32),
        (-4, 0),
        (-4, 3),
        (-2, -5),
        (-2, 5),
        (0, -5),
        (0, 5),
        (2, -5),
        (2, 5),
        (4, -3),
        (4, 0),
        (4, 3),
    ] {
        let bottom = if (x + z) % 3 == 0 { 1 } else { 2 };
        for y in bottom..=12 {
            let dx = (y + x * 2 + z).rem_euclid(3) - 1;
            put(
                &mut willow,
                x + dx,
                y,
                z,
                3,
                if y % 4 == 0 {
                    [119, 151, 66]
                } else {
                    [91, 129, 54]
                },
            );
        }
    }
    save_asset(&mut model, willow, "weeping_willow.spvox", [8.0, 0.0, 0.0])?;
    model.save_all()?;
    let reopened = EditorModel::load(&out)?;
    assert_eq!(reopened.voxel_assets.len(), 2);
    assert_eq!(reopened.scene.entities.len(), 2);
    Ok(())
}

fn save_asset(
    model: &mut EditorModel,
    asset: VoxelAssetFile,
    file_name: &str,
    position: [f32; 3],
) -> Result<(), Box<dyn std::error::Error>> {
    let command = model.new_voxel_asset_command(asset.name.clone());
    let EditorCommand::CreateVoxelAsset { mut record, .. } = command else {
        unreachable!()
    };
    record.storage = PathBuf::from("assets").join(file_name);
    let asset_id = record.id;
    let command = EditorCommand::CreateVoxelAsset { record, asset };
    command.apply(model)?;
    let mut entity = model.scene.new_entity(file_name.trim_end_matches(".spvox"));
    entity.voxel_asset = Some(asset_id);
    entity.transform = Transform {
        translation: position,
        ..Transform::default()
    };
    model.scene.entities.insert(entity.id, entity);
    Ok(())
}
