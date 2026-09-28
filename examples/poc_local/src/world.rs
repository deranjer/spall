//! A fully hardcoded, static block grid — no chunking, no streaming, no
//! meshing. Every block occupies the unit cube `[x, x+1] x [y, y+1] x [z,
//! z+1]` in world space. This exists purely as a fixed test bed (flat
//! ground, a wall, a staircase up to a platform) to move around on while
//! chasing the movement-jitter issue from scratch.

#[derive(Clone, Copy)]
pub struct BlockKind {
    pub color: [f32; 3],
}

pub const GROUND: BlockKind = BlockKind {
    color: [0.30, 0.55, 0.30],
};
pub const WALL: BlockKind = BlockKind {
    color: [0.55, 0.55, 0.58],
};
pub const STEP: BlockKind = BlockKind {
    color: [0.60, 0.55, 0.35],
};
pub const BOX: BlockKind = BlockKind {
    color: [0.60, 0.35, 0.35],
};

pub struct World {
    pub size_x: i32,
    pub size_y: i32,
    pub size_z: i32,
    blocks: Vec<Option<BlockKind>>,
}

impl World {
    fn empty(size_x: i32, size_y: i32, size_z: i32) -> Self {
        Self {
            size_x,
            size_y,
            size_z,
            blocks: vec![None; (size_x * size_y * size_z) as usize],
        }
    }

    fn index(&self, x: i32, y: i32, z: i32) -> Option<usize> {
        if x < 0 || y < 0 || z < 0 || x >= self.size_x || y >= self.size_y || z >= self.size_z {
            return None;
        }
        Some(((z * self.size_y + y) * self.size_x + x) as usize)
    }

    fn set(&mut self, x: i32, y: i32, z: i32, kind: BlockKind) {
        if let Some(i) = self.index(x, y, z) {
            self.blocks[i] = Some(kind);
        }
    }

    pub fn is_solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.index(x, y, z)
            .is_some_and(|i| self.blocks[i].is_some())
    }

    /// `(min corner, color)` for every solid block — enough to build the
    /// static instance buffer once at startup.
    pub fn iter_blocks(&self) -> impl Iterator<Item = ([f32; 3], [f32; 3])> + '_ {
        (0..self.size_z).flat_map(move |z| {
            (0..self.size_y).flat_map(move |y| {
                (0..self.size_x).filter_map(move |x| {
                    let kind = self.blocks[self.index(x, y, z).unwrap()]?;
                    Some(([x as f32, y as f32, z as f32], kind.color))
                })
            })
        })
    }

    /// Spawn point: open ground, well clear of the wall and staircase.
    pub fn spawn_point(&self) -> [f32; 3] {
        [5.0, 1.0, 5.0]
    }

    pub fn generate() -> Self {
        let mut world = Self::empty(40, 16, 40);

        // Flat ground, one block thick, across the whole footprint.
        for z in 0..world.size_z {
            for x in 0..world.size_x {
                world.set(x, 0, z, GROUND);
            }
        }

        // A wall to walk into / strafe along (horizontal-collision test).
        // Three blocks tall, ten blocks long, at x = 20.
        for z in 15..25 {
            for y in 1..4 {
                world.set(20, y, z, WALL);
            }
        }

        // A large square block (5x5x5) to walk around / strafe past —
        // open ground on every side, close enough to spawn to reach in a
        // few steps.
        for z in 10..15 {
            for x in 10..15 {
                for y in 1..6 {
                    world.set(x, y, z, BOX);
                }
            }
        }

        // A staircase (single ascending steps — no auto step-up assist, so
        // reaching the top requires jumping) leading onto a raised platform.
        world.set(30, 1, 10, STEP);
        world.set(31, 2, 10, STEP);
        world.set(32, 3, 10, STEP);
        for z in 8..13 {
            for x in 33..38 {
                world.set(x, 3, z, STEP);
            }
        }

        world
    }
}
