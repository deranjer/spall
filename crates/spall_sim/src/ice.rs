//! Winter ice on settled shallow water (ENG-268): the settings, the freeze
//! planner and the crack roll. [`crate::Simulation`] drives them each tick
//! (`sim/ice.rs`).
//!
//! Ice is ordinary terrain. It forms as server-authored [`BoxReplace`] edits
//! (air to ice) through the normal commit pipeline, so it replicates, persists
//! and can be dug like any cell. It sits *on* the water: the lowest ice cell is
//! the first cell wholly above the solver's surface, so freezing never moves
//! water, and the water under it keeps flowing in the closed span the ice caps.
//! A placed cell that touches no solid terrain would split off as a loose
//! body, so the plan keeps only ice connected to the shore and hands the boxes
//! out in layers: each layer touches terrain or a layer before it.

use std::collections::{HashMap, HashSet};

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{Sample, Volume};

use crate::intent::BoxReplace;

/// Every ice threshold and rate, in one place. The game picks the materials;
/// [`Self::new`] fills the rest with the shipped values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IceSettings {
    pub ice: MaterialId,
    /// What ice turns into at the warning crack.
    pub cracked: MaterialId,
    /// Water deeper than this (m) does not freeze.
    pub max_depth_m: f64,
    /// Water shallower than this (m) gets two cells of ice, deeper water one.
    pub thick_below_m: f64,
    /// A water region freezes once its published water has not changed for
    /// this long (s): the water has settled or the region sleeps.
    pub settle_s: f64,
    /// No ice forms within this distance (m, horizontal) of a player, so a
    /// swimmer is never sealed in.
    pub keep_clear_m: f64,
    /// Chance per second that one cell of ice over water cracks under a
    /// walking player, and two cells.
    pub crack_thin_per_s: f64,
    pub crack_thick_per_s: f64,
    /// Chance per second that cracked ice breaks under a walking player.
    pub break_per_s: f64,
    /// Every rate above is multiplied by this while the player sprints.
    pub sprint_factor: f64,
    /// Half-width (cells) of the cracked patch a warning leaves under the feet.
    pub crack_radius_cells: i64,
    /// Half-width (cells) of the hole a break opens around the feet.
    pub break_radius_cells: i64,
}

impl IceSettings {
    pub fn new(ice: MaterialId, cracked: MaterialId) -> Self {
        Self {
            ice,
            cracked,
            max_depth_m: 1.0,
            thick_below_m: 0.5,
            settle_s: 20.0,
            keep_clear_m: 1.5,
            crack_thin_per_s: 0.12,
            crack_thick_per_s: 0.02,
            break_per_s: 0.5,
            sprint_factor: 4.0,
            crack_radius_cells: 2,
            break_radius_cells: 4,
        }
    }

    /// Ice cells a column of water `depth_m` deep gets: 0 when it is too deep.
    pub fn thickness_cells(&self, depth_m: f64) -> i64 {
        if !(depth_m > 0.0) || depth_m > self.max_depth_m {
            0
        } else if depth_m < self.thick_below_m {
            2
        } else {
            1
        }
    }

    /// Chance per second of the next crack step under a player standing on
    /// `ice_cells` cells of ice over water: the warning crack on sound ice, the
    /// break on `cracked` ice.
    pub fn crack_rate_per_s(&self, ice_cells: u32, cracked: bool, sprinting: bool) -> f64 {
        let walking = if cracked {
            self.break_per_s
        } else {
            match ice_cells {
                0 => 0.0,
                1 => self.crack_thin_per_s,
                2 => self.crack_thick_per_s,
                _ => 0.0,
            }
        };
        walking * if sprinting { self.sprint_factor } else { 1.0 }
    }

    /// Whether the next crack step happens this tick. Deterministic in the
    /// seed, tick, player and cell, so a replay in the same binary repeats it.
    pub fn crack_roll(&self, roll: CrackRoll, dt_s: f64) -> bool {
        let rate = self.crack_rate_per_s(roll.ice_cells, roll.cracked, roll.sprinting);
        let chance = 1.0 - (-rate * dt_s).exp();
        unit_hash(&[
            roll.seed,
            roll.tick,
            roll.player,
            roll.cell.x as u64,
            roll.cell.y as u64,
            roll.cell.z as u64,
        ]) < chance
    }
}

