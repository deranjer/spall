//! Versioned living vegetation. Authored species belong to the game; this CPU
//! module owns deterministic establishment, reproduction and retained wood paths.
use crate::cover::{
    Cleared, CoverField, CoverSpecies, FULL_AFTER_S, MAX_CLEARED, MAX_COVER_SPECIES, is_carpet_form,
};
use serde::{Deserialize, Serialize};
use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{EditPlan, Sample, Volume};
use spall_worldgen::{Biome, ColumnMap};

/// Version 2 moved the ground carpet out of the organisms: grass, sedge,
/// clover and tussock are stateless cover now ([`crate::cover`]).
pub const VERSION: u16 = 4;
/// Sized for a 1 km world: about 10 to 20 thousand trees and the living
/// understory (cover is stateless and not counted here).
pub const MAX_TREES: usize = 24_576;
pub const MAX_GROUND: usize = 32_768;
/// Every biome's bit (thirteen).
pub const ALL_BIOMES: u16 = 0x1FFF;
pub const MAX_SPECIES: usize = 128;
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
pub const PALETTE: [[u8; 3]; 32] = [
    [57, 105, 32],
    [82, 133, 41],
    [39, 85, 48],
    [123, 155, 61],
    [210, 143, 28],
    [183, 69, 30],
    [146, 42, 38],
    [124, 97, 53],
    [155, 150, 109],
    [207, 205, 175],
    [223, 219, 187],
    [150, 81, 153],
    [204, 114, 151],
    [110, 157, 106],
    [78, 124, 118],
    [177, 184, 110],
    // Appended with ground-cover variety; indexes above never change.
    [235, 200, 52],
    [240, 240, 232],
    [96, 92, 190],
    [101, 52, 38],
    [96, 150, 64],
    [150, 165, 140],
    [170, 170, 160],
    [70, 52, 34],
    [214, 176, 120],
    [60, 110, 70],
    [190, 120, 60],
    [228, 140, 160],
    [120, 130, 60],
    [88, 70, 110],
    [200, 190, 130],
    [48, 80, 60],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Season {
    Spring = 0,
    Summer = 1,
    Autumn = 2,
    Winter = 3,
}
impl Season {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "spring" => Some(Self::Spring),
            "summer" => Some(Self::Summer),
            "autumn" | "fall" => Some(Self::Autumn),
            "winter" => Some(Self::Winter),
            _ => None,
        }
    }
    pub fn at(start: Self, time_ms: u64, season_ms: u64) -> Self {
        [Self::Spring, Self::Summer, Self::Autumn, Self::Winter]
            [(start as usize + (time_ms / season_ms.max(1)) as usize % 4) % 4]
    }
}

/// Fixed DTO codes: form 0 broad crown, 1 cone, 2 column, 3 weeping,
/// 4 umbrella, 5 fan; ground forms 6 grass, 7 reed, 8 fern, 9 sedge,
/// 10 flowering stalk, 11 rosette, 12 cushion, 13 scrub; 14 to 17 underwater;
/// 18 to 21 low cover; 22 shrub, 23 soft bush, 24 tall herb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Species {
    pub id: u16,
    pub tree: bool,
    /// Bit `1 << biome` for each of the twelve biomes the species grows in.
    pub biomes: u16,
    pub soils: [u16; 4],
    pub moisture: [u8; 2],
    /// Water depth in cells the plant grows in: `[0, 0]` is dry land; anything
    /// else lives under water (kelp, coral) and is never a tree.
    pub depth: [u8; 2],
    pub wood: u16,
    pub trunk_width: u8,
    pub form: u8,
    pub height: u16,
    pub spread: u16,
    pub spacing: u16,
    pub evergreen: bool,
    pub colours: [u8; 4],
    pub flower: u8,
    pub growth_ms: u64,
    pub seed_ms: u64,
}
impl Species {
    pub(crate) fn accepts_depth(&self, depth: u8) -> bool {
        (self.depth[0]..=self.depth[1]).contains(&depth)
    }
    pub(crate) fn accepts(&self, biome: Biome, moisture: u8, soil: u16) -> bool {
        self.biomes & (1 << biome as u16) != 0
            && self.soils.contains(&soil)
            && (self.moisture[0]..=self.moisture[1]).contains(&moisture)
    }
}
/// A living plant. Saved without its skeleton (see [`crate::codec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organism {
    pub id: u64,
    pub species: u16,
    pub root: [i64; 3],
    pub age_ms: u64,
    /// Accumulated age during spring, summer and autumn only.
    pub growth_ms: u64,
    /// Active growing time since the last seed attempt.
    pub seed_growth_ms: u64,
    pub last_update_ms: u64,
    pub last_seed_ms: u64,
    pub alive: bool,
    pub biomass: u16,
    /// Parent-before-child, face-connected. Cuts permanently retire descendants.
    /// Shared and never changed after establishment, so copying the state for
    /// a keyframe does not copy every tree's wood.
    pub wood: std::sync::Arc<[[i64; 3]]>,
    pub parents: std::sync::Arc<[u16]>,
    /// Copy-on-write, like the skeleton: a keyframe's copy of the state shares it.
    pub removed: std::sync::Arc<Vec<bool>>,
    pub committed: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivingState {
    pub version: u16,
    pub seed: u64,
    pub size: u32,
    pub time_ms: u64,
    pub credit_ms: u64,
    pub start_season: Season,
    pub season_ms: u64,
    pub next_id: u64,
    pub cursor: u32,
    pub species: Vec<Species>,
    #[serde(with = "crate::codec")]
    pub organisms: Vec<Organism>,
    pub spawns: Vec<[i64; 3]>,
    /// The stateless ground cover this world carries, and the climate raster
    /// it reads. Nothing here ever changes after the world is made.
    pub cover: Vec<CoverSpecies>,
    pub field: CoverField,
    /// Columns whose cover was destroyed and when, sorted by `(x, z)`; a record
    /// is dropped once its cover has regrown in full.
    pub cleared: Vec<Cleared>,
    /// Which worldgen preset the world came from (0 showcase, 1 natural), so
    /// the server can rebuild the same climate columns the world was made
    /// from when it installs or restores this state.
    pub preset: u8,
    /// Where the organisms stand (derived, never saved).
    #[serde(skip)]
    pub(crate) spatial: crate::placement::Spatial,
}

/// Version 3 cleared-cover layout. `at_s` was a chronological timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ClearedV3 {
    x: u16,
    z: u16,
    at_s: u32,
}

/// Previous saved schema, retained solely for deterministic migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LivingStateV3 {
    version: u16,
    seed: u64,
    size: u32,
    time_ms: u64,
    credit_ms: u64,
    start_season: Season,
    season_ms: u64,
    next_id: u64,
    cursor: u32,
    species: Vec<Species>,
    #[serde(with = "crate::codec::v3")]
    organisms: Vec<Organism>,
    spawns: Vec<[i64; 3]>,
    cover: Vec<CoverSpecies>,
    field: CoverField,
    cleared: Vec<ClearedV3>,
    preset: u8,
}

fn active_prefix(start: Season, season_ms: u64, time_ms: u64) -> u64 {
    let length = season_ms.max(1);
    let elapsed_seasons = time_ms / length;
    let cycles = elapsed_seasons / 4;
    let remainder_seasons = elapsed_seasons % 4;
    let partial = time_ms % length;
    let mut active = cycles.saturating_mul(length).saturating_mul(3);
    for i in 0..remainder_seasons {
        let season = (start as usize + i as usize) % 4;
        if season != Season::Winter as usize {
            active = active.saturating_add(length);
        }
    }
    if (start as u64 + remainder_seasons) % 4 != Season::Winter as u64 {
        active = active.saturating_add(partial);
    }
    active
}

fn active_between(start: Season, season_ms: u64, from_ms: u64, to_ms: u64) -> u64 {
    active_prefix(start, season_ms, to_ms).saturating_sub(active_prefix(start, season_ms, from_ms))
}

fn upgrade_v3(old: LivingStateV3) -> LivingState {
    LivingState {
        version: VERSION,
        seed: old.seed,
        size: old.size,
        time_ms: old.time_ms,
        credit_ms: old.credit_ms,
        start_season: old.start_season,
        season_ms: old.season_ms,
        next_id: old.next_id,
        cursor: old.cursor,
        species: old.species,
        organisms: old.organisms,
        spawns: old.spawns,
        cover: old.cover,
        field: old.field,
        cleared: old
            .cleared
            .into_iter()
            .map(|c| Cleared {
                x: c.x,
                z: c.z,
                growth_ms: c.at_s.saturating_mul(1000),
            })
            .collect(),
        preset: old.preset,
        spatial: Default::default(),
    }
}

