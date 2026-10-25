//! Loose sticks, logs and rocks scattered over a generated world (ENG-258).
//!
//! Sticks and logs lie near living trees, so they are denser in forest; rocks
//! lie mostly along river and lake banks with a sparse scatter elsewhere.
//! Placement is a pure function of `(seed, generated world, settings)`: the
//! same inputs give the same list. Each item rests on the top face of the
//! ground cell with open air above. Snow is a render layer over terrain, not
//! voxels, so it never moves or buries an item; it can only hide one visually.

use spall_core::GlobalCell;
use spall_server::InitialWorldItem;
use spall_voxel::Sample;

use crate::game::items;
use crate::worldgen_scene::GeneratedScene;

/// Edge of one cell in metres.
const CELL_M: f64 = 0.25;
/// Cells of clear air required above a resting item.
const HEADROOM_CELLS: i64 = 3;

/// How much of each item the world gets. These are the only knobs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterSettings {
    /// Expected sticks lying around each living tree.
    pub sticks_per_tree: f32,
    /// Expected logs lying around each living tree.
    pub logs_per_tree: f32,
    /// Chance a dry column within [`Self::bank_cells`] of water holds a rock.
    pub rock_per_bank_column: f64,
    /// Chance any other dry column holds a rock.
    pub rock_per_other_column: f64,
    /// Distance from water, in cells, that counts as the bank.
    pub bank_cells: u8,
    /// Farthest a stick or log lies from its tree, in cells.
    pub tree_radius_cells: i64,
}

impl Default for ScatterSettings {
    fn default() -> Self {
        Self {
            sticks_per_tree: 0.25,
            logs_per_tree: 0.05,
            rock_per_bank_column: 5.0e-4,
            rock_per_other_column: 1.0e-5,
            bank_cells: 12,
            tree_radius_cells: 24,
        }
    }
}

fn mix(seed: u64, salt: u64, a: u64, b: u64) -> u64 {
    let mut z = seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for v in [a, b] {
        z = z.wrapping_add(v).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z ^= z >> 31;
    }
    z ^= z >> 30;
    z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 29)
}

fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// A whole count with the given expectation, from one hash.
fn count(expected: f32, h: u64) -> u32 {
    let whole = expected.floor();
    whole as u32 + u32::from(unit(h) < f64::from(expected - whole))
}

/// The resting item at column `(x, z)`, or `None` if it is water, border, or
/// not open ground.
fn rest(scene: &GeneratedScene, kind: u16, x: i64, z: i64, h: u64) -> Option<InitialWorldItem> {
    let world = scene.world();
    let columns = &world.columns;
    if columns.in_border(x, z) || columns.water_surface(x, z) > 0 {
        return None;
    }
    let y = i64::from(columns.height(x, z));
    let solid = matches!(
        world.terrain.sample(GlobalCell::new(x, y, z)),
        Ok(Sample::Filled(_))
    );
    let open = (1..=HEADROOM_CELLS).all(|up| {
        matches!(
            world.terrain.sample(GlobalCell::new(x, y + up, z)),
            Ok(Sample::Empty { .. })
        )
    });
    if !solid || !open {
        return None;
    }
    let jitter = |bits: u64| (unit(bits) - 0.5) * 0.2;
    Some(InitialWorldItem {
        kind,
        translation_m: [
            (x as f64 + 0.5) * CELL_M + jitter(h),
            (y + 1) as f64 * CELL_M,
            (z as f64 + 0.5) * CELL_M + jitter(h >> 20),
        ],
        yaw_rad: (unit(h >> 40) * std::f64::consts::TAU) as f32,
    })
}

impl GeneratedScene {
    /// The sticks, logs and rocks a new world starts with.
    pub fn scatter_items(&self, seed: u64, settings: &ScatterSettings) -> Vec<InitialWorldItem> {
        let mut out = Vec::new();
        let stick = items::STICK.0;
        let log = items::WOOD_LOG.0;
        let stone = items::STONE_CHUNK.0;

        let state = self.vegetation();
        let is_tree = |id: u16| state.species.iter().any(|s| s.id == id && s.tree);
        let r = settings.tree_radius_cells;
        for tree in state
            .organisms
            .iter()
            .filter(|t| t.alive && is_tree(t.species))
        {
            for (kind, expected, salt) in [
                (stick, settings.sticks_per_tree, 1),
                (log, settings.logs_per_tree, 2),
            ] {
                let n = count(expected, mix(seed, salt, tree.id, 0));
                for k in 0..n {
                    let h = mix(seed, salt + 10, tree.id, u64::from(k));
                    let dx = (h % (2 * r as u64 + 1)) as i64 - r;
                    let dz = ((h >> 16) % (2 * r as u64 + 1)) as i64 - r;
                    if let Some(item) =
                        rest(self, kind, tree.root[0] + dx, tree.root[2] + dz, h >> 8)
                    {
                        out.push(item);
                    }
                }
            }
        }

        let columns = &self.world().columns;
        let size = i64::from(columns.size());
        for z in 0..size {
            for x in 0..size {
                let near = columns.river_dist(x, z) <= settings.bank_cells;
                let p = if near {
                    settings.rock_per_bank_column
                } else {
                    settings.rock_per_other_column
                };
                let h = mix(seed, 3, x as u64, z as u64);
                if unit(h) < p
                    && let Some(item) = rest(self, stone, x, z, h >> 4)
                {
                    out.push(item);
                }
            }
        }
        out
    }
}
