//! Ecological placement of living plants for a new world, and the spatial
//! index the establishment rules use.
//!
//! Trees are placed by dart throwing in a fixed pseudo-random order, so a
//! world is the same for the same seed. What varies is the *spacing*: dense
//! canopy biomes pack trees at about 0.85 of a species' spacing, open ground
//! at twice that, and glade noise removes whole clearings. Each stand
//! (a patch of about 40 m) has one dominant species that takes most of its
//! trees, so stands, edges between them and mixed ecotones appear without any
//! per-biome rule. The understory (the interactive ground plants) is thicker
//! in the shade of trees and thinner in the open.
//!
//! Nothing here reads wall-clock time or global state: the same
//! `(seed, ColumnMap)` yields the same plants.
use std::collections::HashMap;

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{EditPlan, Sample, Volume};
use spall_worldgen::{Biome, ColumnMap};

use crate::living::{LivingState, MAX_GROUND, MAX_TREES, Species, cell, hash, moisture};

const BUCKET: i64 = 16;
/// Candidate spacing in cells: 1 m for trees, 1.25 m for the understory.
const TREE_STRIDE: i64 = 4;
const GROUND_STRIDE: i64 = 5;
/// Size of a stand and of a glade, in cells.
const STAND: i64 = 160;
const GLADE: f64 = 56.0;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub root: [i64; 3],
    pub tree: bool,
}

/// Where the live organisms stand, bucketed by 16-cell squares, so spacing and
/// footprint checks cost a few buckets instead of a scan of every plant. It is
/// derived from the organisms and never saved; two states are equal whatever
/// their indexes hold.
#[derive(Debug, Default)]
pub(crate) struct Spatial {
    cells: HashMap<(i64, i64), Vec<Entry>>,
    pub trees: usize,
    pub ground: usize,
    /// How many organisms the index was built for; a mismatch means someone
    /// changed `organisms` directly and the index is rebuilt before use.
    built: usize,
}

/// A copy of the state starts without an index and rebuilds it if it ever
/// needs one (a keyframe's copy never does), so copying costs nothing here.
impl Clone for Spatial {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Spatial {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for Spatial {}

impl Spatial {
    pub(crate) fn rebuild(&mut self, organisms: &[crate::living::Organism]) {
        self.cells.clear();
        self.trees = 0;
        self.ground = 0;
        for p in organisms.iter().filter(|p| p.alive) {
            self.insert(Entry {
                root: p.root,
                tree: !p.wood.is_empty(),
            });
        }
        self.built = organisms.len();
    }
    fn insert(&mut self, e: Entry) {
        self.cells
            .entry((e.root[0].div_euclid(BUCKET), e.root[2].div_euclid(BUCKET)))
            .or_default()
            .push(e);
        if e.tree {
            self.trees += 1;
        } else {
            self.ground += 1;
        }
    }
    /// Records a newly established organism.
    pub(crate) fn add(&mut self, root: [i64; 3], tree: bool) {
        self.insert(Entry { root, tree });
        self.built += 1;
    }
    pub(crate) fn is_current(&self, organisms: usize) -> bool {
        self.built == organisms
    }
    /// Whether any entry within `radius` cells (square) of `(x, z)` satisfies `f`.
    pub(crate) fn any_near(&self, x: i64, z: i64, radius: i64, f: impl Fn(&Entry) -> bool) -> bool {
        let (x0, x1) = (
            (x - radius).div_euclid(BUCKET),
            (x + radius).div_euclid(BUCKET),
        );
        let (z0, z1) = (
            (z - radius).div_euclid(BUCKET),
            (z + radius).div_euclid(BUCKET),
        );
        for bz in z0..=z1 {
            for bx in x0..=x1 {
                if let Some(bucket) = self.cells.get(&(bx, bz))
                    && bucket.iter().any(&f)
                {
                    return true;
                }
            }
        }
        false
    }
}

/// How densely trees stand in a biome, `0..=1`.
fn canopy(biome: Biome) -> f64 {
    match biome {
        Biome::PineForest => 1.0,
        Biome::TemperateForest => 0.95,
        Biome::BorealForest => 0.85,
        Biome::Riparian => 0.7,
        Biome::Swamp => 0.45,
        Biome::Meadow => 0.18,
        Biome::Alpine => 0.12,
        Biome::Steppe => 0.06,
        Biome::Beach | Biome::Scree => 0.03,
        Biome::Desert => 0.01,
        Biome::Snow | Biome::Ocean => 0.0,
    }
}

/// How readily the understory (living ground plants) takes hold, `0..=1`.
fn understory(biome: Biome) -> f64 {
    match biome {
        Biome::TemperateForest
        | Biome::BorealForest
        | Biome::PineForest
        | Biome::Swamp
        | Biome::Riparian => 0.9,
        Biome::Meadow => 0.8,
        Biome::Alpine | Biome::Steppe => 0.45,
        Biome::Ocean => 0.6,
        Biome::Beach => 0.2,
        Biome::Desert => 0.12,
        Biome::Scree => 0.1,
        Biome::Snow => 0.05,
    }
}

fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth value noise in `0..1`, one lattice point per `scale` cells.
fn value_noise(seed: u64, x: i64, z: i64, scale: f64) -> f64 {
    let (fx, fz) = (x as f64 / scale, z as f64 / scale);
    let (ix, iz) = (fx.floor(), fz.floor());
    let (tx, tz) = (fx - ix, fz - iz);
    let (sx, sz) = (tx * tx * (3.0 - 2.0 * tx), tz * tz * (3.0 - 2.0 * tz));
    let at = |dx: i64, dz: i64| {
        unit(hash(
            seed ^ ((ix as i64 + dx) as u64).rotate_left(21) ^ ((iz as i64 + dz) as u64),
        ))
    };
    let top = at(0, 0) + (at(1, 0) - at(0, 0)) * sx;
    let bottom = at(0, 1) + (at(1, 1) - at(0, 1)) * sx;
    top + (bottom - top) * sz
}

impl LivingState {
    /// Rebuilds the spatial index if the organisms changed behind its back.
    pub(crate) fn sync_index(&mut self) {
        if !self.spatial.is_current(self.organisms.len()) {
            self.spatial.rebuild(&self.organisms);
        }
    }

