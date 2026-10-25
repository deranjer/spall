//! Regenerate the hand-held item models: cargo run -p spall_editor --example items
//!
//! Writes `fixtures/items/assets/*.spvox` (or the directory given as the first
//! argument). Every asset uses the finest cell the format allows, code 1
//! (6.25 cm), with +Y up and its origin at a corner of its bounding box.
//! Colours follow `examples/sandbox/assets/items/make_items.py`.
use spall_editor::{AssetId, EditorCommand, EditorModel, VoxelAssetFile, VoxelCoord};
use std::path::PathBuf;

const WOOD: u16 = 1;
const STONE: u16 = 2;

const BARK: [u8; 3] = [112, 76, 44];
const BARK_DARK: [u8; 3] = [78, 52, 30];
const CUT: [u8; 3] = [214, 178, 122];
const RING: [u8; 3] = [160, 120, 72];
const TWIG: [u8; 3] = [128, 90, 54];
const TWIG_LIGHT: [u8; 3] = [156, 114, 70];
const ROCK: [u8; 3] = [128, 128, 132];
const ROCK_LIGHT: [u8; 3] = [166, 166, 170];
const ROCK_DARK: [u8; 3] = [100, 100, 104];
const STEEL: [u8; 3] = [150, 154, 160];
const STEEL_DARK: [u8; 3] = [96, 98, 104];
const BRASS: [u8; 3] = [176, 142, 64];
const PAPER: [u8; 3] = [232, 222, 190];
const INK: [u8; 3] = [70, 56, 44];

fn asset(name: &str) -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(0), name);
    a.cell_size_code = 1;
    a.material_keys.insert(WOOD, "wood.oak".into());
    a.material_keys.insert(STONE, "stone.granite".into());
    a
}
fn put(a: &mut VoxelAssetFile, [x, y, z]: [i32; 3], material: u16, color: [u8; 3]) {
    let p = VoxelCoord { x, y, z };
    a.voxels.insert(p, material);
    a.colors.insert(p, color);
}
fn fill(a: &mut VoxelAssetFile, lo: [i32; 3], hi: [i32; 3], material: u16, color: [u8; 3]) {
    for z in lo[2]..=hi[2] {
        for y in lo[1]..=hi[1] {
            for x in lo[0]..=hi[0] {
                put(a, [x, y, z], material, color);
            }
        }
    }
}

/// Upright, 1.0 m: wooden shaft with a T grip and a steel blade at the bottom.
fn shovel() -> VoxelAssetFile {
    let mut a = asset("Shovel");
    fill(&mut a, [2, 6, 0], [2, 15, 0], WOOD, TWIG);
    fill(&mut a, [0, 15, 0], [4, 15, 0], WOOD, TWIG_LIGHT);
    fill(&mut a, [2, 5, 0], [2, 5, 0], STONE, STEEL_DARK);
    for (y, lo, hi) in [(4, 0, 4), (3, 0, 4), (2, 0, 4), (1, 1, 3), (0, 2, 2)] {
        fill(&mut a, [lo, y, 0], [hi, y, 0], STONE, STEEL);
    }
    // Turned-up rim steps on the back face.
    fill(&mut a, [0, 3, 1], [0, 4, 1], STONE, STEEL_DARK);
    fill(&mut a, [4, 3, 1], [4, 4, 1], STONE, STEEL_DARK);
    a
}

/// Upright, 0.8 m: wooden haft, steel head with the edge toward +Z.
fn axe() -> VoxelAssetFile {
    let mut a = asset("Axe");
    fill(&mut a, [1, 0, 1], [1, 11, 1], WOOD, TWIG);
    fill(&mut a, [1, 0, 1], [1, 0, 1], WOOD, TWIG_LIGHT);
    // Head: poll behind the haft, cheeks flaring to a wide edge at the front.
    for x in 0..=2 {
        fill(&mut a, [x, 9, 0], [x, 12, 0], STONE, STEEL_DARK);
        fill(&mut a, [x, 8, 1], [x, 12, 1], STONE, STEEL);
        fill(&mut a, [x, 8, 2], [x, 13, 2], STONE, STEEL);
        fill(&mut a, [x, 7, 3], [x, 13, 3], STONE, STEEL);
    }
    fill(&mut a, [1, 7, 4], [1, 13, 4], STONE, [200, 204, 210]);
    a
}

/// Flat disc case 7 x 2 x 7 cells (44 cm: the finest format cell is 6.25 cm, so
/// a real compass needs the item system to scale it down). The needle is
/// `compass_needle`.
fn compass() -> VoxelAssetFile {
    let mut a = asset("Compass");
    for z in 0..7i32 {
        for x in 0..7i32 {
            let r2 = (x - 3) * (x - 3) + (z - 3) * (z - 3);
            if r2 <= 10 {
                put(&mut a, [x, 0, z], STONE, BRASS);
                if r2 <= 5 {
                    put(&mut a, [x, 1, z], WOOD, PAPER);
                } else {
                    put(&mut a, [x, 1, z], STONE, BRASS);
                }
            }
        }
    }
    // North tick (red) at -Z and the other cardinal ticks.
    put(&mut a, [3, 1, 1], WOOD, [170, 40, 34]);
    for [x, z] in [[3, 5], [1, 3], [5, 3]] {
        put(&mut a, [x, 1, z], WOOD, INK);
    }
    // The pin the needle turns on.
    put(&mut a, [3, 1, 3], STONE, STEEL_DARK);
    a
}

