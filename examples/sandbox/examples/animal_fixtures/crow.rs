use super::*;

/// Crow palette: black plumage, blue-black gloss, beak and feet, eye.
const CROW_PALETTE: [[u8; 3]; 4] = [[24, 24, 30], [38, 44, 66], [52, 50, 52], [96, 72, 40]];
const BLACK: u16 = 1;
const GLOSS: u16 = 2;
const BEAK: u16 = 3;
const EYE: u16 = 4;

/// Folded wing pose for side `+1` (near) or `-1` (far): the span swung back
/// along the body, then the broad inner wing dropped over the flank.
fn folded_pose(side: f64) -> [i16; 4] {
    then(q(1, -88.0 * side), q(0, 70.0 * side))
}

/// A crow about 0.5 m beak to tail with a 0.95 m wingspan, facing +X. The
/// wings are authored spread along ±Z; ground clips fold them back along the
/// body. About X, a positive angle drops the near wing's tip.
pub(super) fn crow() -> Animal {
    let plumage = |[_, y, _]: [i32; 3]| ("stone", if y >= 2 { GLOSS } else { BLACK });
    let mut torso = part(1, 0, "torso", [0, 0, 0]);
    ellipsoid(&mut torso, [0.0, 0.0, 0.0], [3.5, 2.2, 2.0], &plumage);
    ellipsoid(&mut torso, [2.0, 0.6, 0.0], [2.0, 2.0, 1.8], &plumage);

    let mut head = part(2, 1, "head", [3, 2, 0]);
    ellipsoid(&mut head, [1.0, 0.5, 0.0], [1.8, 1.6, 1.5], &plumage);
    limb(
        &mut head,
        [2.3, 0.3, 0.0],
        [4.6, -0.3, 0.0],
        0.9,
        0.4,
        &flat("wood", BEAK),
    );
    paint(&mut head, [2, 1, 1], ("snow", EYE));
    paint(&mut head, [2, 1, -1], ("snow", EYE));

    let wing = |id: u32, name: &'static str, side: i32| {
        let mut wing = part(id, 1, name, [1, 2, 2 * side]);
        for span in 0..=7 {
            // Long primaries at the tip, a broad inner wing near the body.
            let (back, front) = (-3 - span / 3, 2 - span / 3);
            for x in back..=front {
                let tint = if x == back { GLOSS } else { BLACK };
                voxel(&mut wing, x, 0, span * side, "stone", tint);
            }
        }
        wing
    };
    let near = wing(3, "wing_near", 1);
    let far = wing(4, "wing_far", -1);

    let mut tail = part(5, 1, "tail_fan", [-3, 0, 0]);
    for x in -5..=-1 {
        for z in -(1 + (-x) / 2)..=(1 + (-x) / 2) {
            voxel(
                &mut tail,
                x,
                0,
                z,
                "stone",
                if x == -5 { GLOSS } else { BLACK },
            );
        }
    }

    let mut legs = part(6, 1, "legs_and_feet", [0, -2, 0]);
    for z in [-1, 1] {
        let z = f64::from(z);
        limb(
            &mut legs,
            [0.0, 0.0, z],
            [0.5, -3.0, z],
            0.5,
            0.5,
            &flat("wood", BEAK),
        );
        for x in -1..=2 {
            paint(&mut legs, [x, -3, z as i32], ("wood", BEAK));
        }
    }

    let folded = |id: u32, side: f64, ticks: u32| {
        quat_track(id, &[(0, folded_pose(side)), (ticks, folded_pose(side))])
    };

    // Idle: perched with wings folded, bobbing and cocking its head.
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
                    (90, [0, 24, 0]),
                    (120, [0, 0, 0]),
                ],
            ),
            rot_track(
                2,
                1,
                &[
                    (0, 0.0),
                    (40, 0.0),
                    (45, 30.0),
                    (85, 30.0),
                    (90, 0.0),
                    (120, 0.0),
                ],
            ),
            folded(3, 1.0, 120),
            folded(4, -1.0, 120),
        ],
    );

    // Flap: full wingbeats in flight, tail steering slightly.
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
            rot_track(5, 2, &[(0, 0.0), (24, 8.0), (48, 0.0)]),
        ],
    );

    // Hop: a two-footed bound along the ground, wings half open for balance.
    let hop = anim(
        "hop",
        30,
        vec![
            translate_track(
                1,
                &[
                    (0, [0, 0, 0]),
                    (8, [0, 0, 0]),
                    (15, [0, 640, 0]),
                    (22, [0, 0, 0]),
                    (30, [0, 0, 0]),
                ],
            ),
            quat_track(
                3,
                &[
                    (0, folded_pose(1.0)),
                    (15, q(0, 40.0)),
                    (30, folded_pose(1.0)),
                ],
            ),
            quat_track(
                4,
                &[
                    (0, folded_pose(-1.0)),
                    (15, q(0, -40.0)),
                    (30, folded_pose(-1.0)),
                ],
            ),
            rot_track(
                6,
                2,
                &[(0, 0.0), (8, 25.0), (15, -20.0), (22, 10.0), (30, 0.0)],
            ),
        ],
    );

    // Peck: tips forward to stab and tug at the ground, tail rising to balance.
    let peck = anim(
        "peck",
        40,
        vec![
            rot_track(
                1,
                2,
                &[
                    (0, -15.0),
                    (10, -40.0),
                    (16, -40.0),
                    (24, -28.0),
                    (40, -15.0),
                ],
            ),
            rot_track(
                2,
                2,
                &[
                    (0, -10.0),
                    (10, -35.0),
                    (16, -25.0),
                    (24, -35.0),
                    (40, -10.0),
                ],
            ),
            folded(3, 1.0, 40),
            folded(4, -1.0, 40),
            rot_track(5, 2, &[(0, 15.0), (10, 30.0), (40, 15.0)]),
            rot_track(6, 2, &[(0, 15.0), (10, 40.0), (40, 15.0)]),
        ],
    );

    // Land: braking wingbeats with the body upright and feet reaching forward.
    let land = anim(
        "land",
        40,
        vec![
            rot_track(1, 2, &[(0, 30.0), (40, 30.0)]),
            rot_track(
                3,
                0,
                &[(0, -60.0), (10, 10.0), (20, -60.0), (30, 10.0), (40, -60.0)],
            ),
            rot_track(
                4,
                0,
                &[(0, 60.0), (10, -10.0), (20, 60.0), (30, -10.0), (40, 60.0)],
            ),
            rot_track(5, 2, &[(0, -25.0), (40, -25.0)]),
            rot_track(6, 2, &[(0, -45.0), (40, -45.0)]),
        ],
    );

    // Take off: a crouch, a jump and deep, fast downstrokes.
    let takeoff = anim(
        "takeoff",
        30,
        vec![
            rot_trans_track(
                1,
                2,
                &[
                    (0, 10.0, [0, -128, 0]),
                    (8, 20.0, [0, 384, 0]),
                    (30, 10.0, [0, -128, 0]),
                ],
            ),
            rot_track(
                3,
                0,
                &[(0, -70.0), (8, 45.0), (15, -70.0), (23, 45.0), (30, -70.0)],
            ),
            rot_track(
                4,
                0,
                &[(0, 70.0), (8, -45.0), (15, 70.0), (23, -45.0), (30, 70.0)],
            ),
            rot_track(6, 2, &[(0, 30.0), (8, -50.0), (30, 30.0)]),
        ],
    );

    Animal {
        name: "Crow",
        dir: OUT,
        palette: &CROW_PALETTE,
        actions: vec![
            ("flap", vec![3, 4, 5]),
            ("hop", vec![1, 3, 4, 6]),
            ("peck", vec![1, 2, 3, 4, 5, 6]),
            ("land", vec![1, 3, 4, 5, 6]),
            ("takeoff", vec![1, 3, 4, 6]),
        ],
        uuid: [
            0x5a, 0x87, 0x10, 0x02, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02,
        ],
        parts: vec![torso, head, near, far, tail, legs],
        clips: vec![idle, flap, hop, peck, land, takeoff],
        extent: [-8, 7, -5, 4, -9, 9],
    }
}
