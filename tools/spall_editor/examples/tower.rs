//! Regenerate the lookout tower: cargo run -p spall_editor --example tower
//!
//! A 9 x 9 m granite tower, 22 m to its top platform (ENG-255). Inside, six
//! sections alternate between stairs (one 0.25 m cell of rise per two
//! cells of run: a 1:1 stair is too steep for the capsule) and ladders
//! (material `ladder`, `MaterialFlags::CLIMBABLE`):
//!
//! | section | kind   | rise (m) | from -> to (m above the ground floor) |
//! |---------|--------|----------|---------------------------------------|
//! | S1      | stairs | 3        | 0 -> 3                                |
//! | L1      | ladder | 5        | 3 -> 8                                |
//! | S2      | stairs | 3        | 8 -> 11                               |
//! | L2      | ladder | 5        | 11 -> 16                              |
//! | S3      | stairs | 3        | 16 -> 19                              |
//! | L3      | ladder | 3        | 19 -> 22                              |
//!
//! Stairs always sit at the back of the tower (region A), ladders at the left
//! (region B), so each landing slab has a hole over the section below it.
//! The front door is the cabin's door spec, unchanged. A railed top platform
//! carries a beacon of emissive lamp cells and a voxel-less `win_zone` marker.
use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};
use std::path::PathBuf;

/// Where the tower sits in this fixture's scene, in metres.
const TOWER_AT: [f32; 3] = [6.0, 0.5, 6.0];
/// First empty cell above the ground-floor planks (tower-local y).
const BASE: i32 = 2;
/// Outer footprint in cells (9 m); walls are two cells thick.
const SIDE: i32 = 36;
/// Floor heights above the ground floor, in cells: ground, S1 top, L1 top,
/// S2 top, L2 top, S3 top, platform (L3 top).
const LANDINGS: [i32; 7] = [0, 12, 32, 44, 64, 76, 88];
/// Stair region A: a run along +x from `STAIR_X0`, `STAIR_Z` wide.
const STAIR_X0: i32 = 6;
/// Cells of run per step: a 1:1 staircase is too steep for the capsule.
const STAIR_TREAD: i32 = 2;
const STAIR_RUN: i32 = 12;
const STAIR_Z: (i32, i32) = (27, 32);
/// Ladder region B: a four-cell-wide column at `LADDER_X` facing +x, in a
/// slab hole spanning `LADDER_HOLE_X` by `LADDER_HOLE_Z`.
const LADDER_X: i32 = 7;
const LADDER_Z: (i32, i32) = (8, 11);
const LADDER_HOLE_X: (i32, i32) = (2, 7);
const LADDER_HOLE_Z: (i32, i32) = (7, 12);
/// Door hinge column (tower-local x); the doorway spans six cells.
const DOOR_X: i32 = 15;
/// The win zone centre on the platform, tower-local metres (x, z).
const WIN_XZ_M: [f32; 2] = [6.5, 4.5];

