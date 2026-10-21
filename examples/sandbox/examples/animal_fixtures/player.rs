use super::*;

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
pub(super) fn player() -> Animal {
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