fn migrate_legacy_clocks(state: &mut LivingState) {
    for p in &mut state.organisms {
        let from = p.last_update_ms.saturating_sub(p.age_ms);
        p.growth_ms = if p.age_ms > state.time_ms {
            p.age_ms
        } else {
            active_between(state.start_season, state.season_ms, from, p.last_update_ms)
                .min(p.age_ms)
        };
        p.seed_growth_ms = active_between(
            state.start_season,
            state.season_ms,
            p.last_seed_ms,
            p.last_update_ms,
        );
    }
    for c in &mut state.cleared {
        let timestamp_ms = u64::from(c.growth_ms);
        c.growth_ms = active_between(
            state.start_season,
            state.season_ms,
            timestamp_ms,
            state.time_ms,
        )
        .min(u64::from(FULL_AFTER_S * 1000)) as u32;
    }
    state.cleared.retain(|c| c.growth_ms < FULL_AFTER_S * 1000);
}
/// A version 1 species: its `biomes` were a mask over the four vegetation
/// classes (meadow, alpine, swamp, desert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SpeciesV1 {
    id: u16,
    tree: bool,
    biomes: u8,
    soils: [u16; 4],
    moisture: [u8; 2],
    wood: u16,
    trunk_width: u8,
    form: u8,
    height: u16,
    spread: u16,
    spacing: u16,
    evergreen: bool,
    colours: [u8; 4],
    flower: u8,
    growth_ms: u64,
    seed_ms: u64,
}
impl SpeciesV1 {
    /// The twelve-biome mask a class mask stands for.
    fn widen(&self) -> u16 {
        let mut mask = 0u16;
        for (class, biomes) in [
            (
                0u8,
                &[Biome::Meadow, Biome::TemperateForest, Biome::Steppe][..],
            ),
            (
                1,
                &[
                    Biome::Alpine,
                    Biome::BorealForest,
                    Biome::Scree,
                    Biome::Snow,
                ][..],
            ),
            (2, &[Biome::Swamp, Biome::Riparian][..]),
            (3, &[Biome::Desert, Biome::Beach, Biome::Ocean][..]),
        ] {
            if self.biomes & (1 << class) != 0 {
                for b in biomes {
                    mask |= 1 << *b as u16;
                }
            }
        }
        mask
    }
    fn into_current(self) -> Species {
        Species {
            biomes: self.widen(),
            id: self.id,
            tree: self.tree,
            soils: self.soils,
            moisture: self.moisture,
            depth: [0, 0],
            wood: self.wood,
            trunk_width: self.trunk_width,
            form: self.form,
            height: self.height,
            spread: self.spread,
            spacing: self.spacing,
            evergreen: self.evergreen,
            colours: self.colours,
            flower: self.flower,
            growth_ms: self.growth_ms,
            seed_ms: self.seed_ms,
        }
    }
}
/// The saved layout before ground cover: the same fields without `cover` and
/// `field`, with carpet species among the organisms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LivingStateV1 {
    version: u16,
    seed: u64,
    size: u32,
    time_ms: u64,
    credit_ms: u64,
    start_season: Season,
    season_ms: u64,
    next_id: u64,
    cursor: u32,
    species: Vec<SpeciesV1>,
    organisms: Vec<OrganismV2>,
    spawns: Vec<[i64; 3]>,
}
/// An organism as versions 1 and 2 saved it: with its whole skeleton.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OrganismV2 {
    id: u64,
    species: u16,
    root: [i64; 3],
    age_ms: u64,
    last_update_ms: u64,
    last_seed_ms: u64,
    alive: bool,
    biomass: u16,
    wood: Vec<[i64; 3]>,
    parents: Vec<u16>,
    removed: Vec<bool>,
    committed: u16,
}
impl From<OrganismV2> for Organism {
    fn from(o: OrganismV2) -> Self {
        Self {
            id: o.id,
            species: o.species,
            root: o.root,
            age_ms: o.age_ms,
            growth_ms: o.age_ms,
            seed_growth_ms: 0,
            last_update_ms: o.last_update_ms,
            last_seed_ms: o.last_seed_ms,
            alive: o.alive,
            biomass: o.biomass,
            wood: o.wood.into(),
            parents: o.parents.into(),
            removed: std::sync::Arc::new(o.removed),
            committed: o.committed,
        }
    }
}
/// The saved layout of version 2: version 3 without the cover-independent
/// change of saving skeletons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LivingStateV2 {
    version: u16,
    seed: u64,
    size: u32,
    time_ms: u64,
    credit_ms: u64,
    start_season: Season,
    season_ms: u64,
    next_id: u64,
    cursor: u32,
    species: Vec<Species>,
    organisms: Vec<OrganismV2>,
    spawns: Vec<[i64; 3]>,
    cover: Vec<CoverSpecies>,
    field: CoverField,
    cleared: Vec<ClearedV3>,
    preset: u8,
}
impl From<LivingStateV2> for LivingState {
    fn from(old: LivingStateV2) -> Self {
        Self {
            version: VERSION,
            seed: old.seed,
            size: old.size,
            time_ms: old.time_ms,
            credit_ms: old.credit_ms,
            start_season: old.start_season,
            season_ms: old.season_ms,
            next_id: old.next_id,
            cursor: old.cursor,
            species: old.species,
            organisms: old.organisms.into_iter().map(Organism::from).collect(),
            spawns: old.spawns,
            cover: old.cover,
            field: old.field,
            cleared: old
                .cleared
                .into_iter()
                .map(|c| Cleared {
                    x: c.x,
                    z: c.z,
                    growth_ms: c.at_s.saturating_mul(1000),
                })
                .collect(),
            preset: old.preset,
            spatial: Default::default(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualPlant {
    pub id: u64,
    pub species: u16,
    pub root: [i64; 3],
    pub biomass: u16,
    pub tips: Vec<[i64; 3]>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualFrame {
    pub version: u16,
    pub time_ms: u64,
    pub season: Season,
    pub seed: u64,
    pub species: Vec<Species>,
    pub plants: Vec<VisualPlant>,
    pub cover: Vec<CoverSpecies>,
    pub field: CoverField,
    /// Where cover was destroyed, for the client to leave bare and regrow.
    pub cleared: Vec<Cleared>,
}
#[derive(Debug, Clone, Copy)]
pub struct SoftPart {
    pub center: [f32; 3],
    pub size: [f32; 3],
    pub colour: u8,
    pub yaw: f32,
    pub lean: f32,
}
#[derive(Debug, Clone, Copy)]
pub struct Growth {
    pub plant: u64,
    pub index: u16,
    pub cell: GlobalCell,
    pub material: MaterialId,
}

pub fn hash(mut n: u64) -> u64 {
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}
pub(crate) fn cell(p: [i64; 3]) -> GlobalCell {
    GlobalCell::new(p[0], p[1], p[2])
}
/// Water standing over the ground at the column, in cells (0 on land).
fn depth_of(columns: &ColumnMap, x: i64, z: i64) -> u8 {
    columns.water_depth(x, z).clamp(0, 255) as u8
}
pub(crate) fn moisture(columns: &ColumnMap, x: i64, z: i64) -> u8 {
    columns.moisture(x, z)
}

impl LivingState {
    pub fn generate(
        seed: u64,
        columns: &ColumnMap,
        terrain: &mut Volume,
        species: Vec<Species>,
        spawns: Vec<[i64; 3]>,
        season: Season,
    ) -> Result<Self, String> {
        let mut state = Self {
            version: VERSION,
            seed,
            size: columns.size(),
            time_ms: 0,
            credit_ms: 0,
            start_season: season,
            season_ms: 900_000,
            next_id: 1,
            cursor: 0,
            species,
            organisms: Vec::new(),
            spawns,
            cover: Vec::new(),
            field: CoverField::default(),
            cleared: Vec::new(),
            preset: if columns.is_gallery() {
                2
            } else {
                u8::from(columns.is_natural())
            },
            spatial: Default::default(),
        };
        state.validate()?;
        if columns.is_gallery() {
            state.place_gallery(columns, terrain)?;
            return Ok(state);
        }
        state.populate(columns, terrain)?;
        Ok(state)
    }
    /// The gallery: every species that grows in a biome, placed on that
    /// biome's pad: two of each tree (fully grown) and four of each other
    /// plant, each kind in its own slot of the pad, found by searching out from
    /// the slot's middle for ground that suits it.
    fn place_gallery(&mut self, columns: &ColumnMap, terrain: &mut Volume) -> Result<(), String> {
        let mut plan = EditPlan::new(terrain.id());
        for patch in spall_worldgen::gallery_patches(columns.size()) {
            let bit = 1u16 << patch.biome as u16;
            let kinds: Vec<(u16, bool, i64)> = self
                .species
                .iter()
                .filter(|s| s.biomes & bit != 0)
                .map(|s| (s.id, s.tree, i64::from(s.spacing)))
                .collect();
            if kinds.is_empty() {
                continue;
            }
            let across = (kinds.len() as f64).sqrt().ceil() as i64;
            let down = (kinds.len() as i64 + across - 1) / across;
            let (w, h) = (patch.x1 - patch.x0, patch.z1 - patch.z0);
            let (sw, sh) = (w / across, h / down);
            for (i, &(sid, tree, spacing)) in kinds.iter().enumerate() {
                let (col, row) = (i as i64 % across, i as i64 / across);
                let (cx, cz) = (patch.x0 + col * sw + sw / 2, patch.z0 + row * sh + sh / 2);
                let copies: i64 = if tree { 2 } else { 4 };
                for copy in 0..copies {
                    let start = [
                        cx + (copy - copies / 2) * (spacing + 3).min(sw / copies),
                        cz,
                    ];
                    // First the slot, then anywhere on the pad.
                    let reaches = [sw.max(sh) / 2, w.max(h)];
                    let placed = reaches.iter().find_map(|&reach| {
                        let mut r = 0;
                        while r <= reach {
                            let mut ring = Vec::new();
                            if r == 0 {
                                ring.push((0, 0));
                            } else {
                                let mut t = -r;
                                while t <= r {
                                    ring.extend([(t, -r), (t, r), (-r, t), (r, t)]);
                                    t += 2;
                                }
                            }
                            for (dx, dz) in ring {
                                let (x, z) = (start[0] + dx, start[1] + dz);
                                if x < patch.x0 || x >= patch.x1 || z < patch.z0 || z >= patch.z1 {
                                    continue;
                                }
                                let root = [x, i64::from(columns.height(x, z)) + 1, z];
                                if let Some(index) =
                                    self.establish_spaced(columns, terrain, sid, root, 8)
                                {
                                    return Some(index);
                                }
                            }
                            r += 2;
                        }
                        None
                    });
                    if let (Some(index), true) = (placed, tree) {
                        let wood = self.species.iter().find(|s| s.id == sid).map(|s| s.wood);
                        let p = &mut self.organisms[index];
                        p.committed = p.wood.len() as u16;
                        p.age_ms = 600_000;
                        p.growth_ms = 600_000;
                        for &at in p.wood.iter() {
                            plan.set(cell(at), MaterialId(wood.unwrap_or(0)));
                        }
                    }
                }
            }
        }
        terrain.apply_edit(&plan).map_err(|e| e.to_string())?;
        Ok(())
    }
    fn eligible(&self, columns: &ColumnMap, terrain: &Volume, s: &Species, root: [i64; 3]) -> bool {
        let [x, y, z] = root;
        if x < 33
            || z < 33
            || x >= i64::from(self.size) - 33
            || z >= i64::from(self.size) - 33
            || y < 1
            || y + i64::from(s.height) + 8 >= 351
            || !s.accepts_depth(depth_of(columns, x, z))
            || self
                .spawns
                .iter()
                .any(|p| (p[0] - x).pow(2) + (p[2] - z).pow(2) < 16 * 16)
        {
            return false;
        }
        let Ok(Sample::Filled(soil)) = terrain.sample(GlobalCell::new(x, y - 1, z)) else {
            return false;
        };
        if !s.accepts(columns.biome(x, z), moisture(columns, x, z), soil.0) {
            return false;
        }
        (0..if s.tree { i64::from(s.height) + 8 } else { 4 }).all(|dy| {
            matches!(
                terrain.sample(GlobalCell::new(x, y + dy, z)),
                Ok(Sample::Empty { .. })
            )
        })
    }
    pub(crate) fn establish(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        sid: u16,
        root: [i64; 3],
    ) -> Option<usize> {
        self.establish_spaced(columns, terrain, sid, root, i64::MAX)
    }
    /// [`Self::establish`] with the species' spacing capped at `max_spacing`
    /// cells (the gallery packs many kinds into a small pad).
    fn establish_spaced(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        sid: u16,
        root: [i64; 3],
        max_spacing: i64,
    ) -> Option<usize> {
        self.establish_with(columns, terrain, sid, root, None, max_spacing)
    }
    /// Establishment with an explicit spacing in cells (placement scales a
    /// species' spacing by the local canopy density), or the species' own
    /// spacing capped at `max_spacing`.
    pub(crate) fn establish_with(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        sid: u16,
        root: [i64; 3],
        spacing: Option<i64>,
        max_spacing: i64,
    ) -> Option<usize> {
        self.sync_index();
        let s = self.species.iter().find(|s| s.id == sid)?;
        let spacing = spacing.unwrap_or_else(|| i64::from(s.spacing).min(max_spacing));
        let tree = s.tree;
        let count = if tree {
            self.spatial.trees
        } else {
            self.spatial.ground
        };
        if count >= if tree { MAX_TREES } else { MAX_GROUND }
            || !self.eligible(columns, terrain, s, root)
            // A ground plant never roots in a tree's trunk column, even one
            // whose wood has not been committed yet.
            || (!tree
                && self.spatial.any_near(root[0], root[2], 2, |e| {
                    e.tree
                        && root[0] >= e.root[0]
                        && root[0] < e.root[0] + 2
                        && root[2] >= e.root[2]
                        && root[2] < e.root[2] + 2
                        && root[1] >= e.root[1]
                }))
            || self
                .spatial
                .any_near(root[0], root[2], spacing, |e| {
                    e.tree == tree
                        && (e.root[0] - root[0]).pow(2) + (e.root[2] - root[2]).pow(2)
                            < spacing.pow(2)
                })
        {
            return None;
        }
        let (wood, parents) = if s.tree {
            skeleton(root, s, hash(self.seed ^ self.next_id))
        } else {
            (vec![], vec![])
        };
        if wood
            .iter()
            .any(|p| !matches!(terrain.sample(cell(*p)), Ok(Sample::Empty { .. })))
        {
            return None;
        }
        // And a tree never grows over a ground plant already rooted there.
        if s.tree
            && wood.iter().any(|c| {
                self.spatial
                    .any_near(c[0], c[2], 0, |e| !e.tree && e.root == *c)
            })
        {
            return None;
        }
        let p = Organism {
            id: self.next_id,
            species: sid,
            root,
            age_ms: 0,
            growth_ms: 0,
            seed_growth_ms: 0,
            last_update_ms: self.time_ms,
            last_seed_ms: self.time_ms,
            alive: true,
            biomass: 100,
            removed: std::sync::Arc::new(vec![false; wood.len()]),
            wood: wood.into(),
            parents: parents.into(),
            committed: 0,
        };
        self.next_id += 1;
        self.spatial.add(root, tree);
        self.organisms.push(p);
        Some(self.organisms.len() - 1)
    }
    /// Plants a sapling of species `sid` rooted at `root` if the ground, the
    /// spacing and the caps allow it, exactly as seeding does: it starts
    /// unbuilt and grows through the normal edit pipeline. Returns its id.
    pub fn plant_sapling(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        sid: u16,
        root: [i64; 3],
    ) -> Option<u64> {
        let index = self.establish(columns, terrain, sid, root)?;
        Some(self.organisms[index].id)
    }
    /// Gives the world its ground cover: the species, and the climate raster
    /// they read, sampled from the world's own columns.
    pub fn with_cover(
        mut self,
        cover: Vec<CoverSpecies>,
        columns: &ColumnMap,
    ) -> Result<Self, String> {
        self.cover = cover;
        self.field = CoverField::from_columns(columns);
        self.validate()?;
        Ok(self)
    }
    /// Samples the climate raster if the state has cover but no raster yet (a
    /// state upgraded from version 1, which had no raster).
    pub fn ensure_field(&mut self, columns: &ColumnMap) {
        if self.field.is_empty() && !self.cover.is_empty() {
            self.field = CoverField::from_columns(columns);
        }
    }
    /// Records that the cover in column `(x, z)` was destroyed now; its
    /// regrowth starts over if it was already regrowing. Past
    /// [`MAX_CLEARED`] records the oldest is forgotten (its cover is nearly
    /// back anyway).
    pub fn clear_cover(&mut self, x: i64, z: i64) {
        let (Ok(x), Ok(z)) = (u16::try_from(x), u16::try_from(z)) else {
            return;
        };
        if u32::from(x) >= self.size || u32::from(z) >= self.size {
            return;
        }
        match self.cleared.binary_search_by_key(&(x, z), |c| (c.x, c.z)) {
            Ok(i) => self.cleared[i].growth_ms = 0,
            Err(i) => {
                if self.cleared.len() >= MAX_CLEARED {
                    if let Some(oldest) =
                        (0..self.cleared.len()).min_by_key(|&k| self.cleared[k].growth_ms)
                    {
                        self.cleared.remove(oldest);
                    }
                    let i = self
                        .cleared
                        .binary_search_by_key(&(x, z), |c| (c.x, c.z))
                        .unwrap_err();
                    self.cleared.insert(i, Cleared { x, z, growth_ms: 0 });
                } else {
                    self.cleared.insert(i, Cleared { x, z, growth_ms: 0 });
                }
            }
        }
    }
    pub fn season(&self) -> Season {
        Season::at(self.start_season, self.time_ms, self.season_ms)
    }
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate_with(false)?;
        let bytes = postcard::to_stdvec(self).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err("vegetation state byte limit".into());
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_STATE_BYTES {
            return Err("vegetation state byte limit".into());
        }
        let (version, _) = postcard::take_from_bytes::<u16>(bytes).map_err(|e| e.to_string())?;
        let mut s = match version {
            1 => Self::upgrade(
                postcard::from_bytes::<LivingStateV1>(bytes).map_err(|e| e.to_string())?,
            ),
            2 => postcard::from_bytes::<LivingStateV2>(bytes)
                .map_err(|e| e.to_string())?
                .into(),
            3 => {
                let mut state = upgrade_v3(
                    postcard::from_bytes::<LivingStateV3>(bytes).map_err(|e| e.to_string())?,
                );
                state.grow_skeletons()?;
                state
            }
            _ => {
                let mut s: Self = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
                s.grow_skeletons()?;
                s
            }
        };
        if version < VERSION {
            migrate_legacy_clocks(&mut s);
        }
        s.validate_with(version < VERSION)?;
        if version < VERSION {
            s.check_skeletons_reproducible()?;
        }
        s.spatial.rebuild(&s.organisms);
        Ok(s)
    }
    /// An older save carried its skeletons; version 3 will save only the
    /// removed cells and grow the rest back, so a tree whose saved wood is not
    /// what `skeleton` makes of it would change shape on the next load. Refuse
    /// such a save loudly rather than reshape its trees.
    fn check_skeletons_reproducible(&self) -> Result<(), String> {
        for p in self.organisms.iter().filter(|p| !p.wood.is_empty()) {
            let s = self
                .species
                .iter()
                .find(|s| s.id == p.species)
                .ok_or("organism of an unknown species")?;
            let (wood, parents) = skeleton(p.root, s, hash(self.seed ^ p.id));
            if *p.wood != *wood || *p.parents != *parents {
                return Err(format!(
                    "tree {} of an older save does not match the skeleton rule",
                    p.id
                ));
            }
        }
        Ok(())
    }
    /// Gives every tree its skeleton back after loading (version 3 saves only
    /// the removed cells): the same `skeleton` call that established it, keyed
    /// by the tree's id.
    fn grow_skeletons(&mut self) -> Result<(), String> {
        let species: std::collections::HashMap<u16, &Species> =
            self.species.iter().map(|s| (s.id, s)).collect();
        for p in &mut self.organisms {
            let Some(s) = species.get(&p.species) else {
                return Err("organism of an unknown species".into());
            };
            if !s.tree {
                if !p.removed.is_empty() || p.committed != 0 {
                    return Err("ground plant with wood state".into());
                }
                continue;
            }
            let (wood, parents) = skeleton(p.root, s, hash(self.seed ^ p.id));
            if p.removed.len() > wood.len() || usize::from(p.committed) > wood.len() {
                return Err("removed or committed cell beyond the skeleton".into());
            }
            std::sync::Arc::make_mut(&mut p.removed).resize(wood.len(), false);
            p.wood = wood.into();
            p.parents = parents.into();
        }
        Ok(())
    }
    /// Version 1 to 2. The carpet species (grass, sedge, clover, tussock
    /// forms) become cover species with their ids, biomes, soils, moisture,
    /// heights and colours; their organisms are dropped, because cover is
    /// stateless and is never placed again. Every other organism, the clock and
    /// the ids carry over untouched: an upgrade never regenerates a world.
    fn upgrade(old: LivingStateV1) -> Self {
        let carpet: Vec<u16> = old
            .species
            .iter()
            .filter(|s| !s.tree && is_carpet_form(s.form))
            .map(|s| s.id)
            .collect();
        let cover = old
            .species
            .iter()
            .filter(|s| carpet.contains(&s.id))
            .map(|s| CoverSpecies {
                id: s.id,
                biomes: s.widen(),
                soils: s.soils,
                moisture: s.moisture,
                temperature: [-62, 62],
                form: s.form,
                height: s.height,
                colours: s.colours,
                evergreen: s.evergreen,
                density: 96,
                depth: [0, 0],
                accent: s.colours[1],
            })
            .collect();
        Self {
            version: VERSION,
            seed: old.seed,
            size: old.size,
            time_ms: old.time_ms,
            credit_ms: old.credit_ms,
            start_season: old.start_season,
            season_ms: old.season_ms,
            next_id: old.next_id,
            cursor: 0,
            species: old
                .species
                .into_iter()
                .filter(|s| !carpet.contains(&s.id))
                .map(SpeciesV1::into_current)
                .collect(),
            organisms: old
                .organisms
                .into_iter()
                .filter(|o| !carpet.contains(&o.species))
                .map(Organism::from)
                .collect(),
            spawns: old.spawns,
            cover,
            field: CoverField::default(),
            cleared: Vec::new(),
            // Version 1 predates natural worlds.
            preset: 0,
            spatial: Default::default(),
        }
    }
    /// Full validation, including every skeleton cell of every tree.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_with(true)
    }
    /// `skeletons: false` skips the per-cell checks of the wood, for states
    /// whose skeletons the program itself just grew (a version 3 load) or is
    /// about to save; they cost milliseconds at 20,000 trees.
    fn validate_with(&self, skeletons: bool) -> Result<(), String> {
        if self.version != VERSION
            || self.species.len() > MAX_SPECIES
            || self.season_ms == 0
            || self.size < 128
            || self.size > 4096
            || self.organisms.len() > MAX_TREES + MAX_GROUND
            || self.spawns.len() > 64
            || self.cover.len() > MAX_COVER_SPECIES
            || self.preset > 2
            || self.cleared.len() > MAX_CLEARED
            || self
                .cleared
                .windows(2)
                .any(|w| (w[0].x, w[0].z) >= (w[1].x, w[1].z))
            || self
                .cleared
                .iter()
                .any(|c| u32::from(c.x) >= self.size || u32::from(c.z) >= self.size)
        {
            return Err("invalid vegetation schema/limits".into());
        }
        self.field.validate()?;
        let mut cover_ids = std::collections::BTreeSet::new();
        for c in &self.cover {
            c.validate()?;
            if !cover_ids.insert(c.id) || self.species.iter().any(|s| s.id == c.id) {
                return Err("duplicate cover species id".into());
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for s in &self.species {
            if !ids.insert(s.id)
                || s.id == 0
                || s.biomes == 0
                || s.biomes > ALL_BIOMES
                || s.form > 24
                || s.depth[0] > s.depth[1]
                || (s.tree && s.depth[1] > 0)
                || s.spacing == 0
                || s.height == 0
                || s.height > 64
                || s.spread == 0
                || s.spread > 20
                || s.growth_ms == 0
                || s.seed_ms == 0
                || s.colours
                    .iter()
                    .chain([&s.flower])
                    .any(|c| usize::from(*c) >= PALETTE.len())
                || s.moisture[0] > s.moisture[1]
                || (s.tree && (s.wood == 0 || s.trunk_width == 0 || s.trunk_width > 2))
            {
                return Err("invalid species".into());
            }
        }
        let mut plant_ids = std::collections::BTreeSet::new();
        let mut total = 0;
        for p in &self.organisms {
            if !plant_ids.insert(p.id)
                || p.id == 0
                || p.id >= self.next_id
                || !ids.contains(&p.species)
                || p.biomass > 1000
                || p.wood.len() > 1024
                || p.parents.len() != p.wood.len()
                || p.removed.len() != p.wood.len()
                || usize::from(p.committed) > p.wood.len()
                || p.root.iter().any(|c| c.unsigned_abs() > 1_000_000)
                || (skeletons
                    && p.wood
                        .iter()
                        .flatten()
                        .any(|c| c.unsigned_abs() > 1_000_000))
                || self.credit_ms >= 1000
                || p.root[0] < 33
                || p.root[2] < 33
                || p.root[0] >= i64::from(self.size) - 33
                || p.root[2] >= i64::from(self.size) - 33
            {
                return Err("invalid organism".into());
            }
            for (i, (&at, &parent)) in p.wood.iter().zip(p.parents.iter()).enumerate() {
                if !skeletons {
                    break;
                }
                if i == 0 {
                    if at != p.root || parent != 0 {
                        return Err("invalid root".into());
                    }
                } else if usize::from(parent) >= i
                    || at
                        .iter()
                        .zip(p.wood[usize::from(parent)])
                        .map(|(a, b)| (a - b).abs())
                        .sum::<i64>()
                        != 1
                {
                    return Err("disconnected skeleton".into());
                }
            }
            total += p.wood.len();
        }
        if total > 2_500_000 {
            return Err("skeleton cell limit".into());
        }
        Ok(())
    }
    /// One owner-thread pass. Unknown terrain suspends that organism. Wood requests
    /// carry one cell and must be acknowledged by the normal transaction pipeline.
    pub fn advance(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        elapsed_ms: u64,
    ) -> Vec<Growth> {
        self.credit_ms = self.credit_ms.saturating_add(elapsed_ms);
        if self.credit_ms < 1000 {
            return vec![];
        }
        let dt = self.credit_ms / 1000 * 1000;
        self.credit_ms %= 1000;
        let previous_time_ms = self.time_ms;
        self.time_ms = self.time_ms.saturating_add(dt);
        let season = self.season();
        let active_world_dt = active_between(
            self.start_season,
            self.season_ms,
            previous_time_ms,
            self.time_ms,
        );
        for c in &mut self.cleared {
            c.growth_ms = u64::from(c.growth_ms)
                .saturating_add(active_world_dt)
                .min(u64::from(FULL_AFTER_S * 1000)) as u32;
        }
        self.cleared.retain(|c| c.growth_ms < FULL_AFTER_S * 1000);
        let n = self.organisms.len();
        let mut seeds = Vec::new();
        let mut growth = Vec::new();
        let visits = (n / 40).clamp(128, 512).min(n);
        for step in 0..visits {
            let i = (self.cursor as usize + step) % n;
            let s = self
                .species
                .iter()
                .find(|s| s.id == self.organisms[i].species)
                .unwrap();
            let p = &mut self.organisms[i];
            if !p.alive {
                continue;
            }
            let soil = GlobalCell::new(p.root[0], p.root[1] - 1, p.root[2]);
            match terrain.sample(soil) {
                Ok(Sample::Unknown(_)) => continue,
                Ok(Sample::Filled(m))
                    if s.accepts(
                        columns.biome(p.root[0], p.root[2]),
                        moisture(columns, p.root[0], p.root[2]),
                        m.0,
                    ) && s.accepts_depth(depth_of(columns, p.root[0], p.root[2])) => {}
                _ => {
                    p.alive = false;
                    continue;
                }
            }
            if s.tree {
                if p.wood
                    .iter()
                    .take(usize::from(p.committed))
                    .any(|at| matches!(terrain.sample(cell(*at)), Ok(Sample::Unknown(_))))
                {
                    continue;
                }
                for j in 0..usize::from(p.committed) {
                    if matches!(terrain.sample(cell(p.wood[j])), Ok(Sample::Unknown(_))) {
                        continue;
                    }
                    if p.removed[j]
                        || (j > 0 && p.removed[usize::from(p.parents[j])])
                        || !matches!(terrain.sample(cell(p.wood[j])),Ok(Sample::Filled(m)) if m.0==s.wood)
                    {
                        std::sync::Arc::make_mut(&mut p.removed)[j] = true;
                    }
                }
                if p.committed > 0 && p.removed[0] {
                    p.alive = false;
                    continue;
                }
            } else {
                match terrain.sample(cell(p.root)) {
                    Ok(Sample::Unknown(_)) => continue,
                    Ok(Sample::Empty { .. }) => {}
                    _ => {
                        p.alive = false;
                        continue;
                    }
                }
            }
            // Elapsed ecological time, including fair-budget deferral, never wall-clock time.
            let due = self.time_ms.saturating_sub(p.last_update_ms);
            let active_due = active_between(
                self.start_season,
                self.season_ms,
                p.last_update_ms,
                self.time_ms,
            );
            p.age_ms = p.age_ms.saturating_add(due);
            p.growth_ms = p.growth_ms.saturating_add(active_due);
            p.seed_growth_ms = p.seed_growth_ms.saturating_add(active_due);
            p.last_update_ms = self.time_ms;
            p.biomass = p
                .biomass
                .saturating_add((active_due / 1000).min(50) as u16 * 20)
                .min(1000);
            if s.tree
                && season != Season::Winter
                && growth.len() < 4
                && p.growth_ms >= u64::from(p.committed + 1) * s.growth_ms
            {
                let j = usize::from(p.committed);
                if j < p.wood.len() {
                    if j > 0 && p.removed[usize::from(p.parents[j])] {
                        std::sync::Arc::make_mut(&mut p.removed)[j] = true;
                        p.committed += 1;
                    } else if matches!(terrain.sample(cell(p.wood[j])), Ok(Sample::Empty { .. })) {
                        growth.push(Growth {
                            plant: p.id,
                            index: j as u16,
                            cell: cell(p.wood[j]),
                            material: MaterialId(s.wood),
                        });
                    } else if !matches!(terrain.sample(cell(p.wood[j])), Ok(Sample::Unknown(_))) {
                        std::sync::Arc::make_mut(&mut p.removed)[j] = true;
                        p.committed += 1;
                    }
                }
            }
            let mature = !s.tree || usize::from(p.committed) == p.wood.len();
            if mature && season != Season::Winter && p.seed_growth_ms >= s.seed_ms {
                p.last_seed_ms = self.time_ms;
                p.seed_growth_ms = 0;
                let key = hash(self.seed ^ p.id ^ self.time_ms);
                let radius = i64::from(s.spacing) * 3;
                let dx = (key % (radius as u64 * 2 + 1)) as i64 - radius;
                let dz = (key.rotate_left(23) % (radius as u64 * 2 + 1)) as i64 - radius;
                let at = GlobalCell::new(p.root[0] + dx, p.root[1], p.root[2] + dz);
                if let (Some(root), false) = crate::surface_root(terrain, at, None) {
                    seeds.push((s.id, [root.x, root.y, root.z]));
                }
            }
        }
        self.cursor = if n == 0 {
            0
        } else {
            ((self.cursor as usize + visits) % n) as u32
        };
        self.organisms.retain(|p| p.alive);
        if self.organisms.len() != n {
            self.spatial.rebuild(&self.organisms);
        }
        if !self.organisms.is_empty() {
            self.cursor %= self.organisms.len() as u32;
        } else {
            self.cursor = 0;
        }
        for (sid, root) in seeds {
            self.establish(columns, terrain, sid, root);
        }
        growth
    }
    pub fn acknowledge(&mut self, g: Growth, terrain: &Volume) {
        if let Some(p) = self.organisms.iter_mut().find(|p| p.id == g.plant)
            && p.committed == g.index
            && matches!(terrain.sample(g.cell),Ok(Sample::Filled(m)) if m==g.material)
        {
            p.committed += 1;
        }
    }
    pub fn harvest(&mut self, center: [i64; 3], radius: i64) {
        for p in &mut self.organisms {
            if p.wood.is_empty()
                && p.root
                    .iter()
                    .zip(center)
                    .map(|(a, b)| (a - b).pow(2))
                    .sum::<i64>()
                    <= radius * radius
            {
                p.biomass = 0;
            }
        }
    }
    pub fn visual(&self, terrain: &Volume) -> VisualFrame {
        let mut plants = Vec::new();
        for p in self.organisms.iter().filter(|p| p.alive) {
            let s = self.species.iter().find(|s| s.id == p.species).unwrap();
            if !s.tree && !matches!(terrain.sample(cell(p.root)), Ok(Sample::Empty { .. })) {
                continue;
            }
            if !matches!(terrain.sample(GlobalCell::new(p.root[0],p.root[1]-1,p.root[2])),Ok(Sample::Filled(m)) if s.soils.contains(&m.0))
            {
                continue;
            }
            let mut tips = Vec::new();
            if s.tree {
                let end = usize::from(p.committed);
                let trunk_top = p
                    .wood
                    .iter()
                    .take(end)
                    .filter(|at| {
                        at[0] >= p.root[0]
                            && at[0] < p.root[0] + i64::from(s.trunk_width)
                            && at[2] >= p.root[2]
                            && at[2] < p.root[2] + i64::from(s.trunk_width)
                    })
                    .map(|at| at[1])
                    .max()
                    .unwrap_or(p.root[1]);
                if end > 0
                    && !matches!(terrain.sample(cell(p.root)),Ok(Sample::Filled(m)) if m.0==s.wood)
                {
                    continue;
                }
                for j in 0..end {
                    let at = p.wood[j];
                    if at[0] >= p.root[0]
                        && at[0] < p.root[0] + i64::from(s.trunk_width)
                        && at[2] >= p.root[2]
                        && at[2] < p.root[2] + i64::from(s.trunk_width)
                        && at[1] < trunk_top
                    {
                        continue;
                    }
                    if p.removed[j]
                        || !matches!(terrain.sample(cell(p.wood[j])),Ok(Sample::Filled(m)) if m.0==s.wood)
                    {
                        continue;
                    }
                    if !(j + 1..end).any(|k| !p.removed[k] && usize::from(p.parents[k]) == j)
                        && !tips.iter().any(|tip: &[i64; 3]| {
                            tip.iter()
                                .zip(p.wood[j])
                                .map(|(a, b)| (a - b).pow(2))
                                .sum::<i64>()
                                < 9
                        })
                    {
                        tips.push(p.wood[j]);
                    }
                }
                if end == 0 {
                    tips.push(p.root);
                }
            }
            plants.push(VisualPlant {
                id: p.id,
                species: p.species,
                root: p.root,
                biomass: p.biomass,
                tips,
            });
        }
        VisualFrame {
            version: VERSION,
            time_ms: self.time_ms,
            season: self.season(),
            seed: self.seed,
            species: self.species.clone(),
            plants,
            cover: self.cover.clone(),
            field: self.field.clone(),
            cleared: self.cleared.clone(),
        }
    }
}

/// The wood a tree grows: part of the saved format (see [`crate::codec`]).
fn skeleton(root: [i64; 3], s: &Species, key: u64) -> (Vec<[i64; 3]>, Vec<u16>) {
    let height = i64::from(s.height) + (key % 5) as i64 - 2;
    let mut wood = Vec::new();
    let mut parents = Vec::new();
    for y in 0..height {
        for z in 0..i64::from(s.trunk_width) {
            for x in 0..i64::from(s.trunk_width) {
                let at = [root[0] + x, root[1] + y, root[2] + z];
                let parent_at = if x > 0 {
                    [at[0] - 1, at[1], at[2]]
                } else if z > 0 {
                    [at[0], at[1], at[2] - 1]
                } else {
                    [at[0], at[1] - 1, at[2]]
                };
                let parent = wood.iter().position(|p| *p == parent_at).unwrap_or(0) as u16;
                wood.push(at);
                parents.push(parent);
            }
        }
    }
    let levels = if s.form == 1 { 3 } else { 2 };
    for tier in 0..levels {
        for arm in 0..4 {
            let fork = (height / 2 + tier * (height / 6) + arm % 2).min(height - 2);
            let mut at = [root[0], root[1] + fork, root[2]];
            let mut parent = wood.iter().position(|p| *p == at).unwrap() as u16;
            let len = if s.form == 2 { s.spread / 2 } else { s.spread }.max(2);
            for step in 0..i64::from(len) {
                if step % 3 == 2 {
                    at[1] += if s.form == 3 { -1 } else { 1 };
                } else {
                    let axis = if arm % 2 == 0 { 0 } else { 2 };
                    at[axis] += if arm < 2 { 1 } else { -1 };
                }
                if let Some(i) = wood.iter().position(|p| *p == at) {
                    parent = i as u16;
                    continue;
                }
                let next = wood.len() as u16;
                wood.push(at);
                parents.push(parent);
                parent = next;
            }
        }
    }
    (wood, parents)
}

/// The part of a world one client is interested in: a square of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub x: [i64; 2],
    pub z: [i64; 2],
}

impl Window {
    /// The square of `half` cells around `(x, z)`.
    pub fn around(x: i64, z: i64, half: i64) -> Self {
        Self {
            x: [x - half, x + half],
            z: [z - half, z + half],
        }
    }
    pub fn contains(&self, x: i64, z: i64) -> bool {
        (self.x[0]..=self.x[1]).contains(&x) && (self.z[0]..=self.z[1]).contains(&z)
    }
}

impl VisualFrame {
    /// The frame for a client that only draws `window`: the plants and the
    /// destroyed-cover records inside it. The climate raster is the whole
    /// world's and does not change, so it goes along only when `with_field`
    /// (a client keeps the last one it received).
    pub fn windowed(&self, window: Window, with_field: bool) -> VisualFrame {
        VisualFrame {
            version: self.version,
            time_ms: self.time_ms,
            season: self.season,
            seed: self.seed,
            species: self.species.clone(),
            plants: self
                .plants
                .iter()
                .filter(|p| window.contains(p.root[0], p.root[2]))
                .cloned()
                .collect(),
            cover: self.cover.clone(),
            field: if with_field {
                self.field.clone()
            } else {
                CoverField::default()
            },
            cleared: self
                .cleared
                .iter()
                .filter(|c| window.contains(i64::from(c.x), i64::from(c.z)))
                .copied()
                .collect(),
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(self).map_err(|e| e.to_string())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("visual byte limit".into());
        }
        let frame: Self = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
        if frame.version != VERSION
            || frame.species.len() > MAX_SPECIES
            || frame.cover.len() > MAX_COVER_SPECIES
            || frame.cover.iter().any(|c| c.validate().is_err())
            || frame.field.validate().is_err()
            || frame.cleared.len() > MAX_CLEARED
            || frame.plants.len() > MAX_TREES + MAX_GROUND
            || frame.plants.iter().any(|p| {
                p.tips.len() > 32
                    || p.root
                        .iter()
                        .chain(p.tips.iter().flatten())
                        .any(|v| v.unsigned_abs() > 1_000_000)
            })
            || frame.species.iter().any(|s| {
                s.height > 64
                    || s.spread > 20
                    || s.colours
                        .iter()
                        .chain([&s.flower])
                        .any(|c| usize::from(*c) >= PALETTE.len())
            })
        {
            return Err("visual schema/limits".into());
        }
        Ok(frame)
    }
    /// Deterministic soft geometry, culled before allocation. Leaves occupy quarter
    /// cells; stems/blades are smaller, grounded at the exact top of the soil cell.
    pub fn parts(&self, eye: [f32; 3], radius: f32) -> Vec<SoftPart> {
        self.parts_lod(eye, radius, f32::INFINITY)
    }
    /// [`Self::parts`] with a level of detail: trees farther than `far_m`
    /// metres from `eye` get their crowns at half resolution, leaf blocks of
    /// 2 x 2 x 2 cells (half a metre), about an eighth of the cubes. Ground
    /// plants are unchanged.
    pub fn parts_lod(&self, eye: [f32; 3], radius: f32, far_m: f32) -> Vec<SoftPart> {
        let mut out = Vec::new();
        let distance = |p: &VisualPlant| {
            (p.root[0] as f32 * 0.25 - eye[0]).powi(2) + (p.root[2] as f32 * 0.25 - eye[2]).powi(2)
        };
        let mut nearby: Vec<_> = self
            .plants
            .iter()
            .filter(|p| distance(p) <= radius * radius)
            .collect();
        nearby.sort_by(|a, b| distance(a).total_cmp(&distance(b)).then(a.id.cmp(&b.id)));
        for p in nearby {
            if out.len() > 149_000 {
                break;
            }
            let base = [
                (p.root[0] as f32 + 0.5) * 0.25,
                p.root[1] as f32 * 0.25,
                (p.root[2] as f32 + 0.5) * 0.25,
            ];
            if (base[0] - eye[0]).powi(2) + (base[2] - eye[2]).powi(2) > radius * radius {
                continue;
            }
            let Some(s) = self.species.iter().find(|s| s.id == p.species) else {
                continue;
            };
            let colour = s.colours[self.season as usize];
            if s.tree {
                if self.season == Season::Winter && !s.evergreen {
                    continue;
                }
                // Cells of `step` quarter-cells a side: 1 near, 2 far away.
                let step: i64 = if distance(p) > far_m * far_m { 2 } else { 1 };
                let mut leaves = std::collections::HashSet::new();
                for tip in &p.tips {
                    let juvenile = tip == &p.root;
                    let r = if juvenile {
                        1
                    } else {
                        i64::from(s.spread).clamp(2, 6)
                    };
                    let (tip, r) = if step == 1 {
                        (*tip, r)
                    } else {
                        (tip.map(|v| v.div_euclid(step)), (r + step - 1) / step)
                    };
                    let tip = &tip;
                    let lift = if step == 1 { 1 } else { 0 };
                    for y in -r..=r {
                        for z in -r..=r {
                            for x in -r..=r {
                                let shape = match s.form {
                                    1 => {
                                        let width = (r - y).max(1) / 2;
                                        x * x + z * z <= width * width
                                    }
                                    2 => x * x * 3 + z * z * 3 + y * y <= r * r,
                                    4 => x * x + z * z + y * y * 6 <= r * r,
                                    5 => x * x + y * y + z * z * 5 <= r * r,
                                    _ => x * x + z * z + y * y * 2 <= r * r,
                                };
                                if !shape
                                    || hash(
                                        p.id ^ (x as u64).rotate_left(17)
                                            ^ (z as u64).rotate_left(41)
                                            ^ y as u64,
                                    )
                                    .is_multiple_of(9)
                                {
                                    continue;
                                }
                                leaves.insert([tip[0] + x, tip[1] + y + lift, tip[2] + z]);
                            }
                        }
                    }
                }
                // Shared/fully enclosed opaque leaf cells contribute no visible
                // surface. Keep the exact exterior (including noise holes), but
                // avoid drawing duplicate crowns and hidden interior cubes.
                let mut surface: Vec<_> = leaves
                    .iter()
                    .copied()
                    .filter(|at| {
                        [
                            (1, 0, 0),
                            (-1, 0, 0),
                            (0, 1, 0),
                            (0, -1, 0),
                            (0, 0, 1),
                            (0, 0, -1),
                        ]
                        .into_iter()
                        .any(|(x, y, z)| !leaves.contains(&[at[0] + x, at[1] + y, at[2] + z]))
                    })
                    .collect();
                surface.sort_unstable();
                for at in surface {
                    if out.len() >= 150_000 {
                        return out;
                    }
                    let cell = 0.25 * step as f32;
                    out.push(SoftPart {
                        center: at.map(|v| v as f32 * cell + cell / 2.),
                        size: [cell; 3],
                        colour,
                        yaw: 0.,
                        lean: 0.,
                    });
                }
            } else if p.biomass > 0 {
                let height = f32::from(s.height) * 0.25 * f32::from(p.biomass) / 1000.;
                let winter = if self.season == Season::Winter && !s.evergreen {
                    0.25
                } else {
                    1.
                };
                let height = height * winter;
                crate::cover::ground_parts(
                    s.form,
                    height,
                    colour,
                    s.flower,
                    self.season,
                    p.id,
                    base,
                    crate::cover::Detail::Full,
                    &mut out,
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn species(id: u16, tree: bool, form: u8) -> SpeciesV1 {
        SpeciesV1 {
            id,
            tree,
            biomes: 1,
            soils: [1, 2, 0, 0],
            moisture: [50, 200],
            wood: if tree { 124 } else { 0 },
            trunk_width: u8::from(tree),
            form,
            height: 3,
            spread: 3,
            spacing: 4,
            evergreen: false,
            colours: [1, 2, 3, 4],
            flower: 10,
            growth_ms: 1000,
            seed_ms: 1000,
        }
    }

    #[test]
    fn active_growth_splits_long_and_deferred_intervals_at_season_boundaries() {
        let length = 10_000;
        assert_eq!(active_between(Season::Spring, length, 0, 40_000), 30_000);
        assert_eq!(
            active_between(Season::Spring, length, 5_000, 35_000),
            25_000
        );
        assert_eq!(
            active_between(Season::Spring, length, 5_000, 15_000)
                + active_between(Season::Spring, length, 15_000, 35_000),
            active_between(Season::Spring, length, 5_000, 35_000),
            "fair-budget splits preserve the same active credit"
        );
        assert_eq!(active_between(Season::Winter, length, 0, 10_000), 0);
        assert_eq!(active_between(Season::Winter, length, 0, 40_000), 30_000);
        assert_eq!(active_between(Season::Spring, length, 0, 400_000), 300_000);
    }

    #[test]
    fn legacy_cleared_timestamp_migrates_to_active_growth_and_freezes_in_winter() {
        let mut state = state_with(vec![], vec![]);
        state.start_season = Season::Spring;
        state.season_ms = 100_000;
        state.time_ms = 400_000;
        state.cleared = vec![Cleared {
            x: 5,
            z: 6,
            growth_ms: 200_000,
        }];
        migrate_legacy_clocks(&mut state);
        assert_eq!(
            state.cleared,
            [Cleared {
                x: 5,
                z: 6,
                growth_ms: 100_000
            }]
        );
    }

    #[test]
    fn version_3_save_decodes_with_explicit_clock_migration_and_exact_v4_recovery() {
        #[derive(Serialize)]
        struct LegacyV3 {
            version: u16,
            seed: u64,
            size: u32,
            time_ms: u64,
            credit_ms: u64,
            start_season: Season,
            season_ms: u64,
            next_id: u64,
            cursor: u32,
            species: Vec<Species>,
            #[serde(with = "crate::codec::v3")]
            organisms: Vec<Organism>,
            spawns: Vec<[i64; 3]>,
            cover: Vec<CoverSpecies>,
            field: CoverField,
            cleared: Vec<ClearedV3>,
            preset: u8,
        }
        let (species, mut plant) = grown_tree(9, 3, 60);
        plant.age_ms = 200_000;
        plant.last_update_ms = 200_000;
        plant.last_seed_ms = 200_000;
        let legacy = LegacyV3 {
            version: 3,
            seed: 9,
            size: 256,
            time_ms: 400_000,
            credit_ms: 0,
            start_season: Season::Spring,
            season_ms: 100_000,
            next_id: 4,
            cursor: 0,
            species: vec![species],
            organisms: vec![plant],
            spawns: vec![],
            cover: vec![],
            field: CoverField::default(),
            cleared: vec![ClearedV3 {
                x: 5,
                z: 6,
                at_s: 200,
            }],
            preset: 1,
        };
        let state = LivingState::decode(&postcard::to_stdvec(&legacy).unwrap()).unwrap();
        assert_eq!(
            state.cleared,
            [Cleared {
                x: 5,
                z: 6,
                growth_ms: 100_000
            }]
        );
        assert_eq!(state.organisms[0].growth_ms, 200_000);
        let recovered = LivingState::decode(&state.encode().unwrap()).unwrap();
        assert_eq!(recovered, state);
    }

    fn organism(id: u64, species: u16, x: i64) -> OrganismV2 {
        OrganismV2 {
            id,
            species,
            root: [x, 9, 40],
            age_ms: 5,
            last_update_ms: 7,
            last_seed_ms: 7,
            alive: true,
            biomass: 400,
            wood: vec![],
            parents: vec![],
            removed: vec![],
            committed: 0,
        }
    }

    /// A grown tree of a made-up species, as establishment makes it.
    fn grown_tree(seed: u64, id: u64, x: i64) -> (Species, Organism) {
        let mut s = species(1, true, 0).into_current();
        s.height = 24;
        s.spread = 6;
        let root = [x, 9, 40];
        let (wood, parents) = skeleton(root, &s, hash(seed ^ id));
        let p = Organism {
            id,
            species: 1,
            root,
            age_ms: 600_000,
            growth_ms: 600_000,
            seed_growth_ms: 0,
            last_update_ms: 7,
            last_seed_ms: 7,
            alive: true,
            biomass: 400,
            removed: std::sync::Arc::new(vec![false; wood.len()]),
            committed: wood.len() as u16,
            wood: wood.into(),
            parents: parents.into(),
        };
        (s, p)
    }

    fn state_with(trees: Vec<Organism>, species: Vec<Species>) -> LivingState {
        LivingState {
            version: VERSION,
            seed: 9,
            size: 256,
            time_ms: 5_000,
            credit_ms: 0,
            start_season: Season::Summer,
            season_ms: 900_000,
            next_id: 100,
            cursor: 0,
            species,
            organisms: trees,
            spawns: vec![],
            cover: vec![],
            field: CoverField::default(),
            cleared: vec![],
            preset: 1,
            spatial: Default::default(),
        }
    }

    #[test]
    fn a_windowed_frame_keeps_only_what_is_inside_and_the_field_only_on_request() {
        let (s, a) = grown_tree(9, 3, 60);
        let (_, b) = grown_tree(9, 4, 400);
        let mut state = state_with(vec![a, b], vec![s]);
        state.field = CoverField {
            width: 4,
            rle: vec![16, 1],
        };
        state.cleared = vec![
            Cleared {
                x: 62,
                z: 40,
                growth_ms: 1000,
            },
            Cleared {
                x: 401,
                z: 40,
                growth_ms: 2000,
            },
        ];
        let terrain_free = VisualFrame {
            version: VERSION,
            time_ms: 5_000,
            season: Season::Summer,
            seed: 9,
            species: state.species.clone(),
            plants: state
                .organisms
                .iter()
                .map(|p| VisualPlant {
                    id: p.id,
                    species: p.species,
                    root: p.root,
                    biomass: p.biomass,
                    tips: vec![p.root],
                })
                .collect(),
            cover: vec![],
            field: state.field.clone(),
            cleared: state.cleared.clone(),
        };
        let near = terrain_free.windowed(Window::around(60, 40, 50), false);
        assert_eq!(near.plants.len(), 1);
        assert_eq!(near.plants[0].root[0], 60);
        assert_eq!(near.cleared.len(), 1);
        assert!(
            near.field.is_empty(),
            "the raster rides along only when asked"
        );
        let both = terrain_free.windowed(Window::around(230, 40, 400), true);
        assert_eq!(both.plants.len(), 2);
        assert_eq!(both.field, state.field);
        // The window frame encodes smaller and decodes to itself.
        assert!(near.encode().unwrap().len() < both.encode().unwrap().len());
        assert_eq!(VisualFrame::decode(&near.encode().unwrap()).unwrap(), near);
    }

    #[test]
    fn a_saved_tree_keeps_only_its_removed_cells_and_grows_back_identically() {
        let (s, mut a) = grown_tree(9, 3, 60);
        let (_, b) = grown_tree(9, 4, 120);
        assert!(a.wood.len() > 20);
        let last = a.wood.len() - 1;
        std::sync::Arc::make_mut(&mut a.removed)[7] = true;
        std::sync::Arc::make_mut(&mut a.removed)[last] = true;
        a.committed = (a.wood.len() - 3) as u16;
        let state = state_with(vec![a.clone(), b], vec![s]);
        let bytes = state.encode().unwrap();
        // Two trees of dozens of cells each fit in a few hundred bytes.
        assert!(bytes.len() < 700, "{} bytes", bytes.len());
        let back = LivingState::decode(&bytes).unwrap();
        assert_eq!(
            back, state,
            "wood, parents and removed flags regrow exactly"
        );
        assert_eq!(back.organisms[0].wood, a.wood);
    }

    #[test]
    fn a_version_2_save_with_skeletons_loads_and_resaves_compactly() {
        let (s, a) = grown_tree(9, 3, 60);
        let old = LivingStateV2 {
            version: 2,
            seed: 9,
            size: 256,
            time_ms: 5_000,
            credit_ms: 0,
            start_season: Season::Summer,
            season_ms: 900_000,
            next_id: 100,
            cursor: 0,
            species: vec![s],
            organisms: vec![OrganismV2 {
                id: a.id,
                species: a.species,
                root: a.root,
                age_ms: a.age_ms,
                last_update_ms: a.last_update_ms,
                last_seed_ms: a.last_seed_ms,
                alive: true,
                biomass: a.biomass,
                wood: a.wood.to_vec(),
                parents: a.parents.to_vec(),
                removed: (*a.removed).clone(),
                committed: a.committed,
            }],
            spawns: vec![],
            cover: vec![],
            field: CoverField::default(),
            cleared: vec![],
            preset: 1,
        };
        let v2_len = postcard::to_stdvec(&old).unwrap().len();
        let state = LivingState::decode(&postcard::to_stdvec(&old).unwrap()).unwrap();
        assert_eq!(state.version, VERSION);
        assert_eq!(state.organisms, vec![a]);
        let v3 = state.encode().unwrap();
        assert!(v3.len() < v2_len / 2, "{} against {v2_len}", v3.len());
        assert_eq!(LivingState::decode(&v3).unwrap(), state);
    }

    #[test]
    fn an_old_save_whose_tree_is_not_what_the_rule_grows_is_refused() {
        let (s, a) = grown_tree(9, 3, 60);
        let (mut wood, parents) = (a.wood.to_vec(), a.parents.to_vec());
        // Move the last cell to another face neighbour of its parent: still a
        // connected skeleton, but not the one the rule grows.
        let last = wood.len() - 1;
        let parent = wood[usize::from(parents[last])];
        let moved = [[0, 0, 1], [0, 0, -1], [1, 0, 0], [-1, 0, 0]]
            .map(|d| [parent[0] + d[0], parent[1] + d[1], parent[2] + d[2]])
            .into_iter()
            .find(|c| *c != wood[last] && !wood.contains(c))
            .unwrap();
        wood[last] = moved;
        let v2 = LivingStateV2 {
            version: 2,
            seed: 9,
            size: 256,
            time_ms: 5_000,
            credit_ms: 0,
            start_season: Season::Summer,
            season_ms: 900_000,
            next_id: 100,
            cursor: 0,
            species: vec![s],
            organisms: vec![OrganismV2 {
                id: a.id,
                species: a.species,
                root: a.root,
                age_ms: a.age_ms,
                last_update_ms: a.last_update_ms,
                last_seed_ms: a.last_seed_ms,
                alive: true,
                biomass: a.biomass,
                wood,
                parents,
                removed: (*a.removed).clone(),
                committed: a.committed,
            }],
            spawns: vec![],
            cover: vec![],
            field: CoverField::default(),
            cleared: vec![],
            preset: 1,
        };
        let message = LivingState::decode(&postcard::to_stdvec(&v2).unwrap()).unwrap_err();
        assert!(message.contains("skeleton rule"), "{message}");
    }

    #[test]
    fn a_version_1_save_upgrades_by_moving_the_carpet_to_cover_and_keeping_the_rest() {
        let old = LivingStateV1 {
            version: 1,
            seed: 9,
            size: 256,
            time_ms: 123_000,
            credit_ms: 500,
            start_season: Season::Autumn,
            season_ms: 900_000,
            next_id: 40,
            cursor: 3,
            // A scrub (13), a meadow grass (6), a fern (8), a sedge (9), a clover (11).
            species: vec![
                species(1, false, 13),
                species(11, false, 6),
                species(16, false, 8),
                species(14, false, 9),
                species(17, false, 11),
            ],
            organisms: vec![
                organism(1, 1, 40),
                organism(2, 11, 41),
                organism(3, 16, 42),
                organism(4, 14, 43),
                organism(5, 17, 44),
                organism(6, 16, 45),
            ],
            spawns: vec![[50, 9, 50]],
        };
        let bytes = postcard::to_stdvec(&old).unwrap();
        let state = LivingState::decode(&bytes).unwrap();
        assert_eq!(state.version, VERSION);
        // The carpet species moved to cover, with their ids and looks.
        assert_eq!(
            state.cover.iter().map(|c| c.id).collect::<Vec<_>>(),
            [11, 14, 17]
        );
        assert_eq!(state.cover[0].colours, [1, 2, 3, 4]);
        assert_eq!(state.cover[1].form, 9);
        assert_eq!(
            state.species.iter().map(|s| s.id).collect::<Vec<_>>(),
            [1, 16]
        );
        // Their organisms are gone and every other organism is exactly as it was.
        assert_eq!(
            state.organisms,
            vec![
                Organism::from(old.organisms[0].clone()),
                Organism::from(old.organisms[2].clone()),
                Organism::from(old.organisms[5].clone())
            ]
        );
        // Clock, season, ids and spawns carry over; nothing was regenerated.
        assert_eq!(state.time_ms, 123_000);
        assert_eq!(state.credit_ms, 500);
        assert_eq!(state.start_season, Season::Autumn);
        assert_eq!(state.next_id, 40);
        assert_eq!(state.spawns, old.spawns);
        assert!(
            state.field.is_empty(),
            "the raster is sampled when the world installs it"
        );
        // It is a normal current state from here: it re-encodes and decodes.
        assert_eq!(
            LivingState::decode(&state.encode().unwrap()).unwrap(),
            state
        );
    }

    #[test]
    fn a_version_2_state_refuses_a_cover_id_that_is_also_a_species_id() {
        let mut state = LivingState::decode(
            &postcard::to_stdvec(&LivingStateV1 {
                version: 1,
                seed: 1,
                size: 256,
                time_ms: 0,
                credit_ms: 0,
                start_season: Season::Summer,
                season_ms: 900_000,
                next_id: 1,
                cursor: 0,
                species: vec![species(11, false, 6)],
                organisms: vec![],
                spawns: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        state.species.push(species(11, false, 8).into_current());
        assert!(
            state.validate().is_err(),
            "a cover id may not also be a species id"
        );
    }
}