/// The inputs of one [`IceSettings::crack_roll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrackRoll {
    pub seed: u64,
    pub tick: u64,
    pub player: u64,
    /// The ice cell under the feet.
    pub cell: GlobalCell,
    /// Ice cells stacked there over water.
    pub ice_cells: u32,
    pub cracked: bool,
    pub sprinting: bool,
}

/// A uniform value in `[0, 1)` from `words` (splitmix64 chain).
fn unit_hash(words: &[u64]) -> f64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    for &w in words {
        h ^= w;
        h = h.wrapping_add(0x9E37_79B9_7F4A_7C15);
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
    }
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// One terrain column of settled water open to the sky that should freeze:
/// ice fills `cells` cells from `bottom_y` up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IceColumn {
    pub x: i64,
    pub z: i64,
    pub bottom_y: i64,
    pub cells: i64,
}

fn solid(volume: &Volume, cell: GlobalCell) -> bool {
    matches!(volume.sample(cell), Ok(Sample::Filled(_)))
}

fn air(volume: &Volume, cell: GlobalCell) -> bool {
    matches!(volume.sample(cell), Ok(Sample::Empty { .. }))
}

fn faces(c: GlobalCell) -> [GlobalCell; 6] {
    [
        GlobalCell::new(c.x + 1, c.y, c.z),
        GlobalCell::new(c.x - 1, c.y, c.z),
        GlobalCell::new(c.x, c.y + 1, c.z),
        GlobalCell::new(c.x, c.y - 1, c.z),
        GlobalCell::new(c.x, c.y, c.z + 1),
        GlobalCell::new(c.x, c.y, c.z - 1),
    ]
}

/// The ice for `columns` as air-to-`ice` boxes, in layers to submit one after
/// another (each layer touches terrain or an earlier layer). Besides the
/// columns' own cells, a one-cell crust covers level shore cells beside the
/// bottom ice layer (inside `bounds`, `[x0, z0, x1, z1]` inclusive), so a sheet
/// on a gentle shore rests on the ground. Ice not connected to solid terrain
/// is left out.
pub fn plan_freeze(
    volume: &Volume,
    columns: &[IceColumn],
    bounds: [i64; 4],
    ice: MaterialId,
) -> Vec<Vec<BoxReplace>> {
    let inside =
        |x: i64, z: i64| x >= bounds[0] && x <= bounds[2] && z >= bounds[1] && z <= bounds[3];
    let water: HashSet<(i64, i64)> = columns.iter().map(|c| (c.x, c.z)).collect();
    let mut wanted: HashSet<GlobalCell> = HashSet::new();
    for column in columns {
        for y in column.bottom_y..column.bottom_y + column.cells {
            let cell = GlobalCell::new(column.x, y, column.z);
            if air(volume, cell) {
                wanted.insert(cell);
            }
        }
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (x, z) = (column.x + dx, column.z + dz);
            let crust = GlobalCell::new(x, column.bottom_y, z);
            if inside(x, z)
                && !water.contains(&(x, z))
                && air(volume, crust)
                && solid(volume, GlobalCell::new(x, column.bottom_y - 1, z))
            {
                wanted.insert(crust);
            }
        }
    }
    // Keep what is connected to solid terrain.
    let mut kept: HashSet<GlobalCell> = HashSet::new();
    let mut stack: Vec<GlobalCell> = wanted
        .iter()
        .copied()
        .filter(|&c| faces(c).iter().any(|&n| solid(volume, n)))
        .collect();
    while let Some(cell) = stack.pop() {
        if !kept.insert(cell) {
            continue;
        }
        stack.extend(
            faces(cell)
                .into_iter()
                .filter(|n| wanted.contains(n) && !kept.contains(n)),
        );
    }
    let boxes = cover_with_boxes(volume, &kept, ice);
    support_layers(volume, boxes)
}

/// A box and the wanted cells it fills.
struct Planned {
    replace: BoxReplace,
    cells: Vec<GlobalCell>,
    holds_solid: bool,
}

