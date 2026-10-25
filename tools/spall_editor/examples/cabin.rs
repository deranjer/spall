//! Regenerate the playable cabin: cargo run -p spall_editor --example cabin
use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};
use std::path::PathBuf;

fn asset(name: &str) -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(0), name);
    a.cell_size_code = 0;
    for (id, key) in [
        (1, "stone.granite"),
        (2, "wood.oak"),
        (3, "slate"),
        (4, "grass"),
        (5, "dirt"),
        (6, "emissive.lamp"),
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
fn cabin() -> VoxelAssetFile {
    let mut a = asset("Timber cabin");
    // 8 x 10 m shell; continuous stone footings and plank floor, including porch.
    fill(&mut a, [0, 0, -8], [31, 0, 39], 1, [104, 108, 112]);
    for x in 0..32 {
        fill(
            &mut a,
            [x, 1, -8],
            [x, 1, 39],
            2,
            if x % 4 == 0 {
                [112, 74, 42]
            } else {
                [158, 112, 64]
            },
        );
    }
    for y in 2..14 {
        let color = if y % 2 == 0 {
            [118, 76, 40]
        } else {
            [144, 96, 52]
        };
        for z in 0..40 {
            for x in 0..32 {
                let wall = x == 0 || x == 31 || z == 0 || z == 39;
                let door = z == 0 && (13..19).contains(&x) && y < 12;
                let window = (6..10).contains(&y)
                    && (((x == 0 || x == 31) && ((8..14).contains(&z) || (25..31).contains(&z)))
                        || (z == 39 && (12..20).contains(&x)));
                if wall && !door && !window {
                    fill(&mut a, [x, y, z], [x, y, z], 2, color);
                }
            }
        }
    }
    // Filled gables and overlapping slate courses, with a half-metre overhang.
    for x in -2i32..34 {
        let roof_y = 14 + (15 - x).min(x - 16).abs();
        let roof_y = 22 - (roof_y - 14) / 2;
        for z in [0, 39] {
            if (0..32).contains(&x) {
                fill(&mut a, [x, 14, z], [x, roof_y - 1, z], 2, [118, 76, 40]);
            }
        }
        fill(
            &mut a,
            [x, roof_y, -2],
            [x, roof_y + 1, 41],
            3,
            [63, 73, 84],
        );
    }
    // Porch posts and a flat canopy connected to the front wall.
    for x in [1, 30] {
        fill(&mut a, [x, 2, -7], [x, 12, -7], 2, [90, 60, 34]);
    }
    fill(&mut a, [0, 13, -8], [31, 14, -1], 3, [63, 73, 84]);
    // Broad steps to the lawn, clear of the doorway.
    fill(&mut a, [11, 0, -12], [20, 0, -9], 1, [104, 108, 112]);
    // Stone hearth and chimney against the rear wall, with an unlit recess.
    fill(&mut a, [22, 2, 35], [29, 2, 38], 1, [104, 108, 112]);
    for x in [23, 28] {
        fill(&mut a, [x, 3, 36], [x, 8, 38], 1, [104, 108, 112]);
    }
    fill(&mut a, [23, 9, 36], [28, 10, 38], 1, [104, 108, 112]);
    fill(&mut a, [25, 11, 37], [27, 24, 39], 1, [104, 108, 112]);
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
        .unwrap_or_else(|| "fixtures/cabin".into());
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Timber Cabin");
    model.project.material_mapping = asset("mapping")
        .material_keys
        .into_iter()
        .map(|(id, key)| (key, id))
        .collect();
    let mut ground = asset("Scene Voxels");
    fill(&mut ground, [0, 0, 0], [79, 0, 95], 5, [104, 78, 52]);
    fill(&mut ground, [0, 1, 0], [79, 1, 95], 4, [78, 128, 52]);
    add(&mut model, ground, "ground.spvox", [0.0; 3])?;
    add(&mut model, cabin(), "cabin.spvox", [6.0, 0.5, 6.0])?;
    // Each loose furnishing has a local origin and an independent scene placement.
    let mut bed = asset("Bed platform");
    fill(&mut bed, [0, 0, 0], [6, 1, 9], 2, [158, 112, 64]);
    fill(&mut bed, [0, 2, 8], [6, 4, 9], 2, [90, 60, 34]);
    add(&mut model, bed, "bed.spvox", [6.75, 1.0, 12.75])?;
    let mut table = asset("Timber table");
    for x in [0, 4] {
        for z in [0, 6] {
            fill(&mut table, [x, 0, z], [x, 2, z], 2, [90, 60, 34]);
        }
    }
    fill(&mut table, [0, 3, 0], [4, 3, 6], 2, [158, 112, 64]);
    add(&mut model, table, "table.spvox", [12.0, 1.0, 12.75])?;
    let mut lamp = asset("Table lamp");
    fill(&mut lamp, [0, 0, 0], [0, 0, 0], 6, [255, 194, 110]);
    add(&mut model, lamp, "lamp.spvox", [12.5, 2.0, 13.5])?;
    let mut bench = asset("Timber bench");
    fill(&mut bench, [0, 0, 0], [1, 1, 9], 2, [118, 76, 40]);
    add(&mut model, bench, "bench.spvox", [6.75, 1.0, 8.0])?;
    let mut door = asset("Hinged timber door");
    for y in 0..10 {
        for x in 0..6 {
            door.voxels.insert(
                VoxelCoord { x, y, z: 0 },
                if x == 0 && y == 0 { 8 } else { 9 },
            );
        }
    }
    // Wooden knob a metre up on the side away from the hinge, one cell proud of
    // each face. Panel-material cells beside the leaf turn with it.
    for z in [-1, 1] {
        door.voxels.insert(VoxelCoord { x: 4, y: 4, z }, 9);
    }
    add(&mut model, door, "door.spvox", [9.25, 1.0, 6.0])?;
    let mut starts = asset("Interior player starts");
    for (x, z) in [(16, 11), (12, 20), (20, 20), (16, 24)] {
        starts.voxels.insert(VoxelCoord { x, y: 2, z }, 7);
    }
    add(&mut model, starts, "starts.spvox", [6.0, 0.5, 6.0])?;
    model.save_all()?;
    EditorModel::load(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}
