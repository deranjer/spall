//! Second-pass palm and willow. Written to their own project so the originals in
//! `fixtures/terrain-trees` stay untouched. Deliberately asymmetric: leaning
//! trunks, unevenly spaced fronds/limbs, and hash-driven variation.
type CellTuple = (i32, i32, i32);

use std::f32::consts::TAU;
use std::path::PathBuf;

use spall_editor::{AssetId, EditorCommand, EditorModel, Transform, VoxelAssetFile, VoxelCoord};

const WOOD: u16 = 2;
const FOLIAGE: u16 = 3;

/// Deterministic value in [0, 1) from a seed and a few integers.
fn hash(seed: u32, a: i32, b: i32, c: i32) -> f32 {
    let mut h = seed as u64 ^ 0x9E37_79B9_7F4A_7C15;
    for v in [a, b, c] {
        h = (h ^ (v as u32 as u64)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 31;
    }
    ((h >> 11) as f32) / ((1u64 << 53) as f32)
}

fn range(seed: u32, i: i32, lo: f32, hi: f32) -> f32 {
    lo + (hi - lo) * hash(seed, i, 0, 0)
}

fn put(asset: &mut VoxelAssetFile, x: i32, y: i32, z: i32, material: u16, tint: [u8; 3]) {
    let cell = VoxelCoord { x, y, z };
    asset.voxels.insert(cell, material);
    asset.colors.insert(cell, tint);
}

/// Foliage never replaces wood.
fn put_leaf(asset: &mut VoxelAssetFile, x: i32, y: i32, z: i32, tint: [u8; 3]) {
    if y < 0 || asset.voxels.get(&VoxelCoord { x, y, z }) == Some(&WOOD) {
        return;
    }
    put(asset, x, y, z, FOLIAGE, tint);
}

fn has(asset: &VoxelAssetFile, x: i32, y: i32, z: i32) -> bool {
    asset.voxels.contains_key(&VoxelCoord { x, y, z })
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

fn shade(base: [u8; 3], d: i32) -> [u8; 3] {
    base.map(|c| (c as i32 + d).clamp(0, 255) as u8)
}

fn disc(asset: &mut VoxelAssetFile, cx: i32, y: i32, cz: i32, r: f32, tint: [u8; 3]) {
    let n = r.ceil() as i32;
    for dz in -n..=n {
        for dx in -n..=n {
            if (dx * dx + dz * dz) as f32 <= r * r {
                put(asset, cx + dx, y, cz + dz, WOOD, tint);
            }
        }
    }
}

fn palm() -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(1), "Terrain Palm Tree 2");
    a.portable_id
        .copy_from_slice(&blake3::hash(b"spall.fixture.terrain-palm.v2").as_bytes()[..16]);
    a.cell_size_code = 0;
    a.tags.insert("fixture".into(), "terrain-generation".into());
    a.tags.insert("species".into(), "palm".into());

    const H: i32 = 19;
    // Trunk: leans toward +x with a gentle S-curve in z, flared at the base,
    // with faint growth rings.
    let center = |y: i32| {
        let t = y as f32 / H as f32;
        let x = 5.5 * t * t + 0.8 * t;
        let z = 1.6 * (t * 2.6).sin() - 0.6 * t;
        (x.round() as i32, z.round() as i32)
    };
    for y in 0..H {
        let (cx, cz) = center(y);
        let r = if y < 2 {
            1.5
        } else if y < 9 {
            1.05
        } else {
            0.6
        };
        let ring = if y % 3 == 0 { -14 } else { 0 };
        let jitter = (hash(7, y, 0, 0) * 8.0) as i32;
        disc(&mut a, cx, y, cz, r, shade([136, 106, 66], ring + jitter));
    }
    let (cx, cz) = center(H - 1);
    let top = H;

    // Crown: irregular azimuths, varied lengths, some short upright young fronds.
    let old = 9;
    for i in 0..(old + 3) {
        let young = i >= old;
        let seed = 100 + i as u32;
        let azimuth = if young {
            range(seed, 1, 0.0, TAU)
        } else {
            i as f32 * TAU / old as f32 + range(seed, 1, -0.4, 0.4)
        };
        let len = if young {
            range(seed, 2, 3.0, 4.5)
        } else {
            range(seed, 2, 6.5, 10.0)
        };
        let rise = if young {
            range(seed, 3, 1.6, 2.2)
        } else {
            range(seed, 3, 0.5, 1.2)
        };
        let tip_drop = if young {
            -1.5
        } else {
            range(seed, 4, 1.5, 5.5)
        };
        let k = (rise * len + tip_drop) / (len * len);
        let curl = range(seed, 5, -0.5, 0.5);
        let tone = range(seed, 6, -10.0, 12.0) as i32;

        let mut t = 0.0_f32;
        while t <= len {
            let f = t / len;
            let az = azimuth + curl * f;
            let x = cx + (az.cos() * t).round() as i32;
            let z = cz + (az.sin() * t).round() as i32;
            let y = top + (rise * t - k * t * t).round() as i32;
            put_leaf(&mut a, x, y, z, shade([70, 116, 48], tone));
            // Leaflets on both sides, longest mid-frond, drooping. Each side
            // is rolled separately so fronds are ragged rather than mirrored.
            if t >= 1.5 && (t * 2.0) as i32 % 2 == 0 {
                let reach = ((1.0 - (f - 0.35).abs() * 1.3) * 3.0).round() as i32;
                for (side, sign) in [(0, 1.0_f32), (1, -1.0)] {
                    let side_reach = reach
                        + if hash(seed, (t * 2.0) as i32, side, 9) < 0.35 {
                            -1
                        } else {
                            0
                        };
                    for s in 1..=side_reach.max(0) {
                        let sf = s as f32 * sign;
                        let lx = x + (-az.sin() * sf).round() as i32;
                        let lz = z + (az.cos() * sf).round() as i32;
                        let ly = y - (s as f32 * 0.9).round() as i32;
                        let bright = if s == side_reach { 14 } else { 0 };
                        put_leaf(&mut a, lx, ly, lz, shade([88, 138, 58], tone + bright));
                    }
                }
            }
            t += 0.4;
        }
    }
    // Coconut cluster under the crown, off-center.
    for (dx, dy, dz) in [(1, -1, 1), (-1, -1, 0), (0, -2, -1), (1, -2, 0)] {
        put(&mut a, cx + dx, top + dy, cz + dz, WOOD, [96, 74, 40]);
    }
    a
}

