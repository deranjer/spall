use super::*;

/// Grizzly palette: brown fur, dark legs, grizzled tips, muzzle, nose, claws.
const BEAR_PALETTE: [[u8; 3]; 6] = [
    [108, 74, 46],
    [70, 48, 31],
    [152, 120, 84],
    [166, 130, 92],
    [28, 24, 22],
    [214, 204, 182],
];
const FUR: u16 = 1;
const LEGS: u16 = 2;
const GRIZZLE: u16 = 3;
const MUZZLE: u16 = 4;
const NOSE: u16 = 5;
const CLAW: u16 = 6;

/// Upper and lower leg parts: fore near/far, then hind near/far.
const UPPER: [u32; 4] = [5, 6, 7, 8];
const LOWER: [u32; 4] = [9, 10, 11, 12];

/// A grizzly about 2.2 m nose to rump and 1.15 m at the hump, facing +X. The
/// torso is the root; legs hinge about Z at shoulder/hip and elbow/hock, a
/// positive angle swinging the hanging limb forward. Paws stand 11 cells below
/// the root.
pub(super) fn bear() -> Animal {
    let coat = |[_, y, _]: [i32; 3]| ("wood", if y >= 5 { GRIZZLE } else { FUR });
    let mut torso = part(1, 0, "torso", [0, 0, 0]);
    ellipsoid(&mut torso, [0.0, 0.0, 0.0], [13.0, 6.0, 6.0], &coat);
    ellipsoid(&mut torso, [6.0, 4.0, 0.0], [5.0, 3.5, 4.5], &coat);
    ellipsoid(&mut torso, [-8.0, 0.5, 0.0], [6.0, 6.0, 6.0], &coat);

    let mut neck = part(2, 1, "neck", [11, 2, 0]);
    limb(&mut neck, [0.0, 0.0, 0.0], [3.0, 0.5, 0.0], 4.5, 3.8, &coat);

    let mut head = part(3, 2, "head", [3, 0, 0]);
    ellipsoid(
        &mut head,
        [2.0, 0.5, 0.0],
        [3.5, 3.2, 3.2],
        &flat("wood", FUR),
    );
    limb(
        &mut head,
        [4.0, -0.5, 0.0],
        [7.5, -1.0, 0.0],
        2.0,
        1.5,
        &flat("wood", MUZZLE),
    );
    for at in [[8, -1, 0], [9, -1, 0], [8, 0, 0]] {
        paint(&mut head, at, ("stone", NOSE));
    }
    for side in [-1.0, 1.0] {
        ellipsoid(
            &mut head,
            [0.5, 3.5, side * 2.3],
            [0.9, 1.1, 0.8],
            &flat("wood", LEGS),
        );
        paint(&mut head, [4, 1, side as i32 * 2], ("stone", NOSE));
    }

    let mut tail = part(4, 1, "tail", [-13, 2, 0]);
    ellipsoid(
        &mut tail,
        [-0.5, 0.0, 0.0],
        [1.3, 1.2, 1.2],
        &flat("wood", FUR),
    );

    let mut legs = Vec::new();
    for (i, (name, x, z)) in [
        ("foreleg_near", 7, 3),
        ("foreleg_far", 7, -3),
        ("hindleg_near", -8, 3),
        ("hindleg_far", -8, -3),
    ]
    .into_iter()
    .enumerate()
    {
        let mut upper = part(UPPER[i], 1, name, [x, -2, z]);
        limb(
            &mut upper,
            [0.0, 0.0, 0.0],
            [0.0, -4.0, 0.0],
            3.2,
            2.4,
            &flat("wood", LEGS),
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
            2.2,
            2.0,
            &flat("wood", LEGS),
        );
        ellipsoid(
            &mut lower,
            [1.0, -4.5, 0.0],
            [2.6, 1.2, 2.2],
            &flat("wood", LEGS),
        );
        for z in -1..=1 {
            paint(&mut lower, [4, -5, z], ("stone", CLAW));
        }
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

    // Idle: slow breathing, the head swinging to sniff the air.
    let idle = anim(
        "idle",
        120,
        vec![
            translate_track(1, &[(0, [0, 0, 0]), (60, [0, 40, 0]), (120, [0, 0, 0])]),
            rot_track(2, 2, &[(0, 0.0), (40, 8.0), (80, 0.0), (120, 0.0)]),
            rot_track(3, 1, &[(0, -10.0), (60, 10.0), (120, -10.0)]),
        ],
    );

    // Walk: a heavy, rolling diagonal gait with the head swinging low.
    let diagonal = [1.0, -1.0, -1.0, 1.0];
    let mut walk_tracks = vec![
        rot_track(1, 0, &[(0, -2.0), (32, 2.0), (64, -2.0)]),
        rot_track(2, 1, &[(0, -5.0), (32, 5.0), (64, -5.0)]),
    ];
    for i in 0..4 {
        walk_tracks.push(rot_track(UPPER[i], 2, &swing(16.0, diagonal[i], 64)));
    }
    for i in 0..4 {
        walk_tracks.push(rot_track(LOWER[i], 2, &fold(25.0, diagonal[i], 64)));
    }
    let walk = anim("walk", 64, walk_tracks);

    // Run: a rotary gallop; the back flexes and the forequarters lead.
    let pair = [1.0, 0.7, -1.0, -0.7];
    let mut run_tracks = vec![rot_trans_track(
        1,
        2,
        &[
            (0, 5.0, [0, 0, 0]),
            (9, 0.0, [0, 256, 0]),
            (18, -5.0, [0, 0, 0]),
            (27, 0.0, [0, -128, 0]),
            (36, 5.0, [0, 0, 0]),
        ],
    )];
    for i in 0..4 {
        run_tracks.push(rot_track(UPPER[i], 2, &swing(35.0, pair[i], 36)));
    }
    for i in 0..4 {
        run_tracks.push(rot_track(LOWER[i], 2, &fold(45.0, pair[i], 36)));
    }
    let run = anim("run", 36, run_tracks);

    // Swipe: lifts the forequarters, raises the near paw high and rakes it
    // down and across.
    let swipe = anim(
        "swipe",
        40,
        vec![
            rot_trans_track(
                1,
                2,
                &[
                    (0, 0.0, [0, 0, 0]),
                    (14, 18.0, [0, 384, 0]),
                    (24, 6.0, [0, 128, 0]),
                    (40, 0.0, [0, 0, 0]),
                ],
            ),
            rot_track(3, 2, &[(0, 0.0), (14, -15.0), (24, -5.0), (40, 0.0)]),
            rot_track(
                UPPER[0],
                2,
                &[(0, 0.0), (14, 120.0), (20, 60.0), (26, -10.0), (40, 0.0)],
            ),
            rot_track(
                LOWER[0],
                2,
                &[(0, 0.0), (14, -50.0), (20, -10.0), (26, 0.0), (40, 0.0)],
            ),
        ],
    );

    // Eat: head down at the food, tearing with short upward tugs.
    let eat = anim(
        "eat",
        120,
        vec![
            rot_track(2, 2, &[(0, -45.0), (120, -45.0)]),
            rot_track(
                3,
                2,
                &[
                    (0, -30.0),
                    (20, -15.0),
                    (30, -30.0),
                    (60, -30.0),
                    (80, -12.0),
                    (90, -30.0),
                    (120, -30.0),
                ],
            ),
            rot_track(UPPER[0], 2, &[(0, 12.0), (120, 12.0)]),
            rot_track(UPPER[1], 2, &[(0, 12.0), (120, 12.0)]),
        ],
    );

    // Stand: rears up on the hind legs to look around, forepaws hanging.
    let up = 70.0;
    let stand = anim(
        "stand",
        90,
        vec![
            rot_trans_track(
                1,
                2,
                &[
                    (0, up, [0, 6 * 256, 0]),
                    (45, up - 3.0, [0, 6 * 256 + 64, 0]),
                    (90, up, [0, 6 * 256, 0]),
                ],
            ),
            rot_track(2, 2, &[(0, -40.0), (90, -40.0)]),
            rot_track(3, 2, &[(0, -20.0), (45, -10.0), (90, -20.0)]),
            rot_track(UPPER[0], 2, &[(0, -40.0), (90, -40.0)]),
            rot_track(UPPER[1], 2, &[(0, -40.0), (90, -40.0)]),
            rot_track(UPPER[2], 2, &[(0, -up), (90, -up)]),
            rot_track(UPPER[3], 2, &[(0, -up), (90, -up)]),
            rot_track(LOWER[0], 2, &[(0, 40.0), (90, 40.0)]),
            rot_track(LOWER[1], 2, &[(0, 40.0), (90, 40.0)]),
        ],
    );

    let legs_all: Vec<u32> = UPPER.iter().chain(&LOWER).copied().collect();
    Animal {
        name: "Bear",
        dir: OUT,
        palette: &BEAR_PALETTE,
        actions: vec![
            ("walk", [vec![1, 2], legs_all.clone()].concat()),
            ("run", [vec![1], legs_all].concat()),
            ("swipe", vec![1, 3, UPPER[0], LOWER[0]]),
            ("eat", vec![2, 3, UPPER[0], UPPER[1]]),
            (
                "stand",
                [vec![1, 2, 3], UPPER.to_vec(), vec![LOWER[0], LOWER[1]]].concat(),
            ),
        ],
        uuid: [
            0x5a, 0x87, 0x10, 0x04, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x04,
        ],
        parts: [vec![torso, neck, head, tail], legs].concat(),
        clips: vec![idle, walk, run, swipe, eat, stand],
        extent: [-14, 23, -11, 7, -6, 6],
    }
}
