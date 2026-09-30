type CellTuple = (i32, i32, i32);

use std::path::PathBuf;

use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};

fn put(asset: &mut VoxelAssetFile, x: i32, y: i32, z: i32, material: u16, tint: [u8; 3]) {
    let cell = VoxelCoord { x, y, z };
    asset.voxels.insert(cell, material);
    asset.colors.insert(cell, tint);
}

/// Ensures every voxel in `asset` is six-face-connected back to the trunk
/// base at (0, 0, 0). The engine's structural connectivity graph treats
/// every solid voxel identically regardless of material (`docs/architecture.md`:
/// "use six-face connectivity"; corner/edge contact does not create a bond),
/// so a canopy authored with diagonal-only gaps between its voxels becomes
/// dozens of genuinely disconnected "unsupported" fragments. That is not an
/// engine bug, but it is a scene/content defect: a world placing many of
/// these trees can accumulate thousands of pre-existing disconnected
/// fragments that the very first terrain edit anywhere in that world then
/// tries to detach all at once (see the valley-showcase giant-split
/// rejection this was written to fix). This bridges each disconnected
/// cluster to the trunk's component with a short straight run of its own
/// nearest voxel's material/tint, rather than deleting any authored voxel.
fn connect_foliage_to_trunk(asset: &mut VoxelAssetFile) {
    use std::collections::{HashSet, VecDeque};
    const ROOT: (i32, i32, i32) = (0, 0, 0);
    const NEIGHBOURS: [(i32, i32, i32); 6] = [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];
    loop {
        let cells: HashSet<(i32, i32, i32)> =
            asset.voxels.keys().map(|c| (c.x, c.y, c.z)).collect();
        if !cells.contains(&ROOT) {
            return; // nothing to anchor to; the trunk base is always placed
        }
        let mut visited = HashSet::from([ROOT]);
        let mut queue = VecDeque::from([ROOT]);
        while let Some((x, y, z)) = queue.pop_front() {
            for (dx, dy, dz) in NEIGHBOURS {
                let n = (x + dx, y + dy, z + dz);
                if cells.contains(&n) && visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }
        let mut nearest: Option<(CellTuple, CellTuple, i32)> = None;
        for &d in cells.difference(&visited) {
            for &v in &visited {
                let dist = (d.0 - v.0).abs() + (d.1 - v.1).abs() + (d.2 - v.2).abs();
                if nearest.is_none_or(|(_, _, best)| dist < best) {
                    nearest = Some((d, v, dist));
                }
            }
        }
        let Some((from, to, _)) = nearest else {
            return; // every voxel reaches the trunk
        };
        let material = asset.voxels[&VoxelCoord {
            x: from.0,
            y: from.1,
            z: from.2,
        }];
        let tint = asset
            .colors
            .get(&VoxelCoord {
                x: from.0,
                y: from.1,
                z: from.2,
            })
            .copied();
        // Walk one axis at a time from `from` toward `to`: only one
        // coordinate changes per step, so every step is face-adjacent to
        // the last.
        let (mut x, mut y, mut z) = from;
        while (x, y, z) != to {
            if x != to.0 {
                x += (to.0 - x).signum();
            } else if y != to.1 {
                y += (to.1 - y).signum();
            } else {
                z += (to.2 - z).signum();
            }
            let cell = VoxelCoord { x, y, z };
            if let std::collections::btree_map::Entry::Vacant(entry) = asset.voxels.entry(cell) {
                entry.insert(material);
                if let Some(tint) = tint {
                    asset.colors.insert(cell, tint);
                }
            }
        }
    }
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
    mut asset: VoxelAssetFile,
    file_name: &str,
    position: [f32; 3],
) -> Result<(), Box<dyn std::error::Error>> {
    connect_foliage_to_trunk(&mut asset);
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
