use super::*;

/// Snow leopard palette: smoky cream coat, dark rosettes, white belly, pink
/// nose, pale green eyes.
const LEOPARD_PALETTE: [[u8; 3]; 5] = [
    [204, 198, 182],
    [64, 64, 66],
    [238, 236, 228],
    [188, 136, 136],
    [150, 172, 118],
];
const COAT: u16 = 1;
const ROSETTE: u16 = 2;
const BELLY: u16 = 3;
const NOSE: u16 = 4;
const EYE: u16 = 5;

/// Upper and lower leg parts: fore near/far, then hind near/far.
const UPPER: [u32; 4] = [6, 7, 8, 9];
const LOWER: [u32; 4] = [10, 11, 12, 13];

/// A snow leopard: a long low body about 1.2 m with a thick 0.8 m tail and
/// broad paws, 0.75 m at the shoulder, facing +X. The tail is two parts so it
/// can curl. Legs hinge about Z like the other quadrupeds; paws stand 8 cells
/// below the root.
pub(super) fn leopard() -> Animal {
    let fur = |[x, y, z]: [i32; 3]| {
        let tint = if y <= -3 {
            BELLY
        } else if (x * 3 + y * 5 + z * 7).rem_euclid(7) == 0 {
            ROSETTE
        } else {
            COAT
        };
        ("wood", tint)
    };
    let mut torso = part(1, 0, "torso", [0, 0, 0]);
    ellipsoid(&mut torso, [0.0, 0.0, 0.0], [9.0, 3.6, 3.2], &fur);
    ellipsoid(&mut torso, [5.0, 0.2, 0.0], [4.0, 3.8, 3.3], &fur);
    ellipsoid(&mut torso, [-5.5, 0.4, 0.0], [3.5, 3.8, 3.4], &fur);

    let mut neck = part(2, 1, "neck", [8, 1, 0]);
    limb(&mut neck, [0.0, 0.0, 0.0], [2.5, 1.0, 0.0], 2.4, 2.0, &fur);

    let mut head = part(3, 2, "head", [3, 1, 0]);
    ellipsoid(&mut head, [1.0, 0.5, 0.0], [2.4, 2.1, 2.1], &fur);
    ellipsoid(
        &mut head,
        [3.0, -0.3, 0.0],
        [1.3, 1.1, 1.3],
        &flat("wood", BELLY),
    );
    paint(&mut head, [4, 0, 0], ("clay", NOSE));
    for side in [-1, 1] {
        paint(&mut head, [2, 1, side * 2], ("snow", EYE));
        ellipsoid(
            &mut head,
            [0.0, 2.4, f64::from(side) * 1.4],
            [0.6, 0.7, 0.6],
            &flat("wood", COAT),
        );
    }

    // The tail ringed toward its fluffy tip.
    let ringed = |[x, _, _]: [i32; 3]| ("wood", if x.rem_euclid(3) == 0 { ROSETTE } else { COAT });
    let mut tail_base = part(4, 1, "tail_base", [-9, 1, 0]);
    limb(
        &mut tail_base,
        [0.0, 0.0, 0.0],
        [-7.0, -2.0, 0.0],
        1.6,
        1.5,
        &fur,
    );
    let mut tail_tip = part(5, 4, "tail_tip", [-7, -2, 0]);
    limb(
        &mut tail_tip,
        [0.0, 0.0, 0.0],
        [-6.0, 1.5, 0.0],
        1.5,
        1.7,
        &ringed,
    );

    let mut legs = Vec::new();
    for (i, (name, x, z)) in [
        ("foreleg_near", 5, 2),
        ("foreleg_far", 5, -2),
        ("hindleg_near", -6, 2),
        ("hindleg_far", -6, -2),
    ]
    .into_iter()
    .enumerate()
    {
        let mut upper = part(UPPER[i], 1, name, [x, -2, z]);
        limb(
            &mut upper,
            [0.0, 0.0, 0.0],
            [0.0, -2.5, 0.0],
            1.8,
            1.3,
            &fur,
        );
        legs.push(upper);
    }
    for (i, name) in [
        "paw_fore_near",
        "paw_fore_far",
        "paw_hind_near",
        "paw_hind_far",
    ]
    .into_iter()
    .enumerate()
    {
        let mut lower = part(LOWER[i], UPPER[i], name, [0, -3, 0]);
        limb(
            &mut lower,
            [0.0, 0.0, 0.0],
            [0.0, -2.0, 0.0],
            1.2,
            1.1,
            &fur,
        );
        ellipsoid(&mut lower, [0.6, -2.4, 0.0], [1.6, 0.9, 1.4], &fur);
        legs.push(lower);
    }

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

    // Idle: the tail tip flicks and curls; the head scans.
    let idle = anim(
        "idle",
        120,
        vec![
            rot_track(3, 1, &[(0, -8.0), (60, 8.0), (120, -8.0)]),
            rot_track(
                5,
                2,
                &[(0, 0.0), (30, 25.0), (60, 5.0), (90, 25.0), (120, 0.0)],
            ),
        ],
    );

    // Walk: an easy diagonal gait, head level, tail swaying.
    let diagonal = [1.0, -1.0, -1.0, 1.0];
    let mut walk_tracks = vec![rot_track(4, 1, &[(0, -10.0), (28, 10.0), (56, -10.0)])];
    for i in 0..4 {
        walk_tracks.push(rot_track(UPPER[i], 2, &swing(20.0, diagonal[i], 56)));
    }
    for i in 0..4 {
        walk_tracks.push(rot_track(LOWER[i], 2, &fold(30.0, diagonal[i], 56)));
    }
    let walk = anim("walk", 56, walk_tracks);

    // Stalk: belly low, shoulders up, a slow creep with the head held level
    // and the tail stretched out behind.
    let low = -2 * 256;
    let mut stalk_tracks = vec![
        translate_track(
            1,
            &[(0, [0, low, 0]), (40, [0, low - 32, 0]), (80, [0, low, 0])],
        ),
        rot_track(2, 2, &[(0, -20.0), (80, -20.0)]),
        rot_track(3, 2, &[(0, 20.0), (80, 20.0)]),
        rot_track(4, 2, &[(0, -15.0), (80, -15.0)]),
    ];
    for i in 0..4 {
        let crouch = if i < 2 { 35.0 } else { -35.0 };
        let keys: Vec<(u32, f64)> = swing(8.0, diagonal[i], 80)
            .into_iter()
            .map(|(t, a)| (t, a + crouch))
            .collect();
        stalk_tracks.push(rot_track(UPPER[i], 2, &keys));
    }
    for (i, &id) in LOWER.iter().enumerate() {
        let a = if i < 2 { -60.0 } else { 60.0 };
        stalk_tracks.push(rot_track(id, 2, &[(0, a), (80, a)]));
    }
    let stalk = anim("stalk", 80, stalk_tracks);

    // Pounce: a deep crouch, then the body launches and stretches out, forepaws
    // reaching ahead.
    let pounce = anim(
        "pounce",
        40,
        [
            vec![
                rot_trans_track(
                    1,
                    2,
                    &[
                        (0, 0.0, [0, low, 0]),
                        (12, -5.0, [0, low - 128, 0]),
                        (22, 20.0, [0, 6 * 256, 0]),
                        (32, -10.0, [0, 2 * 256, 0]),
                        (40, 0.0, [0, low, 0]),
                    ],
                ),
                rot_track(5, 2, &[(0, 0.0), (22, -30.0), (40, 0.0)]),
            ],
            UPPER
                .iter()
                .enumerate()
                .map(|(i, &id)| {
                    let (crouch, reach) = if i < 2 { (35.0, 80.0) } else { (-35.0, -70.0) };
                    rot_track(
                        id,
                        2,
                        &[
                            (0, crouch),
                            (12, crouch),
                            (22, reach),
                            (32, reach * 0.5),
                            (40, crouch),
                        ],
                    )
                })
                .collect(),
        ]
        .concat(),
    );

    // Run: a bounding sprint with a long flexing back and tail for balance.
    let pair = [1.0, 1.0, -1.0, -1.0];
    let mut run_tracks = vec![
        rot_trans_track(
            1,
            2,
            &[
                (0, 6.0, [0, 0, 0]),
                (8, 0.0, [0, 256, 0]),
                (15, -6.0, [0, 0, 0]),
                (23, 0.0, [0, -128, 0]),
                (30, 6.0, [0, 0, 0]),
            ],
        ),
        rot_track(4, 2, &[(0, -10.0), (15, 10.0), (30, -10.0)]),
    ];
    for i in 0..4 {
        run_tracks.push(rot_track(UPPER[i], 2, &swing(45.0, pair[i], 30)));
    }
    for i in 0..4 {
        run_tracks.push(rot_track(LOWER[i], 2, &fold(55.0, pair[i], 30)));
    }
    let run = anim("run", 30, run_tracks);

    let legs_all: Vec<u32> = UPPER.iter().chain(&LOWER).copied().collect();
    Animal {
        name: "Leopard",
        dir: OUT,
        palette: &LEOPARD_PALETTE,
        actions: vec![
            ("walk", [vec![4], legs_all.clone()].concat()),
            ("stalk", [vec![1, 2, 3, 4], legs_all.clone()].concat()),
            ("pounce", [vec![1, 5], UPPER.to_vec()].concat()),
            ("run", [vec![1, 4], legs_all].concat()),
        ],
        uuid: [
            0x5a, 0x87, 0x10, 0x05, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x05,
        ],
        parts: [vec![torso, neck, head, tail_base, tail_tip], legs].concat(),
        clips: vec![idle, walk, stalk, pounce, run],
        extent: [-23, 15, -8, 4, -3, 3],
    }
}