fn willow() -> VoxelAssetFile {
    let mut a = VoxelAssetFile::new(AssetId(2), "Terrain Weeping Willow 2");
    a.portable_id
        .copy_from_slice(&blake3::hash(b"spall.fixture.weeping-willow.v2").as_bytes()[..16]);
    a.cell_size_code = 0;
    a.tags.insert("fixture".into(), "terrain-generation".into());
    a.tags.insert("species".into(), "weeping-willow".into());

    const FORK: i32 = 9;
    let bark = [104, 76, 50];
    // Thick, leaning trunk with a flared base, up to the fork.
    let trunk = |y: i32| {
        let t = y as f32 / FORK as f32;
        (
            (1.8 * (t * 1.7).sin()).round() as i32,
            (-1.4 * t).round() as i32,
        )
    };
    for y in 0..FORK {
        let (cx, cz) = trunk(y);
        let r = if y < 2 {
            2.3
        } else if y < 6 {
            1.6
        } else {
            1.2
        };
        disc(
            &mut a,
            cx,
            y,
            cz,
            r,
            shade(bark, (hash(3, y, 0, 0) * 12.0) as i32 - 6),
        );
    }
    let (fx, fz) = trunk(FORK - 1);

    // Uneven limbs and crown lobes. Each limb ends in a leafy lobe of its own size.
    struct Lobe {
        x: f32,
        y: f32,
        z: f32,
        r: f32,
    }
    let mut lobes: Vec<Lobe> = Vec::new();
    let limbs = [
        (0.3_f32, 5.5_f32, 6.5_f32),
        (2.2, 4.0, 5.5),
        (3.9, 6.0, 4.8),
        (5.3, 3.0, 5.0),
    ];
    for (i, (az, reach, rise)) in limbs.into_iter().enumerate() {
        let seed = 200 + i as u32;
        let az = az + range(seed, 1, -0.3, 0.3);
        let reach = reach * range(seed, 2, 0.85, 1.25);
        let mut t = 0.0_f32;
        while t <= 1.0 {
            let x = fx as f32 + az.cos() * reach * t * t.sqrt();
            let z = fz as f32 + az.sin() * reach * t * t.sqrt();
            let y = FORK as f32 + rise * t;
            put(
                &mut a,
                x.round() as i32,
                y.round() as i32,
                z.round() as i32,
                WOOD,
                shade(bark, 6),
            );
            t += 0.04;
        }
        let ex = fx as f32 + az.cos() * reach;
        let ez = fz as f32 + az.sin() * reach;
        let ey = FORK as f32 + rise;
        lobes.push(Lobe {
            x: ex,
            y: ey,
            z: ez,
            r: range(seed, 3, 3.2, 5.0),
        });
        // A smaller lobe part-way along the limb fills in the crown unevenly.
        if hash(seed, 4, 0, 0) > 0.3 {
            lobes.push(Lobe {
                x: fx as f32 + (ex - fx as f32) * 0.55,
                y: FORK as f32 + rise * 0.8,
                z: fz as f32 + (ez - fz as f32) * 0.55,
                r: range(seed, 5, 2.4, 3.4),
            });
        }
    }
    let mut crown: Vec<(i32, i32, i32)> = Vec::new();
    for (li, lobe) in lobes.iter().enumerate() {
        let ry = lobe.r * 0.6;
        let n = lobe.r.ceil() as i32 + 1;
        for dy in -n..=n {
            for dz in -n..=n {
                for dx in -n..=n {
                    let (x, y, z) = (
                        lobe.x.round() as i32 + dx,
                        lobe.y.round() as i32 + dy,
                        lobe.z.round() as i32 + dz,
                    );
                    let d = (dx * dx + dz * dz) as f32 / (lobe.r * lobe.r)
                        + (dy * dy) as f32 / (ry * ry);
                    if d < 1.0 - 0.45 * hash(11, x, y, z) {
                        let g = hash(12 + li as u32, x, y, z);
                        let tint = if g < 0.25 {
                            [126, 156, 72]
                        } else if g < 0.7 {
                            [98, 136, 58]
                        } else {
                            [84, 122, 52]
                        };
                        put_leaf(&mut a, x, y, z, tint);
                        crown.push((x, y, z));
                    }
                }
            }
        }
    }

    // Hanging strands from the crown underside: uneven length, own sway phase,
    // drifting slightly downwind (+x).
    let mut roots: Vec<(i32, i32, i32)> = crown
        .iter()
        .copied()
        .filter(|&(x, y, z)| !has(&a, x, y - 1, z) && (x - fx).abs() + (z - fz).abs() > 2)
        .collect();
    roots.sort();
    roots.dedup();
    for (x, y, z) in roots {
        let roll = hash(21, x, y, z);
        if roll > 0.6 {
            continue;
        }
        let dist = (((x - fx).pow(2) + (z - fz).pow(2)) as f32).sqrt();
        // Outer strands hang longest; a few are stubby.
        let full = (y - 1) as f32;
        let frac = (0.45 + 0.1 * dist + hash(22, x, z, 0) * 0.4).min(1.0)
            * if hash(23, x, z, 1) < 0.15 { 0.4 } else { 1.0 };
        let bottom = (y as f32 - full * frac).round() as i32;
        let phase = hash(24, x, z, 2) * TAU;
        let amp = 0.5 + hash(25, x, z, 3) * 0.9;
        for sy in (bottom.max(1)..y).rev() {
            let depth = (y - sy) as f32;
            let sx = x + (amp * (depth * 0.45 + phase).sin() + depth * 0.05).round() as i32;
            let sz = z + (amp * 0.6 * (depth * 0.37 + phase * 1.7).cos()).round() as i32;
            let tip = (sy - bottom.max(1)) <= 1;
            let tint = if tip {
                [150, 174, 92]
            } else if hash(26, sx, sy, sz) < 0.3 {
                [112, 146, 66]
            } else {
                [90, 128, 54]
            };
            put_leaf(&mut a, sx, sy, sz, tint);
        }
    }
    a
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/terrain-trees-v2"));
    std::fs::create_dir_all(&out)?;
    let mut model = EditorModel::new(&out, "Terrain Trees 2");
    save_asset(&mut model, palm(), "palm_tree_2.spvox", [0.0, 0.0, 0.0])?;
    save_asset(
        &mut model,
        willow(),
        "weeping_willow_2.spvox",
        [8.0, 0.0, 0.0],
    )?;
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
