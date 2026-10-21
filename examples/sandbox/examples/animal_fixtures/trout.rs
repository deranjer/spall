use super::*;

/// Trout palette: olive back, silver flank, pink lateral band, white belly,
/// dark spots, eyes and fins.
const TROUT_PALETTE: [[u8; 3]; 6] = [
    [92, 104, 62],
    [176, 182, 172],
    [208, 120, 128],
    [232, 230, 220],
    [34, 36, 30],
    [124, 118, 84],
];
const BACK: u16 = 1;
const SILVER: u16 = 2;
const BAND: u16 = 3;
const BELLY: u16 = 4;
const SPOT: u16 = 5;
const FIN: u16 = 6;

/// A rainbow trout about 0.65 m long, facing +X: a spindle body with a pink
/// lateral band and a spotted back, and a hinged tail that yaws about Y.
pub(super) fn trout() -> Animal {
    let skin = |[x, y, z]: [i32; 3]| {
        let tint = if y >= 1 && (x * 7 + y * 3 + z * 5).rem_euclid(5) == 0 {
            SPOT
        } else if y >= 1 {
            BACK
        } else if y == 0 && z != 0 {
            BAND
        } else if y <= -2 {
            BELLY
        } else {
            SILVER
        };
        ("stone", tint)
    };
    let mut body = part(1, 0, "body", [0, 0, 0]);
    ellipsoid(&mut body, [0.5, 0.0, 0.0], [4.0, 1.8, 1.3], &skin);
    limb(
        &mut body,
        [-2.0, 0.0, 0.0],
        [-3.5, 0.0, 0.0],
        1.2,
        0.9,
        &skin,
    );
    paint(&mut body, [3, 0, 1], ("stone", SPOT));
    paint(&mut body, [3, 0, -1], ("stone", SPOT));

    let mut tail = part(2, 1, "tail_peduncle_and_fork", [-4, 0, 0]);
    limb(
        &mut tail,
        [0.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0],
        0.9,
        0.7,
        &skin,
    );
    for x in -4..=-2 {
        for y in -3i32..=3 {
            // Forked: the fin's middle stops short of its lobes.
            if y.abs() <= 1 + (-2 - x) && !(x == -4 && y.abs() <= 1) {
                voxel(&mut tail, x, y, 0, "moss", FIN);
            }
        }
    }

    let mut dorsal = part(3, 1, "dorsal_fin", [0, 2, 0]);
    for (x, y) in [(-1, 0), (0, 0), (1, 0), (0, 1)] {
        voxel(&mut dorsal, x, y, 0, "moss", FIN);
    }
    let pectoral = |id: u32, name: &'static str, side: i32| {
        let mut fin = part(id, 1, name, [2, -1, side]);
        for (x, z) in [(-1, 0), (0, 0), (-1, 1)] {
            voxel(&mut fin, x, -1, z * side, "moss", FIN);
        }
        fin
    };
    let near = pectoral(4, "pectoral_fin_near", 1);
    let far = pectoral(5, "pectoral_fin_far", -1);

    let idle = anim(
        "idle",
        120,
        vec![
            translate_track(
                1,
                &[
                    (0, [0, 0, 0]),
                    (30, [0, 24, 0]),
                    (60, [0, 0, 0]),
                    (90, [0, -24, 0]),
                    (120, [0, 0, 0]),
                ],
            ),
            rot_track(2, 1, &[(0, -6.0), (60, 6.0), (120, -6.0)]),
            rot_track(4, 1, &[(0, 0.0), (60, -20.0), (120, 0.0)]),
            rot_track(5, 1, &[(0, 0.0), (60, 20.0), (120, 0.0)]),
        ],
    );
    // Swim: the body sways against a wider tail beat.
    let swim = anim(
        "swim",
        40,
        vec![
            rot_track(
                1,
                1,
                &[(0, 5.0), (10, 0.0), (20, -5.0), (30, 0.0), (40, 5.0)],
            ),
            rot_track(
                2,
                1,
                &[(0, -28.0), (10, 0.0), (20, 28.0), (30, 0.0), (40, -28.0)],
            ),
        ],
    );
    Animal {
        name: "Trout",
        dir: OUT,
        palette: &TROUT_PALETTE,
        actions: vec![("swim", vec![1, 2])],
        uuid: [
            0x5a, 0x87, 0x10, 0x01, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ],
        parts: vec![body, tail, dorsal, near, far],
        clips: vec![idle, swim],
        extent: [-8, 4, -3, 3, -2, 2],
    }
}