    /// Fills a new world: trees, then the understory.
    pub(crate) fn populate(
        &mut self,
        columns: &ColumnMap,
        terrain: &mut Volume,
    ) -> Result<(), String> {
        self.spatial.rebuild(&self.organisms);
        let mut plan = EditPlan::new(terrain.id());
        self.place_trees(columns, terrain, &mut plan);
        terrain.apply_edit(&plan).map_err(|e| e.to_string())?;
        self.place_understory(columns, terrain);
        Ok(())
    }

    /// Species that could stand on `(x, z)`: biome, wetness, soil and water depth.
    fn options_at(
        &self,
        columns: &ColumnMap,
        terrain: &Volume,
        root: [i64; 3],
        tree: bool,
    ) -> Vec<u16> {
        let Ok(Sample::Filled(soil)) =
            terrain.sample(GlobalCell::new(root[0], root[1] - 1, root[2]))
        else {
            return Vec::new();
        };
        let biome = columns.biome(root[0], root[2]);
        let wet = moisture(columns, root[0], root[2]);
        let depth = columns.water_depth(root[0], root[2]).clamp(0, 255) as u8;
        self.species
            .iter()
            .filter(|s| s.tree == tree && s.accepts_for_placement(biome, wet, soil.0, depth))
            .map(|s| s.id)
            .collect()
    }

