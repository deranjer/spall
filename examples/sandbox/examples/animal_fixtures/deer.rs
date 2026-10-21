use super::*;

pub(super) fn deer() -> Animal {
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
