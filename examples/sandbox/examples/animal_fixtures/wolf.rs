use super::*;

/// Grey wolf palette: grey coat, dark saddle and tail tip, cream belly and
/// muzzle, black nose, amber eyes.
const WOLF_PALETTE: [[u8; 3]; 5] = [
    [132, 128, 120],
    [72, 68, 64],
    [214, 206, 188],
    [28, 26, 24],
    [198, 152, 62],
];
const COAT: u16 = 1;
const SADDLE: u16 = 2;
const BELLY: u16 = 3;
const NOSE: u16 = 4;
const EYE: u16 = 5;

/// Upper and lower leg parts: fore near/far, then hind near/far.
const UPPER: [u32; 4] = [5, 6, 7, 8];
const LOWER: [u32; 4] = [9, 10, 11, 12];

/// A grey wolf: a deep-chested body about 1.1 m with a 0.45 m bushy tail and
/// long legs, 0.85 m at the shoulder, facing +X, with a long muzzle and upright
/// pointed ears. Legs hinge about Z like the other quadrupeds; paws stand 11
/// cells below the root.
pub(super) fn wolf() -> Animal {
    let fur = |[x, y, _]: [i32; 3]| {
        let tint = if y <= -2 {
            BELLY
        } else if y >= 2 && x < 4 {
            SADDLE
        } else {
            COAT
        };
        ("wood", tint)
    };
    let mut torso = part(1, 0, "torso", [0, 0, 0]);
    ellipsoid(&mut torso, [0.0, 0.0, 0.0], [8.0, 3.2, 2.6], &fur);
    ellipsoid(&mut torso, [4.5, -0.3, 0.0], [3.5, 3.8, 2.9], &fur);
    ellipsoid(&mut torso, [-5.0, 0.3, 0.0], [3.0, 3.2, 2.6], &fur);

    let mut neck = part(2, 1, "neck", [7, 1, 0]);
    limb(&mut neck, [0.0, 0.0, 0.0], [3.0, 1.5, 0.0], 2.4, 1.9, &fur);

    let mut head = part(3, 2, "head", [3, 1, 0]);
    ellipsoid(
        &mut head,
        [1.0, 0.5, 0.0],
        [2.2, 2.0, 1.9],
        &flat("wood", COAT),
    );
    limb(
        &mut head,
        [2.5, -0.3, 0.0],
        [5.5, -0.8, 0.0],
        1.1,
        0.8,
        &|[_, y, _]: [i32; 3]| ("wood", if y >= 0 { COAT } else { BELLY }),
    );
    for at in [[6, -1, 0], [6, 0, 0]] {
        paint(&mut head, at, ("stone", NOSE));
    }
    for side in [-1, 1] {
        paint(&mut head, [2, 1, side * 2], ("snow", EYE));
        // Upright pointed ears.
        limb(
            &mut head,
            [0.0, 2.0, f64::from(side) * 1.2],
            [-0.3, 4.0, f64::from(side) * 1.4],
            0.8,
            0.3,
            &flat("wood", SADDLE),
        );
    }

    // A bushy tail hanging low, dark toward the tip.
    let tipped = |[x, _, _]: [i32; 3]| ("wood", if x <= -5 { SADDLE } else { COAT });
    let mut tail = part(4, 1, "tail", [-8, 1, 0]);
    limb(
        &mut tail,
        [0.0, 0.0, 0.0],
        [-6.0, -3.0, 0.0],
        1.2,
        1.5,
        &tipped,
    );

    let mut legs = Vec::new();
    for (i, (name, x, z)) in [
        ("foreleg_near", 5, 2),
        ("foreleg_far", 5, -2),
        ("hindleg_near", -5, 2),
        ("hindleg_far", -5, -2),
    ]
    .into_iter()
    .enumerate()
    {
        let mut upper = part(UPPER[i], 1, name, [x, -2, z]);
        limb(
            &mut upper,
            [0.0, 0.0, 0.0],
            [0.0, -4.0, 0.0],
            1.6,
            1.1,
            &flat("wood", COAT),
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
        let mut lower = part(LOWER[i], UPPER[i], name, [0, -4, 0]);
        limb(
            &mut lower,
            [0.0, 0.0, 0.0],
            [0.0, -3.5, 0.0],
            0.9,
            0.8,
            &flat("wood", COAT),
        );
        ellipsoid(
            &mut lower,
            [0.5, -4.2, 0.0],
            [1.2, 0.7, 1.0],
            &flat("wood", SADDLE),
        );
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

    // Idle: the head turns to scan and the tail sways a little.
    let idle = anim(
        "idle",
        120,
        vec![
            rot_track(3, 1, &[(0, -12.0), (60, 12.0), (120, -12.0)]),
            rot_track(4, 1, &[(0, -6.0), (60, 6.0), (120, -6.0)]),
        ],
    );

    // Walk: a loping diagonal trot, head carried low and level.
    let diagonal = [1.0, -1.0, -1.0, 1.0];
    let mut walk_tracks = vec![
        rot_track(2, 2, &[(0, -10.0), (24, -6.0), (48, -10.0)]),
        rot_track(4, 1, &[(0, -8.0), (24, 8.0), (48, -8.0)]),
    ];
    for i in 0..4 {
        walk_tracks.push(rot_track(UPPER[i], 2, &swing(24.0, diagonal[i], 48)));
    }
    for i in 0..4 {
        walk_tracks.push(rot_track(LOWER[i], 2, &fold(35.0, diagonal[i], 48)));
    }
    let walk = anim("walk", 48, walk_tracks);

    // Run: a stretched gallop, the back flexing and the tail streaming out.
    let pair = [1.0, 1.0, -1.0, -1.0];
    let mut run_tracks = vec![
        rot_trans_track(
            1,
            2,
            &[
                (0, 5.0, [0, 0, 0]),
                (8, 0.0, [0, 256, 0]),
                (15, -5.0, [0, 0, 0]),
                (23, 0.0, [0, -128, 0]),
                (30, 5.0, [0, 0, 0]),
            ],
        ),
        rot_track(4, 2, &[(0, 25.0), (15, 35.0), (30, 25.0)]),
    ];
    for i in 0..4 {
        run_tracks.push(rot_track(UPPER[i], 2, &swing(48.0, pair[i], 30)));
    }
    for i in 0..4 {
        run_tracks.push(rot_track(LOWER[i], 2, &fold(55.0, pair[i], 30)));
    }
    let run = anim("run", 30, run_tracks);

    // Stalk: crouched and creeping, head low and forward, tail straight back.
    let low = -2 * 256;
    let mut stalk_tracks = vec![
        translate_track(
            1,
            &[(0, [0, low, 0]), (40, [0, low - 32, 0]), (80, [0, low, 0])],
        ),
        rot_track(2, 2, &[(0, -25.0), (80, -25.0)]),
        rot_track(3, 2, &[(0, 15.0), (80, 15.0)]),
        rot_track(4, 2, &[(0, 20.0), (80, 20.0)]),
    ];
    for i in 0..4 {
        let crouch = if i < 2 { 30.0 } else { -30.0 };
        let keys: Vec<(u32, f64)> = swing(10.0, diagonal[i], 80)
            .into_iter()
            .map(|(t, a)| (t, a + crouch))
            .collect();
        stalk_tracks.push(rot_track(UPPER[i], 2, &keys));
    }
    for (i, &id) in LOWER.iter().enumerate() {
        let a = if i < 2 { -50.0 } else { 50.0 };
        stalk_tracks.push(rot_track(id, 2, &[(0, a), (80, a)]));
    }
    let stalk = anim("stalk", 80, stalk_tracks);

    // Bite: a lunge forward, the head dipping and snapping up.
    let bite = anim(
        "bite",
        30,
        vec![
            translate_track(
                1,
                &[
                    (0, [0, 0, 0]),
                    (10, [2 * 256, -128, 0]),
                    (18, [2 * 256, -128, 0]),
                    (30, [0, 0, 0]),
                ],
            ),
            rot_track(
                2,
                2,
                &[(0, 0.0), (10, -30.0), (16, -10.0), (22, -30.0), (30, 0.0)],
            ),
            rot_track(3, 2, &[(0, 0.0), (10, 20.0), (16, -15.0), (30, 0.0)]),
            rot_track(UPPER[0], 2, &[(0, 0.0), (10, 30.0), (30, 0.0)]),
            rot_track(UPPER[1], 2, &[(0, 0.0), (10, 30.0), (30, 0.0)]),
        ],
    );

    // Eat: head down at the food, tearing with short tugs.
    let eat = anim(
        "eat",
        90,
        vec![
            rot_track(2, 2, &[(0, -50.0), (90, -50.0)]),
            rot_track(
                3,
                2,
                &[
                    (0, -25.0),
                    (15, -8.0),
                    (25, -25.0),
                    (50, -25.0),
                    (65, -5.0),
                    (75, -25.0),
                    (90, -25.0),
                ],
            ),
            rot_track(UPPER[0], 2, &[(0, 15.0), (90, 15.0)]),
            rot_track(UPPER[1], 2, &[(0, 15.0), (90, 15.0)]),
        ],
    );

    // Howl: head thrown back, muzzle to the sky, tail low, the chest swelling
    // with each breath of the call.
    let howl = anim(
        "howl",
        120,
        vec![
            rot_trans_track(
                1,
                2,
                &[
                    (0, 6.0, [0, 0, 0]),
                    (60, 8.0, [0, 48, 0]),
                    (120, 6.0, [0, 0, 0]),
                ],
            ),
            rot_track(2, 2, &[(0, 45.0), (60, 52.0), (120, 45.0)]),
            rot_track(3, 2, &[(0, 30.0), (60, 38.0), (120, 30.0)]),
            rot_track(4, 2, &[(0, -15.0), (120, -15.0)]),
        ],
    );

    let legs_all: Vec<u32> = UPPER.iter().chain(&LOWER).copied().collect();
    Animal {
        name: "Wolf",
        dir: OUT,
        palette: &WOLF_PALETTE,
        actions: vec![
            ("walk", [vec![2, 4], legs_all.clone()].concat()),
            ("run", [vec![1, 4], legs_all.clone()].concat()),
            ("stalk", [vec![1, 2, 3, 4], legs_all].concat()),
            ("bite", vec![1, 2, 3, UPPER[0], UPPER[1]]),
            ("eat", vec![2, 3, UPPER[0], UPPER[1]]),
            ("howl", vec![1, 2, 3, 4]),
        ],
        uuid: [
            0x5a, 0x87, 0x10, 0x06, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x06,
        ],
        parts: [vec![torso, neck, head, tail], legs].concat(),
        clips: vec![idle, walk, run, stalk, bite, eat, howl],
        extent: [-15, 16, -10, 5, -3, 3],
    }
}