    fn place_trees(&mut self, columns: &ColumnMap, terrain: &mut Volume, plan: &mut EditPlan) {
        let size = i64::from(self.size);
        let mut candidates = Vec::new();
        for z in (33..size - 33).step_by(TREE_STRIDE as usize) {
            for x in (33..size - 33).step_by(TREE_STRIDE as usize) {
                let h = hash(self.seed ^ 0x7ee5 ^ (x as u64).rotate_left(21) ^ z as u64);
                let x = (x + (h % TREE_STRIDE as u64) as i64).min(size - 34);
                let z = (z + (h.rotate_left(17) % TREE_STRIDE as u64) as i64).min(size - 34);
                let c = canopy(columns.biome(x, z));
                if c > 0.0 && !columns.is_water(x, z) {
                    candidates.push((h, x, z, c));
                }
            }
        }
        candidates.sort_unstable_by_key(|c| c.0);
        for (key, x, z, c) in candidates {
            if self.spatial.trees >= MAX_TREES {
                break;
            }
            // Glades: whole clearings where the canopy thins out or is absent.
            let glade = value_noise(self.seed ^ 0x61ade, x, z, GLADE);
            let density = c * (0.25 + 0.75 * (glade * 1.6).min(1.0));
            if density < 0.08 {
                continue;
            }
            let root = [x, i64::from(columns.height(x, z)) + 1, z];
            let options = self.options_at(columns, terrain, root, true);
            if options.is_empty() {
                continue;
            }
            // The stand's dominant species takes most of its trees.
            let stand = hash(
                self.seed ^ 0x57a4d ^ ((x / STAND) as u64).rotate_left(25) ^ (z / STAND) as u64,
            );
            // The option the stand ranks highest, so the choice does not shift
            // with the local list of what could grow.
            let dominant = options
                .iter()
                .copied()
                .max_by_key(|id| hash(stand ^ u64::from(*id).rotate_left(29)))
                .unwrap_or(options[0]);
            let sid = if key % 100 < 72 {
                dominant
            } else {
                options[((key / 101) % options.len() as u64) as usize]
            };
            // Open ground spaces trees out; a dense canopy packs them.
            let scale = 2.2 - 1.35 * density.sqrt();
            let Some(spacing) = self
                .species
                .iter()
                .find(|s| s.id == sid)
                .map(|s| (f64::from(s.spacing) * scale).round() as i64)
            else {
                continue;
            };
            if let Some(index) =
                self.establish_with(columns, terrain, sid, root, Some(spacing.max(3)), i64::MAX)
            {
                let p = &mut self.organisms[index];
                // Mixed ages make initial worlds wooded immediately; the rest grow at runtime.
                if key % 5 != 0 {
                    p.committed = p.wood.len() as u16;
                    p.age_ms = 600_000;
                    let wood = self
                        .species
                        .iter()
                        .find(|s| s.id == sid)
                        .map_or(0, |s| s.wood);
                    for &at in p.wood.iter() {
                        plan.set(cell(at), MaterialId(wood));
                    }
                }
            }
        }
    }

    fn place_understory(&mut self, columns: &ColumnMap, terrain: &Volume) {
        let size = i64::from(self.size);
        let mut candidates = Vec::new();
        for z in (33..size - 33).step_by(GROUND_STRIDE as usize) {
            for x in (33..size - 33).step_by(GROUND_STRIDE as usize) {
                let h = hash(self.seed ^ 0x9d0 ^ (x as u64).rotate_left(21) ^ z as u64);
                let x = (x + (h % GROUND_STRIDE as u64) as i64).min(size - 34);
                let z = (z + (h.rotate_left(17) % GROUND_STRIDE as u64) as i64).min(size - 34);
                let u = understory(columns.biome(x, z));
                // Roll first: most candidates need no terrain lookup at all.
                if unit(h.rotate_left(33)) < 0.075 * u {
                    candidates.push((h, x, z));
                }
            }
        }
        candidates.sort_unstable_by_key(|c| c.0);
        for (key, x, z) in candidates {
            if self.spatial.ground >= MAX_GROUND {
                break;
            }
            let root = [x, i64::from(columns.height(x, z)) + 1, z];
            // Shade: plants gather under trees and thin out in the open.
            let shaded = self.spatial.any_near(x, z, 7, |e| {
                e.tree && (e.root[0] - x).pow(2) + (e.root[2] - z).pow(2) <= 49
            });
            if !shaded && key % 100 < 45 {
                continue;
            }
            let options = self.options_at(columns, terrain, root, false);
            if options.is_empty() {
                continue;
            }
            let sid = options[((key / 101) % options.len() as u64) as usize];
            self.establish(columns, terrain, sid, root);
        }
    }
}

impl Species {
    fn accepts_for_placement(&self, biome: Biome, moisture: u8, soil: u16, depth: u8) -> bool {
        self.accepts_depth(depth) && self.accepts(biome, moisture, soil)
    }
}