/// Greedy rectangles per layer over the wanted cells, also spanning solid
/// cells (an air-to-ice box leaves them alone) but never an air cell that is
/// not wanted.
fn cover_with_boxes(volume: &Volume, kept: &HashSet<GlobalCell>, ice: MaterialId) -> Vec<Planned> {
    let mut by_layer: HashMap<i64, Vec<GlobalCell>> = HashMap::new();
    for &cell in kept {
        by_layer.entry(cell.y).or_default().push(cell);
    }
    let mut layers: Vec<i64> = by_layer.keys().copied().collect();
    layers.sort_unstable();
    let mut planned = Vec::new();
    for y in layers {
        let mut cells = by_layer.remove(&y).unwrap_or_default();
        cells.sort_by_key(|c| (c.z, c.x));
        let mut covered: HashSet<(i64, i64)> = HashSet::new();
        let open = |x: i64, z: i64, covered: &HashSet<(i64, i64)>| {
            let cell = GlobalCell::new(x, y, z);
            if kept.contains(&cell) {
                !covered.contains(&(x, z))
            } else {
                solid(volume, cell)
            }
        };
        for start in &cells {
            if covered.contains(&(start.x, start.z)) {
                continue;
            }
            let mut w = 1;
            while w < 32 && open(start.x + w, start.z, &covered) {
                w += 1;
            }
            let mut h = 1;
            while h < 32 && (0..w).all(|dx| open(start.x + dx, start.z + h, &covered)) {
                h += 1;
            }
            let mut filled = Vec::new();
            let mut holds_solid = false;
            for dz in 0..h {
                for dx in 0..w {
                    let cell = GlobalCell::new(start.x + dx, y, start.z + dz);
                    if kept.contains(&cell) {
                        covered.insert((cell.x, cell.z));
                        filled.push(cell);
                    } else {
                        holds_solid = true;
                    }
                }
            }
            planned.push(Planned {
                replace: BoxReplace {
                    min: *start,
                    size: [w as u8, 1, h as u8],
                    from: [MaterialId::AIR, MaterialId::AIR],
                    to: ice,
                },
                cells: filled,
                holds_solid,
            });
        }
    }
    planned
}

