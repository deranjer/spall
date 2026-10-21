use super::*;

pub(super) fn trout() -> Animal {
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