fn asset(name: &str) -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(0), name);
    a.cell_size_code = 0;
    for (id, key) in [
        (1, "stone.granite"),
        (2, "wood.oak"),
        (4, "grass"),
        (5, "dirt"),
        (6, "emissive.lamp"),
        (7, "player.spawn"),
        (8, "door.hinge.closed"),
        (9, "door.panel"),
        (10, "ladder"),
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
fn within(v: i32, range: (i32, i32)) -> bool {
    (range.0..=range.1).contains(&v)
}
type Hole = Option<((i32, i32), (i32, i32))>;
/// A two-cell slab whose top face is `landing` cells above the ground floor,
/// with an optional rectangular `hole` (x range, z range).
fn slab(a: &mut VoxelAssetFile, landing: i32, hole: Hole) {
    let top = BASE + landing;
    for z in 0..SIDE {
        for x in 0..SIDE {
            if hole.is_some_and(|(hx, hz)| within(x, hx) && within(z, hz)) {
                continue;
            }
            let color = if (x / 2 + z / 2) % 2 == 0 {
                [158, 112, 64]
            } else {
                [140, 98, 56]
            };
            fill(a, [x, top - 2, z], [x, top - 1, z], 2, color);
        }
    }
}
/// A solid wedge: step `i` is `i + 1` cells high; the last step of the run is
/// the upper landing itself.
fn stairs(a: &mut VoxelAssetFile, from: i32) {
    for i in 0..STAIR_RUN - 1 {
        let color = if i % 2 == 0 {
            [150, 104, 58]
        } else {
            [128, 88, 48]
        };
        fill(
            a,
            [STAIR_X0 + STAIR_TREAD * i, BASE + from, STAIR_Z.0],
            [
                STAIR_X0 + STAIR_TREAD * i + STAIR_TREAD - 1,
                BASE + from + i,
                STAIR_Z.1,
            ],
            2,
            color,
        );
    }
}
/// Rungs from the lower floor up to exactly the upper landing's top face.
fn ladder(a: &mut VoxelAssetFile, from: i32, to: i32) {
    fill(
        a,
        [LADDER_X, BASE + from, LADDER_Z.0],
        [LADDER_X, BASE + to - 1, LADDER_Z.1],
        10,
        [140, 100, 60],
    );
}
fn tower() -> VoxelAssetFile {
    let mut a = asset("Lookout tower");
    let top = BASE + LANDINGS[6];
    // Footing and ground-floor planks.
    fill(
        &mut a,
        [0, 0, 0],
        [SIDE - 1, 0, SIDE - 1],
        1,
        [104, 108, 112],
    );
    fill(
        &mut a,
        [0, 1, 0],
        [SIDE - 1, 1, SIDE - 1],
        2,
        [158, 112, 64],
    );
    // Deck and a low stone step in front of the door.
    fill(&mut a, [13, 0, -8], [22, 0, -1], 1, [104, 108, 112]);
    fill(&mut a, [13, 1, -4], [22, 1, -1], 2, [158, 112, 64]);
    // Granite walls, two cells thick, with the doorway and light slits.
    for y in BASE..top {
        let section = (0..6).find(|&k| y < BASE + LANDINGS[k + 1]).unwrap_or(5);
        let base = BASE + LANDINGS[section];
        let slit_y = (base + 5..=base + 9).contains(&y);
        for z in 0..SIDE {
            for x in 0..SIDE {
                let wall = !(2..SIDE - 2).contains(&x) || !(2..SIDE - 2).contains(&z);
                let doorway = z < 2 && (DOOR_X..DOOR_X + 6).contains(&x) && y < BASE + 10;
                let slit = slit_y
                    && ((x < 2 && within(z, (10, 11)))
                        || (x >= SIDE - 2 && within(z, (10, 11)))
                        || (z >= SIDE - 2 && within(x, (10, 11)))
                        || (z < 2 && within(x, (28, 29))));
                if wall && !doorway && !slit {
                    let color = if (x + z + y / 4) % 3 == 0 {
                        [112, 116, 120]
                    } else {
                        [98, 102, 106]
                    };
                    fill(&mut a, [x, y, z], [x, y, z], 1, color);
                }
            }
        }
    }
    // Landing slabs: each has a hole over the section below it.
    let stair_hole = Some(((2, STAIR_X0 + STAIR_TREAD * (STAIR_RUN - 1) - 1), STAIR_Z));
    let ladder_hole = Some((LADDER_HOLE_X, LADDER_HOLE_Z));
    for (k, hole) in [stair_hole, ladder_hole]
        .into_iter()
        .cycle()
        .take(6)
        .enumerate()
    {
        slab(&mut a, LANDINGS[k + 1], hole);
    }
    // Sections: stairs on even indices, ladders on odd.
    for k in 0..6 {
        if k % 2 == 0 {
            stairs(&mut a, LANDINGS[k]);
        } else {
            ladder(&mut a, LANDINGS[k], LANDINGS[k + 1]);
        }
    }
    // Railing on the platform's outer ring: posts every six cells, two rails.
    let wood = [118, 76, 40];
    for z in 0..SIDE {
        for x in 0..SIDE {
            if x != 0 && x != SIDE - 1 && z != 0 && z != SIDE - 1 {
                continue;
            }
            let post = (x % 6 == 0 || x == SIDE - 1) && (z % 6 == 0 || z == SIDE - 1);
            if post {
                fill(&mut a, [x, top, z], [x, top + 4, z], 2, wood);
            }
            fill(&mut a, [x, top + 2, z], [x, top + 2, z], 2, wood);
            fill(&mut a, [x, top + 4, z], [x, top + 4, z], 2, wood);
        }
    }
    // Beacon: a granite plinth in the middle of the platform topped by a lamp.
    fill(&mut a, [16, top, 16], [19, top + 1, 19], 1, [112, 116, 120]);
    fill(
        &mut a,
        [17, top + 2, 17],
        [18, top + 4, 18],
        6,
        [255, 194, 110],
    );
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
        .unwrap_or_else(|| "fixtures/tower".into());
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Lookout Tower");
    model.project.material_mapping = asset("mapping")
        .material_keys
        .into_iter()
        .map(|(id, key)| (key, id))
        .collect();
    let mut ground = asset("Scene Voxels");
    fill(&mut ground, [0, 0, 0], [79, 0, 95], 5, [104, 78, 52]);
    fill(&mut ground, [0, 1, 0], [79, 1, 95], 4, [78, 128, 52]);
    add(&mut model, ground, "ground.spvox", [0.0; 3])?;
    add(&mut model, tower(), "tower.spvox", TOWER_AT)?;
    // The cabin's door, unchanged: lower-left hinge cell, 6 x 10 cells, opening
    // from +X toward +Z, with its knob on both faces.
    let mut door = asset("Hinged timber door");
    for y in 0..10 {
        for x in 0..6 {
            door.voxels.insert(
                VoxelCoord { x, y, z: 0 },
                if x == 0 && y == 0 { 8 } else { 9 },
            );
        }
    }
    for z in [-1, 1] {
        door.voxels.insert(VoxelCoord { x: 4, y: 4, z }, 9);
    }
    add(
        &mut model,
        door,
        "door.spvox",
        [
            TOWER_AT[0] + DOOR_X as f32 * 0.25,
            TOWER_AT[1] + BASE as f32 * 0.25,
            TOWER_AT[2],
        ],
    )?;
    let mut starts = asset("Player start");
    starts.voxels.insert(
        VoxelCoord {
            x: 18,
            y: BASE,
            z: -3,
        },
        7,
    );
    add(&mut model, starts, "starts.spvox", TOWER_AT)?;
    // A named marker with no voxels: the centre of the win zone, on the
    // platform surface. See the fixture README for the zone's extent.
    let mut marker = model.scene.new_entity("win_zone");
    marker.transform = Transform {
        translation: [
            TOWER_AT[0] + WIN_XZ_M[0],
            TOWER_AT[1] + (BASE + LANDINGS[6]) as f32 * 0.25,
            TOWER_AT[2] + WIN_XZ_M[1],
        ],
        ..Default::default()
    };
    model.scene.entities.insert(marker.id, marker);
    model.save_all()?;
    EditorModel::load(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}
