//! Versioned living vegetation. Authored species belong to the game; this CPU
//! module owns deterministic establishment, reproduction and retained wood paths.
use serde::{Deserialize, Serialize};
use spall_core::{GlobalCell, MaterialId};
use spall_voxel::{EditPlan, Sample, Volume};
use spall_worldgen::{Biome, ColumnMap};

pub const VERSION: u16 = 1;
pub const MAX_TREES: usize = 512;
pub const MAX_GROUND: usize = 8192;
pub const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
pub const PALETTE: [[u8; 3]; 16] = [
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
/// 10 flowering stalk, 11 rosette, 12 cushion, 13 scrub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Species {
    pub id: u16,
    pub tree: bool,
    pub biomes: u8,
    pub soils: [u16; 4],
    pub moisture: [u8; 2],
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
    fn accepts(&self, biome: Biome, moisture: u8, soil: u16) -> bool {
        self.biomes & (1 << biome as u8) != 0
            && self.soils.contains(&soil)
            && (self.moisture[0]..=self.moisture[1]).contains(&moisture)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organism {
    pub id: u64,
    pub species: u16,
    pub root: [i64; 3],
    pub age_ms: u64,
    pub last_update_ms: u64,
    pub last_seed_ms: u64,
    pub alive: bool,
    pub biomass: u16,
    /// Parent-before-child, face-connected. Cuts permanently retire descendants.
    pub wood: Vec<[i64; 3]>,
    pub parents: Vec<u16>,
    pub removed: Vec<bool>,
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
    pub organisms: Vec<Organism>,
    pub spawns: Vec<[i64; 3]>,
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
    pub species: Vec<Species>,
    pub plants: Vec<VisualPlant>,
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
fn cell(p: [i64; 3]) -> GlobalCell {
    GlobalCell::new(p[0], p[1], p[2])
}
fn moisture(columns: &ColumnMap, x: i64, z: i64) -> u8 {
    match columns.biome(x, z) {
        Biome::Meadow => 120,
        Biome::Alpine => 85,
        Biome::Swamp => 210,
        Biome::Desert => 25,
    }
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
        };
        state.validate()?;
        let stride = (i64::from(state.size) / 128).max(4);
        let mut candidates = Vec::new();
        for z in (33..i64::from(state.size) - 33).step_by(stride as usize) {
            for x in (33..i64::from(state.size) - 33).step_by(stride as usize) {
                let h = hash(seed ^ (x as u64).rotate_left(21) ^ z as u64);
                let x = (x + (h % stride as u64) as i64).min(i64::from(state.size) - 33);
                let z = (z + (h.rotate_left(17) % stride as u64) as i64)
                    .min(i64::from(state.size) - 33);
                candidates.push((h, [x, i64::from(columns.height(x, z)) + 1, z]));
            }
        }
        candidates.sort_unstable_by_key(|c| c.0);
        let mut plan = EditPlan::new(terrain.id());
        for (key, root) in candidates {
            // Mixed ages make initial worlds wooded immediately; children grow at runtime.
            for tree in [true, false] {
                let options: Vec<_> = state
                    .species
                    .iter()
                    .filter(|s| s.tree == tree && state.eligible(columns, terrain, s, root))
                    .map(|s| s.id)
                    .collect();
                if options.is_empty() {
                    continue;
                }
                let sid = options[((key / 13) % options.len() as u64) as usize];
                if let Some(index) = state.establish(columns, terrain, sid, root)
                    && tree
                {
                    let p = &mut state.organisms[index];
                    if key % 5 != 0 {
                        p.committed = p.wood.len() as u16;
                        p.age_ms = 600_000;
                        for &at in &p.wood {
                            plan.set(
                                cell(at),
                                MaterialId(
                                    state.species.iter().find(|s| s.id == sid).unwrap().wood,
                                ),
                            );
                        }
                    }
                }
            }
        }
        terrain.apply_edit(&plan).map_err(|e| e.to_string())?;
        Ok(state)
    }
    fn eligible(&self, columns: &ColumnMap, terrain: &Volume, s: &Species, root: [i64; 3]) -> bool {
        let [x, y, z] = root;
        if x < 33
            || z < 33
            || x >= i64::from(self.size) - 33
            || z >= i64::from(self.size) - 33
            || y < 1
            || y + i64::from(s.height) + 8 >= 351
            || columns.is_water(x, z)
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
    fn establish(
        &mut self,
        columns: &ColumnMap,
        terrain: &Volume,
        sid: u16,
        root: [i64; 3],
    ) -> Option<usize> {
        let s = self.species.iter().find(|s| s.id == sid)?;
        let count = self
            .organisms
            .iter()
            .filter(|p| p.wood.is_empty() != s.tree)
            .count();
        if count >= if s.tree { MAX_TREES } else { MAX_GROUND }
            || !self.eligible(columns, terrain, s, root)
            || self
                .organisms
                .iter()
                .filter(|p| p.alive && (p.wood.is_empty() != s.tree))
                .any(|p| {
                    (p.root[0] - root[0]).pow(2) + (p.root[2] - root[2]).pow(2)
                        < i64::from(s.spacing).pow(2)
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
        let p = Organism {
            id: self.next_id,
            species: sid,
            root,
            age_ms: 0,
            last_update_ms: self.time_ms,
            last_seed_ms: self.time_ms,
            alive: true,
            biomass: 100,
            removed: vec![false; wood.len()],
            wood,
            parents,
            committed: 0,
        };
        self.next_id += 1;
        self.organisms.push(p);
        Some(self.organisms.len() - 1)
    }
    pub fn season(&self) -> Season {
        Season::at(self.start_season, self.time_ms, self.season_ms)
    }
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
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
        let s: Self = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
        s.validate()?;
        Ok(s)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != VERSION
            || self.species.len() > 64
            || self.season_ms == 0
            || self.size < 128
            || self.size > 4096
            || self.organisms.len() > MAX_TREES + MAX_GROUND
            || self.spawns.len() > 64
        {
            return Err("invalid vegetation schema/limits".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for s in &self.species {
            if !ids.insert(s.id)
                || s.id == 0
                || s.biomes == 0
                || s.biomes > 15
                || s.form > 13
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
                || p.root
                    .iter()
                    .chain(p.wood.iter().flatten())
                    .any(|c| c.unsigned_abs() > 1_000_000)
                || self.credit_ms >= 1000
                || p.root[0] < 33
                || p.root[2] < 33
                || p.root[0] >= i64::from(self.size) - 33
                || p.root[2] >= i64::from(self.size) - 33
            {
                return Err("invalid organism".into());
            }
            for (i, (&at, &parent)) in p.wood.iter().zip(&p.parents).enumerate() {
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
        if total > 262144 {
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
        self.time_ms = self.time_ms.saturating_add(dt);
        let season = self.season();
        let n = self.organisms.len();
        let mut seeds = Vec::new();
        let mut growth = Vec::new();
        for step in 0..n.min(128) {
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
                    ) => {}
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
                        p.removed[j] = true;
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
            p.age_ms = p.age_ms.saturating_add(due);
            p.last_update_ms = self.time_ms;
            if season != Season::Winter {
                p.biomass = p
                    .biomass
                    .saturating_add((due / 1000).min(50) as u16 * 20)
                    .min(1000);
            }
            if s.tree
                && season != Season::Winter
                && growth.len() < 4
                && p.age_ms >= u64::from(p.committed + 1) * s.growth_ms
            {
                let j = usize::from(p.committed);
                if j < p.wood.len() {
                    if j > 0 && p.removed[usize::from(p.parents[j])] {
                        p.removed[j] = true;
                        p.committed += 1;
                    } else if matches!(terrain.sample(cell(p.wood[j])), Ok(Sample::Empty { .. })) {
                        growth.push(Growth {
                            plant: p.id,
                            index: j as u16,
                            cell: cell(p.wood[j]),
                            material: MaterialId(s.wood),
                        });
                    } else if !matches!(terrain.sample(cell(p.wood[j])), Ok(Sample::Unknown(_))) {
                        p.removed[j] = true;
                        p.committed += 1;
                    }
                }
            }
            let mature = !s.tree || usize::from(p.committed) == p.wood.len();
            if mature
                && season != Season::Winter
                && self.time_ms.saturating_sub(p.last_seed_ms) >= s.seed_ms
            {
                p.last_seed_ms = self.time_ms;
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
            ((self.cursor as usize + n.min(128)) % n) as u32
        };
        self.organisms.retain(|p| p.alive);
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
            species: self.species.clone(),
            plants,
        }
    }
}

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

impl VisualFrame {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(self).map_err(|e| e.to_string())
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("visual byte limit".into());
        }
        let frame: Self = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
        if frame.version != VERSION
            || frame.species.len() > 64
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
                    || s.colours.iter().chain([&s.flower]).any(|c| *c >= 16)
            })
        {
            return Err("visual schema/limits".into());
        }
        Ok(frame)
    }
    /// Deterministic soft geometry, culled before allocation. Leaves occupy quarter
    /// cells; stems/blades are smaller, grounded at the exact top of the soil cell.
    pub fn parts(&self, eye: [f32; 3], radius: f32) -> Vec<SoftPart> {
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
                for tip in &p.tips {
                    let juvenile = tip == &p.root;
                    let r = if juvenile {
                        1
                    } else {
                        i64::from(s.spread).clamp(2, 6)
                    };
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
                                if out.len() >= 150_000 {
                                    return out;
                                }
                                out.push(SoftPart {
                                    center: [
                                        (tip[0] + x) as f32 * 0.25 + 0.125,
                                        (tip[1] + y + 1) as f32 * 0.25 + 0.125,
                                        (tip[2] + z) as f32 * 0.25 + 0.125,
                                    ],
                                    size: [0.25; 3],
                                    colour,
                                    yaw: 0.,
                                    lean: 0.,
                                });
                            }
                        }
                    }
                }
            } else if p.biomass > 0 {
                let height = f32::from(s.height) * 0.25 * f32::from(p.biomass) / 1000.;
                let winter = if self.season == Season::Winter && !s.evergreen {
                    0.25
                } else {
                    1.
                };
                let height = if matches!(s.form, 8 | 11 | 13) {
                    height * winter
                } else {
                    height
                };
                if matches!(s.form, 8 | 11 | 13) {
                    let count = if s.form == 13 { 9 } else { 6 };
                    for leaf in 0..count {
                        let angle = leaf as f32 * std::f32::consts::TAU / count as f32;
                        let reach = height
                            * if s.form == 8 {
                                0.65
                            } else if s.form == 13 {
                                0.40
                            } else {
                                0.75
                            };
                        let level = if s.form == 13 {
                            height * (0.3 + (leaf % 3) as f32 * 0.23)
                        } else {
                            height * 0.3
                        };
                        out.push(SoftPart {
                            center: [base[0], base[1] + level / 2., base[2]],
                            size: [0.02, level, 0.02],
                            colour: if s.form == 13 { 7 } else { colour },
                            yaw: angle,
                            lean: 0.,
                        });
                        out.push(SoftPart {
                            center: [
                                base[0] + angle.cos() * reach / 2.,
                                base[1] + level,
                                base[2] + angle.sin() * reach / 2.,
                            ],
                            size: [reach, 0.02, 0.02],
                            colour: if s.form == 13 { 7 } else { colour },
                            yaw: -angle,
                            lean: 0.,
                        });
                        for segment in 1..=3 {
                            let distance = reach * segment as f32 / 3.;
                            let y = base[1]
                                + level
                                + if s.form == 8 {
                                    height * 0.12 * (3 - segment) as f32
                                } else {
                                    0.
                                };
                            let size = if s.form == 8 {
                                [0.13 * (4 - segment) as f32, 0.025, 0.10]
                            } else {
                                [0.10, 0.04, 0.12]
                            };
                            out.push(SoftPart {
                                center: [
                                    base[0] + angle.cos() * distance,
                                    y,
                                    base[2] + angle.sin() * distance,
                                ],
                                size,
                                colour,
                                yaw: angle,
                                lean: 0.,
                            });
                        }
                    }
                    continue;
                }
                for blade in 0..if s.form == 12 { 13 } else { 7_u64 } {
                    let key = hash(p.id ^ blade);
                    let yaw = (key % 628) as f32 / 100.;
                    let dx = ((key % 101) as f32 / 100. - 0.5) * 0.20;
                    let dz = ((key.rotate_left(23) % 101) as f32 / 100. - 0.5) * 0.20;
                    let h = (height * (0.65 + (key % 35) as f32 / 100.) * winter).max(0.025);
                    let wide = 0.025;
                    let lean = if s.form == 7 {
                        0.04
                    } else {
                        0.12 + (key % 25) as f32 / 100.
                    };
                    out.push(SoftPart {
                        center: [
                            base[0] + dx - h * lean.sin() * yaw.cos() / 2.,
                            base[1] + (h * lean.cos() + wide * lean.sin()) / 2.,
                            base[2] + dz + h * lean.sin() * yaw.sin() / 2.,
                        ],
                        size: [wide, h, 0.025],
                        colour,
                        yaw,
                        lean,
                    });
                    if s.form == 7
                        || (s.form == 10 && matches!(self.season, Season::Spring | Season::Summer))
                    {
                        out.push(SoftPart {
                            center: [
                                base[0] + dx - h * lean.sin() * yaw.cos(),
                                base[1] + h * lean.cos(),
                                base[2] + dz + h * lean.sin() * yaw.sin(),
                            ],
                            size: if s.form == 7 {
                                [0.055, 0.18, 0.055]
                            } else {
                                [0.08, 0.045, 0.08]
                            },
                            colour: if s.form == 7 { 7 } else { s.flower },
                            yaw,
                            lean: 0.,
                        });
                    }
                }
            }
        }
        out
    }
}