/// 1 x 1 x 5 needle, red half toward -Z, pale half toward +Z. Its local origin
/// is compass cell (3, 2, 1) and its pivot is the centre of the middle cell, so
/// rotating it about +Y at the compass centre swings it over the card.
fn compass_needle() -> VoxelAssetFile {
    let mut a = asset("Compass needle");
    for z in 0..5 {
        let color = match z {
            0..=1 => [190, 40, 34],
            2 => STEEL_DARK,
            _ => [226, 224, 216],
        };
        put(&mut a, [0, 0, z], STONE, color);
    }
    a.pivot_subcells = [128, 128, 2 * 256 + 128];
    a
}

/// Flat sheet 4 x 1 x 5 cells (25 x 31 cm) with ruled ink lines and a clipped corner.
fn paper_note() -> VoxelAssetFile {
    let mut a = asset("Paper note");
    for z in 0..5 {
        for x in 0..4 {
            if x == 0 && z == 4 {
                continue;
            }
            let ink = (z == 1 || z == 3) && (1..3).contains(&x);
            put(&mut a, [x, 0, z], WOOD, if ink { INK } else { PAPER });
        }
    }
    a
}

/// 10 x 4 x 1 (62 cm): a crooked branch with a side twig and a pale cut end.
fn stick() -> VoxelAssetFile {
    let mut a = asset("Stick");
    let bend = [0, 0, 0, 1, 1, 1, 1, 0, 0, 0];
    for (x, y) in bend.iter().enumerate() {
        let color = match x {
            0 => CUT,
            _ if x % 3 == 0 => TWIG_LIGHT,
            _ => TWIG,
        };
        put(&mut a, [x as i32, *y, 0], WOOD, color);
    }
    put(&mut a, [6, 2, 0], WOOD, TWIG);
    put(&mut a, [7, 3, 0], WOOD, TWIG);
    a
}

/// 7 x 4 x 4 (44 x 25 x 25 cm): bark cylinder along X with pale cut ends.
fn log() -> VoxelAssetFile {
    let mut a = asset("Log");
    for x in 0..7 {
        for y in 0..4i32 {
            for z in 0..4i32 {
                // Squared distance from the axis, in half cells.
                let d = (y * 2 - 3) * (y * 2 - 3) + (z * 2 - 3) * (z * 2 - 3);
                if d > 10 {
                    continue;
                }
                let color = if x == 0 || x == 6 {
                    if d <= 2 { RING } else { CUT }
                } else if d == 10 && (x + y + z) % 3 == 0 {
                    BARK_DARK
                } else {
                    BARK
                };
                put(&mut a, [x, y, z], WOOD, color);
            }
        }
    }
    a
}

/// A rough lump 4 x 3 x 4 cells (25 x 19 x 25 cm) with a lit top.
fn stone() -> VoxelAssetFile {
    let mut a = asset("Stone");
    for z in 0..4i32 {
        for y in 0..3i32 {
            for x in 0..4i32 {
                let dx = (x as f32 - 1.5) / 2.2;
                let dy = (y as f32 - 0.8) / 1.7;
                let dz = (z as f32 - 1.5) / 2.2;
                if dx * dx + dy * dy + dz * dz > 1.0 || (x == 3 && z == 3) {
                    continue;
                }
                let color = if y == 2 {
                    ROCK_LIGHT
                } else if (x * 7 + y * 3 + z * 5) % 5 == 0 {
                    ROCK_DARK
                } else {
                    ROCK
                };
                put(&mut a, [x, y, z], STONE, color);
            }
        }
    }
    a
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "fixtures/items/assets".into());
    std::fs::create_dir_all(&out)?;
    // A scratch model only encodes the assets; no project is written.
    let scratch = std::env::temp_dir().join("spall-items-scratch");
    let mut model = EditorModel::new(&scratch, "Items");
    for (file, mut a) in [
        ("shovel", shovel()),
        ("axe", axe()),
        ("compass", compass()),
        ("compass_needle", compass_needle()),
        ("paper_note", paper_note()),
        ("stick", stick()),
        ("log", log()),
        ("stone", stone()),
    ] {
        let EditorCommand::CreateVoxelAsset { record, .. } =
            model.new_voxel_asset_command(a.name.clone())
        else {
            unreachable!()
        };
        a.id = record.id;
        let id = record.id;
        EditorCommand::CreateVoxelAsset { record, asset: a }.apply(&mut model)?;
        let path = out.join(format!("{file}.spvox"));
        model.export_spvox_asset(id, &path)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}