/// Orders boxes so each touches solid terrain, or a box of an earlier layer,
/// when it commits. A box holding a solid cell touches it already: every run
/// of wanted cells in a rectangle borders its solid cells or is the whole box.
fn support_layers(volume: &Volume, mut boxes: Vec<Planned>) -> Vec<Vec<BoxReplace>> {
    let mut placed: HashSet<GlobalCell> = HashSet::new();
    let mut layers = Vec::new();
    while !boxes.is_empty() {
        let (ready, rest): (Vec<Planned>, Vec<Planned>) = boxes.into_iter().partition(|b| {
            b.holds_solid
                || b.cells.iter().any(|&c| {
                    faces(c)
                        .iter()
                        .any(|&n| placed.contains(&n) || solid(volume, n))
                })
        });
        if ready.is_empty() {
            break;
        }
        for b in &ready {
            placed.extend(b.cells.iter().copied());
        }
        layers.push(ready.into_iter().map(|b| b.replace).collect());
        boxes = rest;
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrickCoord, CellSizeCode, Revision, VolumeId};
    use spall_voxel::{Brick, EditPlan};

    const STONE: MaterialId = MaterialId(1);
    const ICE: MaterialId = MaterialId(9);

    /// Floor at y < 4 and a 10 x 10 pit at y = 2..4 (x, z in 4..14), sky above.
    fn pond() -> Volume {
        let mut v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        v.insert_brick(
            BrickCoord::new(0, 0, 0),
            Brick::uniform(MaterialId::AIR, Revision(1)),
        )
        .unwrap();
        let mut plan = EditPlan::new(v.id());
        for z in 0..32 {
            for x in 0..32 {
                let pit = (4..14).contains(&x) && (4..14).contains(&z);
                for y in 0..if pit { 2 } else { 4 } {
                    plan.set(GlobalCell::new(x, y, z), STONE);
                }
            }
        }
        v.apply_edit(&plan).unwrap();
        v
    }

    fn apply(v: &mut Volume, layers: &[Vec<BoxReplace>]) -> usize {
        let mut n = 0;
        for b in layers.iter().flatten() {
            let plan = b.plan(v);
            n += plan.writes.len();
            v.apply_edit(&plan).unwrap();
        }
        n
    }

    #[test]
    fn the_plan_fills_exactly_the_wanted_cells_and_a_shore_crust() {
        let mut v = pond();
        // Water to y = 4 (two cells deep): ice goes at y = 4, one cell.
        let columns: Vec<IceColumn> = (4..14)
            .flat_map(|z| {
                (4..14).map(move |x| IceColumn {
                    x,
                    z,
                    bottom_y: 4,
                    cells: 1,
                })
            })
            .collect();
        let layers = plan_freeze(&v, &columns, [0, 0, 31, 31], ICE);
        let written = apply(&mut v, &layers);
        // 100 ice cells over water plus a crust ring of 40 shore cells.
        assert_eq!(written, 140);
        for c in &columns {
            assert_eq!(
                v.sample(GlobalCell::new(c.x, 4, c.z)),
                Ok(Sample::Filled(ICE))
            );
            assert!(air(&v, GlobalCell::new(c.x, 3, c.z)), "water stays clear");
        }
        assert_eq!(
            v.sample(GlobalCell::new(3, 4, 7)),
            Ok(Sample::Filled(ICE)),
            "crust"
        );
        assert!(matches!(
            v.sample(GlobalCell::new(2, 4, 7)),
            Ok(Sample::Empty { .. })
        ));
        assert!(
            matches!(v.sample(GlobalCell::new(3, 4, 3)), Ok(Sample::Empty { .. })),
            "no corner crust"
        );
    }

    #[test]
    fn layers_only_ever_touch_terrain_or_earlier_layers() {
        let mut v = pond();
        let columns: Vec<IceColumn> = (4..14)
            .flat_map(|z| {
                (4..14).map(move |x| IceColumn {
                    x,
                    z,
                    bottom_y: 4,
                    cells: 2,
                })
            })
            .collect();
        let layers = plan_freeze(&v, &columns, [0, 0, 31, 31], ICE);
        assert!(
            layers.len() >= 2,
            "the second ice layer waits for the first"
        );
        for layer in &layers {
            for b in layer {
                let cells = b.cells(&v);
                assert!(
                    cells
                        .iter()
                        .any(|&c| faces(c).iter().any(|&n| solid(&v, n))),
                    "box {b:?} would float"
                );
            }
            apply(&mut v, std::slice::from_ref(layer));
        }
    }

    #[test]
    fn ice_out_of_reach_of_the_shore_is_left_out() {
        let v = pond();
        // One column in the middle with no wanted neighbours: nothing holds it.
        let columns = [IceColumn {
            x: 8,
            z: 8,
            bottom_y: 4,
            cells: 1,
        }];
        assert!(plan_freeze(&v, &columns, [0, 0, 31, 31], ICE).is_empty());
    }

    #[test]
    fn sprinting_and_thin_ice_raise_the_crack_rate() {
        let s = IceSettings::new(ICE, MaterialId(10));
        assert!(s.crack_rate_per_s(1, false, false) > s.crack_rate_per_s(2, false, false));
        assert!(s.crack_rate_per_s(1, false, true) > s.crack_rate_per_s(1, false, false));
        assert_eq!(s.crack_rate_per_s(3, false, true), 0.0);
        assert_eq!(s.thickness_cells(0.2), 2);
        assert_eq!(s.thickness_cells(0.7), 1);
        assert_eq!(s.thickness_cells(1.5), 0);
    }

    /// Runs the crack state machine the server runs (sound ice, then cracked,
    /// then broken) for `ticks` ticks with one roll per tick.
    fn breaks_within(s: &IceSettings, seed: u64, ticks: u64, sprinting: bool) -> bool {
        let mut cracked = false;
        let cell = GlobalCell::new(5, 4, 5);
        for tick in 0..ticks {
            let roll = CrackRoll {
                seed,
                tick,
                player: 7,
                cell,
                ice_cells: 1,
                cracked,
                sprinting,
            };
            if s.crack_roll(roll, 1.0 / 60.0) {
                if cracked {
                    return true;
                }
                cracked = true;
            }
        }
        false
    }

    #[test]
    fn sprinting_on_thin_ice_breaks_it_more_often_over_many_seeds() {
        let s = IceSettings::new(ICE, MaterialId(10));
        let trials = 400;
        let count = |sprinting| {
            (0..trials)
                .filter(|&seed| breaks_within(&s, seed, 5 * 60, sprinting))
                .count()
        };
        let (walk, sprint) = (count(false), count(true));
        // Standing on the crack: about 30% walking and 88% sprinting in 5 s.
        assert!(
            sprint > 2 * walk,
            "sprint {sprint} vs walk {walk} of {trials}"
        );
        assert!(
            walk > 0 && (sprint as u64) < trials,
            "both are chances, not certainties"
        );
    }
}
