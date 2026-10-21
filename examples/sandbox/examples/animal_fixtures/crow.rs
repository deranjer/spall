use super::*;

pub(super) fn crow() -> Animal {
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
