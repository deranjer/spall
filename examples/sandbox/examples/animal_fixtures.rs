//! Deterministic authoring source for the sandbox's small articulated SPVX fixtures:
//! the wildlife under `assets/animals` and the player body under `assets/characters`.
//! Run with `cargo run -p sandbox --example animal_fixtures -- --write`.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
};

const OUT: &str = "examples/sandbox/assets/animals";
const CHARACTERS: &str = "examples/sandbox/assets/characters";
const CELL_SIZE_CM: u8 = 1; // SPVX code 1 = 6.25 cm.
const PALETTE: [[u8; 3]; 5] = [
    [76, 129, 170],
    [205, 179, 123],
    [35, 38, 44],
    [120, 91, 55],
    [224, 220, 191],
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Voxel {
    x: i32,
    y: i32,
    z: i32,
    material: &'static str,
    tint: u16,
}
#[derive(Clone, Debug)]
struct Part {
    id: u32,
    parent: u32,
    name: &'static str,
    at: [i32; 3],
    voxels: Vec<Voxel>,
}
#[derive(Clone, Copy)]
struct Key {
    tick: u32,
    interpolation: u8,
    fields: u8,
    translation: Option<[i32; 3]>,
    rotation: Option<[i16; 4]>,
    scale: Option<[u16; 3]>,
    visibility: Option<u8>,
}
struct Track {
    part: u32,
    keys: Vec<Key>,
}
struct Clip {
    name: &'static str,
    duration: u32,
    mode: u8,
    tracks: Vec<Track>,
}
struct Animal {
    name: &'static str,
    /// Repository-relative output directory.
    dir: &'static str,
    palette: &'static [[u8; 3]],
    /// Non-idle clips and the exact part ids each one animates.
    actions: Vec<(&'static str, Vec<u32>)>,
    uuid: [u8; 16],
    parts: Vec<Part>,
    clips: Vec<Clip>,
    extent: [i32; 6],
}
type DecodedChunk = ([u8; 4], Vec<u8>);

fn part(id: u32, parent: u32, name: &'static str, at: [i32; 3]) -> Part {
    Part {
        id,
        parent,
        name,
        at,
        voxels: Vec::new(),
    }
}
fn voxel(p: &mut Part, x: i32, y: i32, z: i32, m: &'static str, t: u16) {
    p.voxels.push(Voxel {
        x,
        y,
        z,
        material: m,
        tint: t,
    });
}
fn cuboid(p: &mut Part, bounds: [i32; 6], m: &'static str, t: u16) {
    for z in bounds[4]..=bounds[5] {
        for y in bounds[2]..=bounds[3] {
            for x in bounds[0]..=bounds[1] {
                voxel(p, x, y, z, m, t);
            }
        }
    }
}
fn q(axis: usize, degrees: f64) -> [i16; 4] {
    let a = degrees.to_radians() / 2.0;
    let mut q = [0i16; 4];
    q[axis] = (a.sin() * 32767.0).round() as i16;
    q[3] = (a.cos() * 32767.0).round() as i16;
    q
}
fn key(tick: u32, rotation: Option<[i16; 4]>, translation: Option<[i32; 3]>) -> Key {
    let mut fields = 0;
    if translation.is_some() {
        fields |= 1
    }
    if rotation.is_some() {
        fields |= 2
    }
    Key {
        tick,
        interpolation: if rotation.is_some() { 2 } else { 1 },
        fields,
        translation,
        rotation,
        scale: None,
        visibility: None,
    }
}
fn rot_track(id: u32, axis: usize, angles: &[(u32, f64)]) -> Track {
    Track {
        part: id,
        keys: angles
            .iter()
            .map(|&(t, a)| key(t, Some(q(axis, a)), None))
            .collect(),
    }
}
fn translate_track(id: u32, vals: &[(u32, [i32; 3])]) -> Track {
    Track {
        part: id,
        keys: vals.iter().map(|&(t, v)| key(t, None, Some(v))).collect(),
    }
}
fn anim(name: &'static str, duration: u32, tracks: Vec<Track>) -> Clip {
    Clip {
        name,
        duration,
        mode: 1,
        tracks,
    }
}

fn trout() -> Animal {
    let mut body = part(1, 0, "body", [0, 0, 0]);
    for x in -4i32..=6 {
        let (w, h) = match x {
            -4 | 5 | 6 => (0, 0),
            -3 | -2 | 3 | 4 => (1, 1),
            _ => (1, 2),
        };
        for z in -w..=w {
            for y in -h..=h {
                let tint = if x == 4 && y == 1 && z != 0 {
                    3 // Contrasting eyes on both sides of the head.
                } else if y == -h && x.abs() < 3 {
                    2
                } else {
                    1
                };
                voxel(&mut body, x, y, z, "stone", tint);
            }
        }
    }
    let mut tail = part(2, 1, "tail_peduncle_and_fork", [-4, 0, 0]);
    cuboid(&mut tail, [-2, 0, 0, 0, -1, 1], "stone", 1);
    for y in -2i32..=2 {
        voxel(&mut tail, -3, y, 0, "moss", 3);
        if y.abs() >= 1 {
            voxel(&mut tail, -4, y, 0, "moss", 3);
        }
    }
    let mut dorsal = part(3, 1, "dorsal_fin", [-1, 2, 0]);
    for x in -1i32..=1 {
        for y in 0..=2 - x.abs() {
            voxel(&mut dorsal, x, y, 0, "moss", 3);
        }
    }
    let mut pectoral = part(4, 1, "pectoral_fin_near", [1, 0, 1]);
    for z in 0..=2 {
        for x in -1..=1 - z {
            voxel(&mut pectoral, x, -1, z, "moss", 3);
        }
    }
    let mut far = part(5, 1, "pectoral_fin_far", [1, 0, -1]);
    for z in 0..=2 {
        for x in -1..=1 - z {
            voxel(&mut far, x, -1, -z, "moss", 3);
        }
    }
    let idle = anim(
        "idle",
        120,
        vec![translate_track(
            1,
            &[
                (0, [0, 0, 0]),
                (30, [0, 24, 0]),
                (60, [0, 0, 0]),
                (90, [0, -24, 0]),
                (120, [0, 0, 0]),
            ],
        )],
    );
    let swim = anim(
        "swim",
        60,
        vec![rot_track(
            2,
            1,
            &[(0, 0.0), (15, 22.0), (30, 0.0), (45, -22.0), (60, 0.0)],
        )],
    );
    Animal {
        name: "Trout",
        dir: OUT,
        palette: &PALETTE,
        actions: vec![("swim", vec![2])],
        uuid: [
            0x5a, 0x87, 0x10, 0x01, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ],
        parts: vec![body, tail, dorsal, pectoral, far],
        clips: vec![idle, swim],
        extent: [-8, 6, -2, 4, -3, 3],
    }
}
fn crow() -> Animal {
    let mut body = part(1, 0, "torso", [0, 0, 0]);
    cuboid(&mut body, [-3, 3, -1, 1, -1, 1], "stone", 3);
    let mut head = part(2, 1, "head", [3, 1, 0]);
    for x in 0..=2 {
        for y in 0..=2 {
            for z in -1i32..=1 {
                if !(x == 1 && y == 1 && z.abs() == 1) {
                    voxel(&mut head, x, y, z, "stone", 3)
                }
            }
        }
    }
    cuboid(&mut head, [3, 4, 0, 0, 0, 0], "wood", 3);
    voxel(&mut head, 1, 1, 1, "snow", 5);
    voxel(&mut head, 1, 1, -1, "snow", 5);
    let mut near = part(3, 1, "wing_near", [0, 1, 1]);
    for x in -2..=2 {
        for z in 0..=5 {
            let y = if z >= 4 { -(z - 3) } else { 1 };
            voxel(&mut near, x, y, z, "stone", 3);
        }
    }
    let mut far = part(4, 1, "wing_far", [0, 1, -1]);
    for x in -2..=2 {
        for z in -5..=0 {
            let y = if z <= -4 { z + 3 } else { 1 };
            voxel(&mut far, x, y, z, "stone", 3);
        }
    }
    let mut tail = part(5, 1, "tail_fan", [-3, 0, 0]);
    cuboid(&mut tail, [-3, -1, -1, 1, -2, 2], "stone", 3);
    let mut legs = part(6, 1, "legs_and_feet", [0, -1, 0]);
    for z in [-1, 1] {
        cuboid(&mut legs, [-1, 0, -4, -2, z, z], "wood", 4);
        cuboid(&mut legs, [-2, 1, -5, -5, z, z], "wood", 4);
    }
    let idle = anim(
        "idle",
        120,
        vec![translate_track(
            1,
            &[
                (0, [0, 0, 0]),
                (30, [0, 16, 0]),
                (60, [0, 0, 0]),
                (90, [0, 16, 0]),
                (120, [0, 0, 0]),
            ],
        )],
    );
    let flap = anim(
        "flap",
        48,
        vec![
            rot_track(
                3,
                0,
                &[(0, -48.0), (12, 38.0), (24, -48.0), (36, 38.0), (48, -48.0)],
            ),
            rot_track(
                4,
                0,
                &[(0, 48.0), (12, -38.0), (24, 48.0), (36, -38.0), (48, 48.0)],
            ),
        ],
    );
    Animal {
        name: "Crow",
        dir: OUT,
        palette: &PALETTE,
        actions: vec![("flap", vec![3, 4])],
        uuid: [
            0x5a, 0x87, 0x10, 0x02, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02,
        ],
        parts: vec![body, head, near, far, tail, legs],
        clips: vec![idle, flap],
        extent: [-6, 7, -6, 3, -6, 6],
    }
}
fn deer() -> Animal {
    let mut body = part(1, 0, "torso", [0, 0, 0]);
    for x in -4i32..=4 {
        let half = if x.abs() < 3 { 2 } else { 1 };
        cuboid(&mut body, [x, x, -1, 2, -half, half], "wood", 4);
    }
    let mut neck = part(2, 1, "neck", [3, 1, 0]);
    cuboid(&mut neck, [0, 1, 0, 2, -1, 1], "wood", 4);
    let mut head = part(3, 2, "head", [1, 2, 0]);
    for x in 0..=2 {
        for y in 0..=2 {
            for z in -1i32..=1 {
                if !(x == 0 && y == 1 && z.abs() == 1) {
                    voxel(&mut head, x, y, z, "wood", 4)
                }
            }
        }
    }
    cuboid(&mut head, [3, 4, 0, 0, -1, 1], "dirt", 4);
    voxel(&mut head, 0, 1, 1, "snow", 5);
    voxel(&mut head, 0, 1, -1, "snow", 5);
    let mut antlers = part(4, 3, "branched_antlers", [1, 2, 0]);
    for side in [-1, 1] {
        for y in 0..=3 {
            voxel(&mut antlers, 0, y, side, "snow", 5);
        }
        for x in 1..=2 {
            voxel(&mut antlers, x, 2, side, "snow", 5);
            if x < 2 {
                voxel(&mut antlers, x, 3, side, "snow", 5);
            }
        }
        voxel(&mut antlers, 1, 4, side, "snow", 5);
    }
    let mut legs = Vec::new();
    for (id, name, x, z) in [
        (5, "foreleg_near", 2, 2),
        (6, "foreleg_far", 2, -2),
        (7, "hindleg_near", -3, 2),
        (8, "hindleg_far", -3, -2),
    ] {
        let mut leg = part(id, 1, name, [x, -1, z]);
        cuboid(&mut leg, [-1, 0, -5, -1, 0, 0], "wood", 4);
        cuboid(&mut leg, [-1, 1, -6, -6, 0, 0], "dirt", 3);
        legs.push(leg);
    }
    let mut tail = part(9, 1, "tail", [-4, 1, 0]);
    cuboid(&mut tail, [-1, 0, -1, 0, -1, 1], "wood", 4);
    cuboid(&mut tail, [-1, 0, -2, -2, -1, 1], "snow", 5);
    let idle = anim(
        "idle",
        120,
        vec![rot_track(
            3,
            1,
            &[(0, -5.0), (30, 0.0), (60, 5.0), (90, 0.0), (120, -5.0)],
        )],
    );
    let walk = anim(
        "walk",
        48,
        (5..=8)
            .map(|id| {
                let phase = if id == 5 || id == 8 { 1.0 } else { -1.0 };
                rot_track(
                    id,
                    2,
                    &[
                        (0, -20.0 * phase),
                        (12, 0.0),
                        (24, 20.0 * phase),
                        (36, 0.0),
                        (48, -20.0 * phase),
                    ],
                )
            })
            .collect(),
    );
    Animal {
        name: "Deer",
        dir: OUT,
        palette: &PALETTE,
        actions: vec![("walk", vec![5, 6, 7, 8])],
        uuid: [
            0x5a, 0x87, 0x10, 0x03, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x03,
        ],
        parts: [vec![body, neck, head, antlers], legs, vec![tail]].concat(),
        clips: vec![idle, walk],
        extent: [-5, 8, -7, 9, -2, 2],
    }
}

/// Player body palette: skin, shirt, trousers, shoes, hair, eyes, belt.
const PLAYER_PALETTE: [[u8; 3]; 7] = [
    [222, 170, 132],
    [56, 96, 150],
    [60, 66, 82],
    [38, 34, 32],
    [86, 58, 38],
    [28, 28, 36],
    [104, 76, 48],
];
const SKIN: u16 = 1;
const SHIRT: u16 = 2;
const TROUSERS: u16 = 3;
const SHOES: u16 = 4;
const HAIR: u16 = 5;
const EYES: u16 = 6;
const BELT: u16 = 7;

/// Rotation keys `(tick, degrees, translation)` about `axis`, each also
/// setting the part's translation (subcells; 256 = one cell).
fn rot_trans_track(id: u32, axis: usize, keys: &[(u32, f64, [i32; 3])]) -> Track {
    Track {
        part: id,
        keys: keys
            .iter()
            .map(|&(t, a, v)| key(t, Some(q(axis, a)), Some(v)))
            .collect(),
    }
}

/// A 28-cell (1.75 m) humanoid facing +X with +Y up and +Z to its left,
/// matching the 1.8 m player capsule. The pelvis is the root; its pivot is
/// the hip joint, 13 cells above the shoe soles. Limbs hinge about Z, where a
/// positive angle swings a hanging limb forward.
fn player() -> Animal {
    let mut pelvis = part(1, 0, "pelvis", [0, 0, 0]);
    cuboid(&mut pelvis, [-2, 1, 0, 0, -3, 2], "moss", TROUSERS);
    cuboid(&mut pelvis, [-2, 1, 1, 1, -3, 2], "wood", BELT);

    let mut torso = part(2, 1, "torso", [0, 2, 0]);
    cuboid(&mut torso, [-2, 1, 0, 3, -3, 2], "moss", SHIRT);
    cuboid(&mut torso, [-2, 1, 4, 7, -4, 3], "moss", SHIRT);

    let mut head = part(3, 2, "head", [0, 8, 0]);
    cuboid(&mut head, [-1, 0, 0, 0, -1, 0], "clay", SKIN);
    for y in 1..=4 {
        for x in -2..=1 {
            for z in -2i32..=1 {
                let (material, tint) = if y == 4 || (x == -2 && y >= 2) || (x <= -1 && y == 3) {
                    ("wood", HAIR)
                } else if x == 1 && y == 2 && (z == -2 || z == 1) {
                    ("stone", EYES)
                } else {
                    ("clay", SKIN)
                };
                voxel(&mut head, x, y, z, material, tint);
            }
        }
    }

    let mut limbs = Vec::new();
    for (id, name, side) in [(4, "upper_arm_left", 5), (5, "upper_arm_right", -5)] {
        let mut arm = part(id, 2, name, [0, 8, side]);
        cuboid(&mut arm, [-1, 0, -3, -1, -1, 0], "moss", SHIRT);
        cuboid(&mut arm, [-1, 0, -6, -4, -1, 0], "clay", SKIN);
        limbs.push(arm);
    }
    for (id, parent, name) in [(6, 4, "forearm_left"), (7, 5, "forearm_right")] {
        let mut forearm = part(id, parent, name, [0, -6, 0]);
        cuboid(&mut forearm, [-1, 0, -5, -1, -1, 0], "clay", SKIN);
        limbs.push(forearm);
    }
    for (id, name, z) in [(8, "thigh_left", [0, 2]), (9, "thigh_right", [-3, -1])] {
        let mut thigh = part(id, 1, name, [0, 0, 0]);
        cuboid(&mut thigh, [-1, 1, -6, -1, z[0], z[1]], "moss", TROUSERS);
        limbs.push(thigh);
    }
    for (id, parent, name, z) in [
        (10, 8, "shin_left", [0, 2]),
        (11, 9, "shin_right", [-3, -1]),
    ] {
        let mut shin = part(id, parent, name, [0, -6, 0]);
        cuboid(&mut shin, [-1, 1, -6, -1, z[0], z[1]], "moss", TROUSERS);
        cuboid(&mut shin, [-1, 2, -7, -7, z[0], z[1]], "wood", SHOES);
        limbs.push(shin);
    }

    // Idle: slow breathing sway of the torso and arms.
    let idle = anim(
        "idle",
        120,
        vec![
            rot_track(2, 2, &[(0, 0.0), (60, -1.5), (120, 0.0)]),
            rot_track(4, 2, &[(0, 2.0), (60, -1.0), (120, 2.0)]),
            rot_track(5, 2, &[(0, -2.0), (60, 1.0), (120, -2.0)]),
        ],
    );

    // Walk: one 1 s stride cycle. Arms counter-swing the legs; the swing knee
    // bends while that thigh comes forward; the hips dip at double support.
    let low = [0, -256, 0];
    let high = [0, 0, 0];
    let walk = anim(
        "walk",
        60,
        vec![
            translate_track(1, &[(0, low), (15, high), (30, low), (45, high), (60, low)]),
            rot_track(
                4,
                2,
                &[(0, -20.0), (15, 0.0), (30, 20.0), (45, 0.0), (60, -20.0)],
            ),
            rot_track(
                5,
                2,
                &[(0, 20.0), (15, 0.0), (30, -20.0), (45, 0.0), (60, 20.0)],
            ),
            rot_track(6, 2, &[(0, 10.0), (30, 20.0), (60, 10.0)]),
            rot_track(7, 2, &[(0, 20.0), (30, 10.0), (60, 20.0)]),
            rot_track(
                8,
                2,
                &[(0, 25.0), (15, 0.0), (30, -25.0), (45, 0.0), (60, 25.0)],
            ),
            rot_track(
                9,
                2,
                &[(0, -25.0), (15, 0.0), (30, 25.0), (45, 0.0), (60, -25.0)],
            ),
            rot_track(
                10,
                2,
                &[(0, -5.0), (15, -10.0), (30, -15.0), (45, -45.0), (60, -5.0)],
            ),
            rot_track(
                11,
                2,
                &[
                    (0, -15.0),
                    (15, -45.0),
                    (30, -5.0),
                    (45, -10.0),
                    (60, -15.0),
                ],
            ),
        ],
    );

    // Run (for Shift): a faster 0.6 s cycle with bigger swings, a forward
    // torso lean, bent elbows and a high knee tuck.
    let r_low = [0, -576, 0];
    let r_high = [0, -128, 0];
    let run = anim(
        "run",
        36,
        vec![
            translate_track(
                1,
                &[
                    (0, r_low),
                    (9, r_high),
                    (18, r_low),
                    (27, r_high),
                    (36, r_low),
                ],
            ),
            rot_track(
                2,
                2,
                &[
                    (0, -10.0),
                    (9, -12.0),
                    (18, -10.0),
                    (27, -12.0),
                    (36, -10.0),
                ],
            ),
            rot_track(
                4,
                2,
                &[(0, -40.0), (9, 0.0), (18, 40.0), (27, 0.0), (36, -40.0)],
            ),
            rot_track(
                5,
                2,
                &[(0, 40.0), (9, 0.0), (18, -40.0), (27, 0.0), (36, 40.0)],
            ),
            rot_track(6, 2, &[(0, 90.0), (18, 70.0), (36, 90.0)]),
            rot_track(7, 2, &[(0, 70.0), (18, 90.0), (36, 70.0)]),
            rot_track(
                8,
                2,
                &[(0, 40.0), (9, 0.0), (18, -35.0), (27, 0.0), (36, 40.0)],
            ),
            rot_track(
                9,
                2,
                &[(0, -35.0), (9, 0.0), (18, 40.0), (27, 0.0), (36, -35.0)],
            ),
            rot_track(
                10,
                2,
                &[
                    (0, -20.0),
                    (9, -15.0),
                    (18, -30.0),
                    (27, -90.0),
                    (36, -20.0),
                ],
            ),
            rot_track(
                11,
                2,
                &[
                    (0, -30.0),
                    (9, -90.0),
                    (18, -20.0),
                    (27, -15.0),
                    (36, -30.0),
                ],
            ),
        ],
    );

    // Swim: the body lies prone (head along +X, face down) and bobs. Each arm
    // runs a front crawl half a cycle apart: recover above the water, enter
    // past the head, pull down under the body to the hip. The legs flutter-kick.
    let swim = anim(
        "swim",
        60,
        vec![
            rot_trans_track(
                1,
                2,
                &[
                    (0, -90.0, [0, 0, 0]),
                    (30, -90.0, [0, 48, 0]),
                    (60, -90.0, [0, 0, 0]),
                ],
            ),
            rot_track(
                4,
                2,
                &[
                    (0, 0.0),
                    (15, -90.0),
                    (30, -180.0),
                    (45, -270.0),
                    (60, -360.0),
                ],
            ),
            rot_track(
                5,
                2,
                &[
                    (0, -180.0),
                    (15, -270.0),
                    (30, -360.0),
                    (45, -450.0),
                    (60, -540.0),
                ],
            ),
            rot_track(6, 2, &[(0, 10.0), (30, 40.0), (60, 10.0)]),
            rot_track(7, 2, &[(0, 40.0), (30, 10.0), (60, 40.0)]),
            rot_track(
                8,
                2,
                &[(0, 12.0), (15, -12.0), (30, 12.0), (45, -12.0), (60, 12.0)],
            ),
            rot_track(
                9,
                2,
                &[(0, -12.0), (15, 12.0), (30, -12.0), (45, 12.0), (60, -12.0)],
            ),
            rot_track(
                10,
                2,
                &[(0, -5.0), (15, -20.0), (30, -5.0), (45, -20.0), (60, -5.0)],
            ),
            rot_track(
                11,
                2,
                &[(0, -20.0), (15, -5.0), (30, -20.0), (45, -5.0), (60, -20.0)],
            ),
        ],
    );

    let limb_ids = vec![4, 5, 6, 7, 8, 9, 10, 11];
    Animal {
        name: "Player",
        dir: CHARACTERS,
        palette: &PLAYER_PALETTE,
        actions: vec![
            ("walk", [vec![1], limb_ids.clone()].concat()),
            ("run", [vec![1, 2], limb_ids.clone()].concat()),
            ("swim", [vec![1], limb_ids].concat()),
        ],
        uuid: [
            0x5a, 0x87, 0x20, 0x01, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ],
        parts: [vec![pelvis, torso, head], limbs].concat(),
        clips: vec![idle, walk, run, swim],
        extent: [-2, 2, -13, 14, -6, 5],
    }
}

fn put_u16(v: &mut Vec<u8>, n: u16) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn put_i32(v: &mut Vec<u8>, n: i32) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn str8(v: &mut Vec<u8>, s: &str) {
    v.push(s.len() as u8);
    v.extend_from_slice(s.as_bytes())
}
fn str16(v: &mut Vec<u8>, s: &str) {
    put_u16(v, s.len() as u16);
    v.extend_from_slice(s.as_bytes())
}
fn quat_identity() -> [i16; 4] {
    [0, 0, 0, 32767]
}
fn meta(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&a.uuid);
    b.extend([CELL_SIZE_CM, 0]);
    for v in [0i32, 0, 0] {
        put_i32(&mut b, v)
    }
    str16(&mut b, a.name);
    b.extend([0; 16]);
    put_u16(&mut b, 0);
    b
}
fn part_chunk(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, a.parts.len() as u16);
    for p in &a.parts {
        put_u32(&mut b, p.id);
        put_u32(&mut b, p.parent);
        str8(&mut b, p.name);
        for n in p.at {
            put_i32(&mut b, n * 256)
        }
        for n in quat_identity() {
            b.extend_from_slice(&n.to_le_bytes())
        }
        for _ in 0..3 {
            put_u16(&mut b, 1024)
        }
        put_u16(&mut b, 0)
    }
    b
}
fn voxels(a: &Animal, slots: &BTreeMap<&str, u16>) -> Vec<u8> {
    let mut rows = Vec::new();
    for p in &a.parts {
        let mut cells = p.voxels.clone();
        cells.sort_by_key(|c| (c.z, c.y, c.x, c.material, c.tint));
        let mut i = 0;
        while i < cells.len() {
            let c = cells[i];
            let mut end = i + 1;
            while end < cells.len()
                && cells[end].z == c.z
                && cells[end].y == c.y
                && cells[end].x == cells[end - 1].x + 1
                && cells[end].material == c.material
                && cells[end].tint == c.tint
            {
                end += 1
            }
            rows.push((
                p.id,
                c.x,
                c.y,
                c.z,
                (end - i) as u32,
                slots[c.material],
                c.tint,
            ));
            i = end;
        }
    }
    rows.sort_by_key(|r| (r.0, r.3, r.2, r.1));
    let mut b = vec![0];
    put_u32(&mut b, rows.len() as u32);
    for (part, x, y, z, len, mat, tint) in rows {
        put_u32(&mut b, part);
        put_i32(&mut b, x);
        put_i32(&mut b, y);
        put_i32(&mut b, z);
        put_u32(&mut b, len);
        put_u16(&mut b, mat);
        put_u16(&mut b, tint);
    }
    b
}
fn anim_chunk(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, a.clips.len() as u16);
    for c in &a.clips {
        str8(&mut b, c.name);
        put_u32(&mut b, c.duration);
        b.push(c.mode);
        put_u16(&mut b, c.tracks.len() as u16);
        for t in &c.tracks {
            put_u32(&mut b, t.part);
            put_u16(&mut b, t.keys.len() as u16);
            for k in &t.keys {
                put_u32(&mut b, k.tick);
                b.push(k.interpolation);
                b.push(k.fields);
                if let Some(v) = k.translation {
                    for n in v {
                        put_i32(&mut b, n)
                    }
                }
                if let Some(v) = k.rotation {
                    for n in v {
                        b.extend_from_slice(&n.to_le_bytes())
                    }
                }
                if let Some(v) = k.scale {
                    for n in v {
                        put_u16(&mut b, n)
                    }
                }
                if let Some(v) = k.visibility {
                    b.push(v)
                }
            }
        }
        put_u16(&mut b, 0)
    }
    b
}
fn chunk(id: &[u8; 4], payload: &[u8], out: &mut Vec<u8>) {
    out.extend(id);
    out.extend([0, 0, 0, 0]);
    put_u32(out, payload.len() as u32);
    put_u32(out, payload.len() as u32);
    out.extend(payload)
}
fn encode(a: &Animal) -> Vec<u8> {
    let materials: BTreeSet<&str> = a
        .parts
        .iter()
        .flat_map(|p| p.voxels.iter().map(|v| v.material))
        .collect();
    let slots: BTreeMap<&str, u16> = materials
        .iter()
        .enumerate()
        .map(|(i, &m)| (m, (i + 1) as u16))
        .collect();
    let mut chunks = Vec::new();
    let mut mtrl = Vec::new();
    put_u16(&mut mtrl, materials.len() as u16);
    for m in &materials {
        str8(&mut mtrl, m)
    }
    let mut palt = Vec::new();
    put_u16(&mut palt, a.palette.len() as u16);
    for rgb in a.palette {
        palt.extend(rgb)
    }
    chunks.push((*b"META", meta(a)));
    chunks.push((*b"MTRL", mtrl));
    chunks.push((*b"PALT", palt));
    chunks.push((*b"PART", part_chunk(a)));
    chunks.push((*b"VOXL", voxels(a, &slots)));
    chunks.push((*b"ANIM", anim_chunk(a)));
    let mut hash = blake3::Hasher::new();
    hash.update(b"spall.asset.v1");
    for (id, payload) in &chunks {
        hash.update(id);
        hash.update(&(payload.len() as u32).to_le_bytes());
        hash.update(payload);
    }
    let digest = hash.finalize();
    let mut out = b"SPVX\x01\x00\x01\x00\0\0\0\0".to_vec();
    for (id, payload) in &chunks {
        chunk(id, payload, &mut out)
    }
    chunk(b"HASH", digest.as_bytes(), &mut out);
    out
}

