use super::*;

/// Deer palette: coat, dark back, white belly and flag, hooves/nose/eyes,
/// antler, muzzle.
const DEER_PALETTE: [[u8; 3]; 6] = [
    [139, 99, 62],
    [98, 70, 44],
    [232, 226, 210],
    [30, 28, 28],
    [206, 190, 158],
    [168, 138, 108],
];
const COAT: u16 = 1;
const BACK: u16 = 2;
const WHITE: u16 = 3;
const DARK: u16 = 4;
const ANTLER: u16 = 5;
const MUZZLE: u16 = 6;

/// Ids of the four upper and four lower leg parts, near/far fore then hind.
const UPPER: [u32; 4] = [6, 7, 8, 9];
const LOWER: [u32; 4] = [10, 11, 12, 13];

/// A white-tailed buck about 1.8 m nose to rump and 1.1 m at the shoulder,
/// facing +X. The torso is the root, its pivot mid-body; legs hinge about Z at
/// the shoulder/hip and again at the knee/hock, where a positive angle swings
/// the hanging limb forward. The hooves stand 12 cells below the root.
pub(super) fn deer() -> Animal {
    let mut torso = part(1, 0, "torso", [0, 0, 0]);
    let coat = |[x, y, z]: [i32; 3]| {
        if y <= -4 || (x >= 7 && y <= -1 && z.abs() <= 2) {
            ("wood", WHITE) // Belly and throat patch.
        } else if y >= 5 {
            ("wood", BACK)
        } else {
            ("wood", COAT)
        }
    };
    ellipsoid(&mut torso, [0.0, 0.0, 0.0], [10.0, 5.5, 4.2], &coat);
    // Fuller chest and haunch than a plain ellipsoid.
    ellipsoid(&mut torso, [6.0, 0.5, 0.0], [4.5, 5.0, 4.0], &coat);
    ellipsoid(&mut torso, [-7.0, 0.5, 0.0], [4.0, 5.2, 4.2], &coat);

    let mut neck = part(2, 1, "neck", [8, 2, 0]);
    limb(
        &mut neck,
        [0.0, 0.0, 0.0],
        [4.0, 7.0, 0.0],
        3.0,
        2.2,
        &|[x, y, z]: [i32; 3]| {
            if x >= 2 && z.abs() <= 1 && y <= 4 {
                ("wood", WHITE)
            } else {
                ("wood", COAT)
            }
        },
    );

    let mut head = part(3, 2, "head", [4, 7, 0]);
    ellipsoid(
        &mut head,
        [1.5, 1.0, 0.0],
        [3.0, 2.4, 2.2],
        &flat("wood", COAT),
    );
    limb(
        &mut head,
        [3.0, 0.5, 0.0],
        [6.5, -0.5, 0.0],
        1.8,
        1.2,
        &flat("wood", MUZZLE),
    );
    paint(&mut head, [7, -1, 0], ("stone", DARK));
    paint(&mut head, [7, 0, 0], ("stone", DARK));
    for side in [-2, 2] {
        paint(&mut head, [3, 2, side], ("stone", DARK));
        // Ears: thin upright ovals set back on the skull.
        ellipsoid(
            &mut head,
            [0.0, 3.5, f64::from(side) * 1.4],
            [0.7, 1.5, 0.6],
            &flat("wood", COAT),
        );
    }

    let mut antlers = part(4, 3, "branched_antlers", [1, 3, 0]);
    for s in [-1.0, 1.0] {
        let beam = [
            [0.0, 0.0, s],
            [-1.0, 3.0, s * 2.5],
            [1.5, 5.0, s * 3.2],
            [4.0, 5.5, s * 2.5],
        ];
        for pair in beam.windows(2) {
            limb(
                &mut antlers,
                pair[0],
                pair[1],
                0.6,
                0.5,
                &flat("wood", ANTLER),
            );
        }
        for (base, tip) in [
            ([-0.5, 2.0, s * 2.0], [-0.5, 4.0, s * 2.5]),
            ([1.5, 5.0, s * 3.2], [1.5, 7.0, s * 3.0]),
            ([3.0, 5.4, s * 2.8], [3.5, 7.0, s * 2.5]),
        ] {
            limb(&mut antlers, base, tip, 0.5, 0.5, &flat("wood", ANTLER));
        }
    }

    let mut tail = part(5, 1, "tail", [-10, 2, 0]);
    limb(
        &mut tail,
        [0.0, 0.0, 0.0],
        [-2.0, 1.0, 0.0],
        1.4,
        1.0,
        &|[_, y, _]: [i32; 3]| {
            if y <= 0 {
                ("wood", WHITE)
            } else {
                ("wood", COAT)
            }
        },
    );

    let mut legs = Vec::new();
    let joints = [
        ("foreleg_near", 6.0, 2.5, 2.0),
        ("foreleg_far", 6.0, -2.5, 2.0),
        ("hindleg_near", -6.5, 2.5, 2.6),
        ("hindleg_far", -6.5, -2.5, 2.6),
    ];
    for (i, (name, x, z, thigh)) in joints.into_iter().enumerate() {
        let mut upper = part(UPPER[i], 1, name, [x as i32, -2, z as i32]);
        limb(
            &mut upper,
            [0.0, 0.0, 0.0],
            [0.0, -5.0, 0.0],
            thigh,
            1.2,
            &flat("wood", COAT),
        );
        legs.push(upper);
    }
    for (i, name) in [
        "shin_fore_near",
        "shin_fore_far",
        "shin_hind_near",
        "shin_hind_far",
    ]
    .into_iter()
    .enumerate()
    {
        let mut lower = part(LOWER[i], UPPER[i], name, [0, -5, 0]);
        limb(
            &mut lower,
            [0.0, 0.0, 0.0],
            [0.0, -3.5, 0.0],
            1.0,
            0.8,
            &flat("wood", COAT),
        );
        for y in [-5, -4] {
            for (x, z) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
                paint(&mut lower, [x, y, z], ("stone", DARK));
            }
        }
        legs.push(lower);
    }

    let idle = anim(
        "idle",
        120,
        vec![
            rot_track(
                3,
                1,
                &[(0, -6.0), (30, 0.0), (60, 6.0), (90, 0.0), (120, -6.0)],
            ),
            rot_track(
                5,
                2,
                &[(0, 0.0), (50, 0.0), (56, 25.0), (62, 0.0), (120, 0.0)],
            ),
        ],
    );

    // Walk: diagonal pairs swing together; each knee folds as its leg comes forward.
    let swing = |amp: f64, phase: f64, cycle: u32| {
        let q = cycle / 4;
        [
            (0, -amp * phase),
            (q, 0.0),
            (2 * q, amp * phase),
            (3 * q, 0.0),
            (cycle, -amp * phase),
        ]
    };
    let fold = |amp: f64, phase: f64, cycle: u32| {
        let q = cycle / 4;
        let (a, b) = if phase > 0.0 {
            (0.0, -amp)
        } else {
            (-amp, 0.0)
        };
        [
            (0, a),
            (q, (a + b) / 2.0),
            (2 * q, b),
            (3 * q, (a + b) / 2.0),
            (cycle, a),
        ]
    };
    let diagonal = [1.0, -1.0, -1.0, 1.0];
    let mut walk_tracks = vec![rot_track(
        2,
        2,
        &[(0, 0.0), (12, -3.0), (24, 0.0), (36, -3.0), (48, 0.0)],
    )];
    for i in 0..4 {
        walk_tracks.push(rot_track(UPPER[i], 2, &swing(18.0, diagonal[i], 48)));
    }
    for i in 0..4 {
        walk_tracks.push(rot_track(LOWER[i], 2, &fold(30.0, diagonal[i], 48)));
    }
    let walk = anim("walk", 48, walk_tracks);

    // Run: a bounding gallop. Forelegs and hindlegs move as pairs half a cycle
    // apart; the body pitches and lifts through the leap.
    let pair = [1.0, 1.0, -1.0, -1.0];
    let mut run_tracks = vec![
        rot_trans_track(
            1,
            2,
            &[
                (0, 6.0, [0, 0, 0]),
                (8, 0.0, [0, 256, 0]),
                (16, -6.0, [0, 0, 0]),
                (24, 0.0, [0, -128, 0]),
                (32, 6.0, [0, 0, 0]),
            ],
        ),
        rot_track(2, 2, &[(0, -10.0), (16, 0.0), (32, -10.0)]),
        rot_track(5, 2, &[(0, 40.0), (16, 50.0), (32, 40.0)]),
    ];
    for i in 0..4 {
        run_tracks.push(rot_track(UPPER[i], 2, &swing(42.0, pair[i], 32)));
    }
    for i in 0..4 {
        run_tracks.push(rot_track(LOWER[i], 2, &fold(55.0, pair[i], 32)));
    }
    let run = anim("run", 32, run_tracks);

    // Graze: the neck swings down until the muzzle is at the grass, the
    // forelegs splay a little, and the head nods as it crops and chews.
    let graze = anim(
        "graze",
        120,
        vec![
            rot_track(1, 2, &[(0, -8.0), (120, -8.0)]),
            rot_track(2, 2, &[(0, -112.0), (60, -108.0), (120, -112.0)]),
            rot_track(
                3,
                2,
                &[
                    (0, 30.0),
                    (20, 22.0),
                    (40, 30.0),
                    (60, 22.0),
                    (80, 30.0),
                    (100, 22.0),
                    (120, 30.0),
                ],
            ),
            rot_track(UPPER[0], 2, &[(0, 10.0), (120, 10.0)]),
            rot_track(UPPER[1], 2, &[(0, 10.0), (120, 10.0)]),
        ],
    );

    // Alert: head high and still, turning to listen; tail flag raised.
    let alert = anim(
        "alert",
        90,
        vec![
            rot_track(2, 2, &[(0, 14.0), (90, 14.0)]),
            rot_track(3, 1, &[(0, 0.0), (30, 25.0), (60, 25.0), (90, 0.0)]),
            rot_track(5, 2, &[(0, 60.0), (90, 60.0)]),
        ],
    );

    // Sleep: bedded down. The body rests on the ground with forelegs folded
    // under and hindlegs tucked forward; the neck curls low and the flank breathes.
    let bed = -6 * 256;
    let sleep = anim(
        "sleep",
        120,
        [
            vec![
                translate_track(
                    1,
                    &[(0, [0, bed, 0]), (60, [0, bed + 48, 0]), (120, [0, bed, 0])],
                ),
                rot_track(2, 2, &[(0, -60.0), (120, -60.0)]),
                rot_track(3, 2, &[(0, 20.0), (120, 20.0)]),
            ],
            UPPER
                .iter()
                .enumerate()
                .map(|(i, &id)| {
                    let a = if i < 2 { -80.0 } else { 75.0 };
                    rot_track(id, 2, &[(0, a), (120, a)])
                })
                .collect(),
            LOWER
                .iter()
                .enumerate()
                .map(|(i, &id)| {
                    let a = if i < 2 { 160.0 } else { -150.0 };
                    rot_track(id, 2, &[(0, a), (120, a)])
                })
                .collect(),
        ]
        .concat(),
    );

    let legs_all: Vec<u32> = UPPER.iter().chain(&LOWER).copied().collect();
    Animal {
        name: "Deer",
        dir: OUT,
        palette: &DEER_PALETTE,
        actions: vec![
            ("walk", [vec![2], legs_all.clone()].concat()),
            ("run", [vec![1, 2, 5], legs_all.clone()].concat()),
            ("graze", vec![1, 2, 3, UPPER[0], UPPER[1]]),
            ("alert", vec![2, 3, 5]),
            ("sleep", [vec![1, 2, 3], legs_all].concat()),
        ],
        uuid: [
            0x5a, 0x87, 0x10, 0x03, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x03,
        ],
        parts: [vec![torso, neck, head, antlers, tail], legs].concat(),
        clips: vec![idle, walk, run, graze, alert, sleep],
        extent: [-13, 19, -12, 19, -4, 4],
    }
}
