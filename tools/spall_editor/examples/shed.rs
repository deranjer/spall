//! Regenerate the woodshed: cargo run -p spall_editor --example shed
//!
//! A 3 x 4 m plank shed with a pitched roof, a hinged door (the cabin's door
//! spec: 6 x 10 cells, hinge at its lower-left cell), a woodpile, and a named
//! `axe_spawn` marker entity in the scene marking where the axe pickup spawns.
use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};
use std::path::PathBuf;

/// Where the shed sits in this fixture's scene, in metres.
const SHED_AT: [f32; 3] = [6.0, 0.5, 6.0];
/// The axe's rest point relative to the shed asset origin: the top face centre
/// of the chopping block (cells x 8..=9, y 2..=3, z 11..=12).
const AXE_OFFSET: [f32; 3] = [2.25, 1.0, 3.0];

fn asset(name: &str) -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(0), name);
    a.cell_size_code = 0;
    for (id, key) in [
        (1, "stone.granite"),
        (2, "wood.oak"),
        (4, "grass"),
        (5, "dirt"),
        (7, "player.spawn"),
        (8, "door.hinge.closed"),
        (9, "door.panel"),
    ] {
        a.material_keys.insert(id, key.into());
    }
    a
}
fn fill(a: &mut VoxelAssetFile, lo: [i32; 3], hi: [i32; 3], material: u16, color: [u8; 3]) {
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                let p = VoxelCoord { x, y, z };
                a.voxels.insert(p, material);
                a.colors.insert(p, color);
            }
        }
    }
}
fn shed() -> VoxelAssetFile {
    let mut a = asset("Woodshed");
    // 12 x 16 cells (3 x 4 m). Stone footing and plank floor, with a one metre
    // deck in front of the door and a low stone step below it.
    fill(&mut a, [0, 0, -4], [11, 0, 15], 1, [104, 108, 112]);
    fill(&mut a, [3, 0, -8], [8, 0, -5], 1, [104, 108, 112]);
    for x in 0..12 {
        let color = if x % 2 == 0 {
            [158, 112, 64]
        } else {
            [140, 98, 56]
        };
        fill(&mut a, [x, 1, -4], [x, 1, 15], 2, color);
    }
    // Vertical board walls, 3 m high. The front doorway (x 3..=8, y 2..=11)
    // holds the door; a high slot window lights the back wall.
    for y in 2..14 {
        for z in 0..16 {
            for x in 0..12 {
                let wall = x == 0 || x == 11 || z == 0 || z == 15;
                let doorway = z == 0 && (3..9).contains(&x) && y < 12;
                let slot = z == 15 && (4..8).contains(&x) && (9..11).contains(&y);
                if wall && !doorway && !slot {
                    let color = if x % 2 == 0 {
                        [132, 90, 50]
                    } else {
                        [112, 76, 42]
                    };
                    fill(&mut a, [x, y, z], [x, y, z], 2, color);
                }
            }
        }
    }
    // Filled gables and a two-thick pitched roof with a half-metre overhang.
    for x in -1i32..13 {
        let d = x.clamp(0, 11).min(11 - x.clamp(0, 11));
        let roof_y = 14 + d / 2;
        if (0..12).contains(&x) {
            for z in [0, 15] {
                if roof_y > 14 {
                    fill(&mut a, [x, 14, z], [x, roof_y - 1, z], 2, [112, 76, 42]);
                }
            }
        }
        let color = if x % 2 == 0 {
            [84, 62, 42]
        } else {
            [96, 70, 46]
        };
        fill(&mut a, [x, roof_y, -2], [x, roof_y + 1, 17], 2, color);
    }
    // Woodpile against the left wall: split logs, pale cut ends facing the room.
    for z in 6..15 {
        for y in 2..6 {
            for x in 1..5 {
                let end = z == 6;
                let color = match (end, (x + y) % 2) {
                    (true, 0) => [214, 178, 122],
                    (true, _) => [196, 158, 104],
                    (false, 0) => [112, 76, 44],
                    (false, _) => [128, 90, 54],
                };
                fill(&mut a, [x, y, z], [x, y, z], 2, color);
            }
        }
    }
    fill(&mut a, [1, 6, 10], [3, 6, 14], 2, [128, 90, 54]);
    // Chopping block in the back-right corner: the axe rests on top of it.
    fill(&mut a, [8, 2, 11], [9, 3, 12], 2, [214, 178, 122]);
    a
}
fn add(
    model: &mut EditorModel,
    mut a: VoxelAssetFile,
    file: &str,
    at: [f32; 3],
) -> Result<(), Box<dyn std::error::Error>> {
    let EditorCommand::CreateVoxelAsset { mut record, .. } =
        model.new_voxel_asset_command(a.name.clone())
    else {
        unreachable!()
    };
    record.storage = PathBuf::from("assets").join(file);
    a.id = record.id;
    let mut e = model.scene.new_entity(a.name.clone());
    e.voxel_asset = Some(record.id);
    e.transform = Transform {
        translation: at,
        ..Default::default()
    };
    EditorCommand::CreateVoxelAsset { record, asset: a }.apply(model)?;
    model.scene.entities.insert(e.id, e);
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "fixtures/shed".into());
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Woodshed");
    model.project.material_mapping = asset("mapping")
        .material_keys
        .into_iter()
        .map(|(id, key)| (key, id))
        .collect();
    let mut ground = asset("Scene Voxels");
    fill(&mut ground, [0, 0, 0], [79, 0, 95], 5, [104, 78, 52]);
    fill(&mut ground, [0, 1, 0], [79, 1, 95], 4, [78, 128, 52]);
    add(&mut model, ground, "ground.spvox", [0.0; 3])?;
    add(&mut model, shed(), "shed.spvox", SHED_AT)?;
    // The cabin's door, unchanged: lower-left hinge cell, 6 x 10 cells, opening
    // from +X toward +Z. It sits in the doorway at x 3, y 2 of the shed.
    let mut door = asset("Hinged timber door");
    for y in 0..10 {
        for x in 0..6 {
            door.voxels.insert(
                VoxelCoord { x, y, z: 0 },
                if x == 0 && y == 0 { 8 } else { 9 },
            );
        }
    }
    add(
        &mut model,
        door,
        "door.spvox",
        [SHED_AT[0] + 0.75, SHED_AT[1] + 0.5, SHED_AT[2]],
    )?;
    let mut starts = asset("Interior player start");
    starts.voxels.insert(VoxelCoord { x: 6, y: 2, z: 8 }, 7);
    add(&mut model, starts, "starts.spvox", SHED_AT)?;
    // A named marker with no voxels: the world-space rest point for the axe pickup.
    let mut marker = model.scene.new_entity("axe_spawn");
    marker.transform = Transform {
        translation: [0, 1, 2].map(|i| SHED_AT[i] + AXE_OFFSET[i]),
        ..Default::default()
    };
    model.scene.entities.insert(marker.id, marker);
    model.save_all()?;
    EditorModel::load(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}