fn validate(a: &Animal, bytes: &[u8]) -> Result<(), String> {
    if a.uuid.iter().all(|&v| v == 0) {
        return Err(format!("{} has zero UUID", a.name));
    }
    if a.parts.len() < 4 {
        return Err(format!("{} hierarchy too small", a.name));
    }
    let ids: BTreeSet<u32> = a.parts.iter().map(|p| p.id).collect();
    if !ids.contains(&1)
        || a.parts
            .iter()
            .any(|p| p.id != 1 && (!ids.contains(&p.parent) || p.parent >= p.id))
    {
        return Err(format!("{} invalid parent order", a.name));
    }
    let occupied: usize = a.parts.iter().map(|p| p.voxels.len()).sum();
    if occupied < 35 {
        return Err(format!("{} has only {occupied} authored cells", a.name));
    }
    let names: BTreeSet<_> = a.clips.iter().map(|c| c.name).collect();
    if !names.contains("idle") {
        return Err(format!("{} lacks idle", a.name));
    }
    for (action, expected_parts) in &a.actions {
        let clip = a
            .clips
            .iter()
            .find(|c| c.name == *action)
            .ok_or("missing action clip")?;
        if clip.tracks.is_empty()
            || clip.tracks.iter().any(|t| t.keys.len() < 3)
            || clip
                .tracks
                .iter()
                .any(|t| t.keys.last().unwrap().tick != clip.duration)
        {
            return Err(format!("{} {action} clip lacks cyclic end keys", a.name));
        }
        // Every track must actually move its part: rotation or translation changes.
        if clip.tracks.iter().map(|t| t.part).collect::<Vec<_>>() != *expected_parts
            || clip.tracks.iter().any(|t| {
                t.keys.iter().all(|k| k.rotation == t.keys[0].rotation)
                    && t.keys
                        .iter()
                        .all(|k| k.translation == t.keys[0].translation)
            })
        {
            return Err(format!(
                "{} {action} does not animate the expected articulated parts",
                a.name
            ));
        }
    }
    let idle = a.clips.iter().find(|c| c.name == "idle").unwrap();
    if idle.tracks.iter().all(|t| {
        t.keys
            .iter()
            .all(|k| k.translation.is_none() && k.rotation.is_none())
    }) {
        return Err(format!("{} idle clip has no subtle motion", a.name));
    }
    if a.parts.iter().any(|p| p.voxels.is_empty()) {
        return Err(format!("{} has empty part", a.name));
    }
    let mut seen = BTreeSet::new();
    for p in &a.parts {
        for v in &p.voxels {
            if !seen.insert((p.id, v.x, v.y, v.z)) {
                return Err(format!(
                    "{} duplicate cell in {} at {},{},{}",
                    a.name, p.name, v.x, v.y, v.z
                ));
            }
        }
    }
    let mut origins = BTreeMap::new();
    let mut bounds = [i32::MAX, i32::MIN, i32::MAX, i32::MIN, i32::MAX, i32::MIN];
    for p in &a.parts {
        let parent = if p.parent == 0 {
            [0; 3]
        } else {
            *origins.get(&p.parent).ok_or("missing parent origin")?
        };
        let at = [
            parent[0] + p.at[0],
            parent[1] + p.at[1],
            parent[2] + p.at[2],
        ];
        origins.insert(p.id, at);
        for v in &p.voxels {
            let xyz = [v.x + at[0], v.y + at[1], v.z + at[2]];
            for axis in 0..3 {
                bounds[axis * 2] = bounds[axis * 2].min(xyz[axis]);
                bounds[axis * 2 + 1] = bounds[axis * 2 + 1].max(xyz[axis]);
            }
        }
    }
    if bounds != a.extent {
        return Err(format!(
            "{} bounds {:?}, expected {:?}",
            a.name, bounds, a.extent
        ));
    }
    let decoded = decode_chunks(bytes)?;
    if decoded.len() != 7
        || !decoded.iter().any(|(id, _)| id == b"PART")
        || !decoded.iter().any(|(id, _)| id == b"ANIM")
    {
        return Err("chunk set invalid".into());
    }
    Ok(())
}
fn decode_chunks(b: &[u8]) -> Result<Vec<DecodedChunk>, String> {
    if b.len() < 12 || &b[..4] != b"SPVX" {
        return Err("bad SPVX header".into());
    }
    let mut i = 12;
    let mut chunks = Vec::new();
    let mut hash = blake3::Hasher::new();
    hash.update(b"spall.asset.v1");
    while i < b.len() {
        if b.len() - i < 16 {
            return Err("short chunk header".into());
        }
        let id: [u8; 4] = b[i..i + 4].try_into().unwrap();
        let codec = b[i + 4];
        let stored = u32::from_le_bytes(b[i + 8..i + 12].try_into().unwrap()) as usize;
        let raw = u32::from_le_bytes(b[i + 12..i + 16].try_into().unwrap()) as usize;
        i += 16;
        if codec != 0 || stored != raw || i + stored > b.len() {
            return Err("bad raw chunk length/codec".into());
        }
        let payload = b[i..i + stored].to_vec();
        i += stored;
        if &id == b"HASH" {
            if payload.len() != 32
                || i != b.len()
                || payload.as_slice() != hash.finalize().as_bytes()
            {
                return Err("SPVX BLAKE3 mismatch".into());
            }
        } else {
            hash.update(&id);
            hash.update(&(raw as u32).to_le_bytes());
            hash.update(&payload);
        }
        chunks.push((id, payload));
    }
    if chunks.last().map(|x| x.0) != Some(*b"HASH") {
        return Err("missing final HASH".into());
    }
    Ok(chunks)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let check = args.iter().any(|a| a == "--check");
    let root =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?).join("../..");
    let animals = [trout(), crow(), deer(), player()];
    let mut uuids = BTreeSet::new();
    let manifest = sandbox::game::manifest();
    let manifest_names: BTreeSet<&str> = manifest
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    for a in &animals {
        if !uuids.insert(a.uuid) {
            return Err(format!("{} reuses an animal UUID", a.name));
        }
        for material in a
            .parts
            .iter()
            .flat_map(|p| p.voxels.iter().map(|v| v.material))
        {
            if !manifest_names.contains(material) {
                return Err(format!(
                    "{} references unknown sandbox material {material}",
                    a.name
                ));
            }
        }
        let first = encode(a);
        let second = encode(a);
        if first != second {
            return Err(format!("{} generation is nondeterministic", a.name));
        }
        validate(a, &first)?;
        let dir = root.join(a.dir);
        let path = dir.join(format!("{}.spvox", a.name.to_lowercase()));
        if check {
            let actual = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if actual != first {
                return Err(format!(
                    "{} differs; regenerate with --write",
                    path.display()
                ));
            }
        } else {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            fs::write(&path, &first).map_err(|e| e.to_string())?;
            println!(
                "wrote {} ({} bytes, {} cells, {} parts)",
                path.display(),
                first.len(),
                a.parts.iter().map(|p| p.voxels.len()).sum::<usize>(),
                a.parts.len()
            )
        }
    }
    if check {
        println!("animal SPVX fixtures deterministic and internally validated")
    };
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("animal fixture generator: {e}");
        std::process::exit(1)
    }
}
