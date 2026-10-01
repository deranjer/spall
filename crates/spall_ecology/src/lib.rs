//! Deterministic, bounded ecological state for grass and voxel trees.
//!
//! Ecology is a separate, versioned plan over terrain. It never changes
//! world-generation output and has no rendering or server-runtime dependency.
//! Soil, explicit moisture and vertical sky exposure are the initial simplified
//! gameplay rules; weather, nutrients and fluid coupling are intentionally absent.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use spall_core::{BrickCoord, GlobalCell, MaterialId, Revision};
use spall_voxel::{Sample, Volume};
use spall_worldgen::{Biome, ColumnMap};
use thiserror::Error;

pub const ECOLOGY_STATE_VERSION: u16 = 1;
pub const ECOLOGY_PLAN_VERSION: u16 = 1;
pub const SPECIES_DEFINITION_VERSION: u16 = 1;
pub const MAX_PATCHES: usize = 65_536;
pub const MAX_PLANTS: usize = 16_384;
pub const MAX_SEEDS: usize = 65_536;
pub const MAX_SKELETON_CELLS_PER_PLANT: usize = 4_096;
pub const MAX_TOTAL_SKELETON_CELLS: usize = 262_144;
pub const MAX_PROPOSAL_CELLS: usize = 256;
pub const SEED_REGION_EDGE_CELLS: i64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpeciesId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeciesDefinition {
    pub version: u16,
    pub id: SpeciesId,
    pub wood: MaterialId,
    pub soil_materials: [MaterialId; 4],
    pub min_moisture: u8,
    pub max_moisture: u8,
    pub min_sky_exposure: u8,
    pub min_spacing_cells: u16,
    pub seed_radius_cells: u16,
    pub seed_lifetime_ms: u64,
    pub seedling_ms: u64,
    pub juvenile_ms: u64,
    pub cell_growth_ms: u64,
}

impl SpeciesDefinition {
    fn accepts_soil(self, material: MaterialId) -> bool {
        self.soil_materials.contains(&material)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrassPatch {
    pub id: u64,
    pub species: SpeciesId,
    pub anchor: GlobalCell,
    pub biomass: u16,
    pub capacity: u16,
    pub last_update_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlantStage {
    Seedling,
    Juvenile,
    Mature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchCell {
    pub cell: GlobalCell,
    pub parent: Option<u32>,
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plant {
    pub id: u64,
    pub species: SpeciesId,
    pub root: GlobalCell,
    pub stage: PlantStage,
    pub age_ms: u64,
    pub health: u16,
    pub root_alive: bool,
    pub skeleton: Vec<BranchCell>,
    pub committed_cells: u32,
    pub growth_credit_ms: u64,
    pub last_seed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedRecord {
    pub id: u64,
    pub species: SpeciesId,
    pub cell: GlobalCell,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeedRegion {
    pub x: i64,
    pub z: i64,
    pub species: SpeciesId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcologyState {
    pub version: u16,
    pub ecological_time_ms: u64,
    pub time_credit_ms: u64,
    pub rng_progress: u64,
    pub next_id: u64,
    pub plants: BTreeMap<u64, Plant>,
    pub grass: BTreeMap<u64, GrassPatch>,
    pub seeds: BTreeMap<u64, SeedRecord>,
    pub regional_seed_availability: BTreeMap<SeedRegion, u32>,
    /// Stable work IDs retained across budget-limited updates.
    pub pending_work: VecDeque<u64>,
    pub work_cursor: u64,
}

impl Default for EcologyState {
    fn default() -> Self {
        Self {
            version: ECOLOGY_STATE_VERSION,
            ecological_time_ms: 0,
            time_credit_ms: 0,
            rng_progress: 0,
            next_id: 1,
            plants: BTreeMap::new(),
            grass: BTreeMap::new(),
            seeds: BTreeMap::new(),
            regional_seed_availability: BTreeMap::new(),
            pending_work: VecDeque::new(),
            work_cursor: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcologyConfig {
    pub update_interval_ms: u64,
    pub max_work_per_update: usize,
    pub max_seed_records: usize,
    pub max_plants: usize,
    pub grass_regrowth_per_interval: u16,
    pub spawn_clearance_cells: u16,
    pub bounds: Option<(GlobalCell, GlobalCell)>,
}

impl Default for EcologyConfig {
    fn default() -> Self {
        Self {
            update_interval_ms: 60_000,
            max_work_per_update: 256,
            max_seed_records: MAX_SEEDS,
            max_plants: MAX_PLANTS,
            grass_regrowth_per_interval: 1,
            spawn_clearance_cells: 12,
            bounds: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EcologyInputs {
    /// Revision of the explicit moisture/clearance/weather input snapshot.
    pub revision: u64,
    /// Explicit moisture level for a root/patch; absent inputs suspend decisions.
    pub moisture: BTreeMap<GlobalCell, u8>,
    /// Cells that are deliberately kept clear for player spawns/structures.
    pub prohibited: BTreeSet<GlobalCell>,
    /// A missing region pauses elapsed ecological time for work in that region.
    pub unloaded_regions: BTreeSet<(i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionDependency {
    pub brick: BrickCoord,
    pub revision: Option<Revision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WoodProposal {
    pub id: u64,
    pub plant_id: u64,
    pub plan_version: u16,
    pub input_revision: u64,
    pub cells: Vec<(GlobalCell, MaterialId)>,
    pub dependencies: Vec<RevisionDependency>,
    pub next_committed_cells: u32,
    pub acknowledged_elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitAck {
    Accepted,
    Rejected,
    Stale,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub processed: usize,
    pub deferred: usize,
    pub suspended_unknown: usize,
    pub seeds_expired: usize,
    pub seeds_germinated: usize,
    pub proposals: usize,
    pub seeds_dispersed: usize,
}

#[derive(Debug, Error)]
pub enum EcologyError {
    #[error("ecology state is too large or exceeds bounded record limits")]
    Capacity,
    #[error("unsupported ecology state version {0}")]
    Version(u16),
    #[error("invalid or truncated ecology encoding")]
    Encoding,
    #[error("update interval must be nonzero")]
    BadInterval,
    #[error("species definition version {0} is unsupported")]
    SpeciesVersion(u16),
    #[error("species growth durations and bounds must be valid")]
    BadSpeciesDefinition,
}

#[derive(Debug, Clone, Copy)]
pub struct InitialPlacementConfig<'a> {
    pub seed: u64,
    pub maximum: usize,
    pub bounds: (GlobalCell, GlobalCell),
    pub spawn_cells: &'a [GlobalCell],
    pub ecology: EcologyConfig,
}

/// Suggest candidates from worldgen columns, but test actual voxel occupancy
/// and revisions before placement. Candidate order and random progress are stable.
pub fn initial_placement(
    columns: &ColumnMap,
    terrain: &Volume,
    species: SpeciesDefinition,
    moisture: &EcologyInputs,
    placement: InitialPlacementConfig<'_>,
    state: &mut EcologyState,
) -> usize {
    if species.version != SPECIES_DEFINITION_VERSION
        || species.cell_growth_ms == 0
        || species.seed_radius_cells < species.min_spacing_cells
    {
        return 0;
    }
    let mut candidates = Vec::new();
    let size = i64::from(columns.size());
    // Candidate density is independent of the requested placement cap; using
    // `maximum` as the stride can accidentally sample a single unsuitable row.
    let stride = (size / 64).max(4);
    let mut z = 32_i64;
    while z < size - 32 {
        let mut x = 32_i64;
        while x < size - 32 {
            if columns.biome(x, z) == Biome::Meadow {
                let h = i64::from(columns.height(x, z));
                let root = GlobalCell::new(x, h + 1, z);
                candidates.push((
                    mix(placement.seed
                        ^ state.rng_progress
                        ^ (x as u64).rotate_left(17)
                        ^ z as u64),
                    root,
                ));
            }
            x += stride;
        }
        z += stride;
    }
    candidates.sort_by_key(|v| (v.0, v.1.z, v.1.x));
    let mut placed = 0;
    let candidate_count = candidates.len();
    for (_, root) in candidates {
        if placed >= placement.maximum || state.plants.len() >= placement.ecology.max_plants {
            break;
        }
        let soil = GlobalCell::new(root.x, root.y - 1, root.z);
        let Ok(Sample::Filled(mat)) = terrain.sample(soil) else {
            continue;
        };
        if !species.accepts_soil(mat)
            || moisture
                .moisture
                .get(&soil)
                .is_none_or(|m| *m < species.min_moisture || *m > species.max_moisture)
        {
            continue;
        }
        if !within(root, placement.bounds)
            || moisture
                .unloaded_regions
                .contains(&(root.x.div_euclid(32 * 16), root.z.div_euclid(32 * 16)))
        {
            continue;
        }
        if placement
            .spawn_cells
            .iter()
            .any(|s| manhattan(*s, root) < i64::from(placement.ecology.spawn_clearance_cells))
        {
            continue;
        }
        if !clear_above(
            terrain,
            root,
            i64::from(placement.ecology.spawn_clearance_cells),
        ) || (0..placement.ecology.spawn_clearance_cells).any(|y| {
            moisture
                .prohibited
                .contains(&GlobalCell::new(root.x, root.y + i64::from(y), root.z))
        }) {
            continue;
        }
        if state
            .plants
            .values()
            .any(|p| manhattan(p.root, root) < i64::from(species.min_spacing_cells))
        {
            continue;
        }
        let id = allocate_id(state);
        let skeleton = tree_skeleton(root);
        state.plants.insert(
            id,
            Plant {
                id,
                species: species.id,
                root,
                stage: PlantStage::Seedling,
                age_ms: 0,
                health: u16::MAX,
                root_alive: true,
                skeleton,
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 0,
            },
        );
        placed += 1;
    }
    state.rng_progress = state.rng_progress.wrapping_add(candidate_count as u64);
    placed
}

pub fn place_grass_patch(
    state: &mut EcologyState,
    species: SpeciesId,
    anchor: GlobalCell,
    capacity: u16,
) -> Result<u64, EcologyError> {
    if state.grass.len() >= MAX_PATCHES {
        return Err(EcologyError::Capacity);
    }
    let id = allocate_id(state);
    state.grass.insert(
        id,
        GrassPatch {
            id,
            species,
            anchor,
            biomass: capacity,
            capacity,
            last_update_ms: state.ecological_time_ms,
        },
    );
    Ok(id)
}

pub fn harvest_grass(state: &mut EcologyState, id: u64, amount: u16) -> u16 {
    let Some(p) = state.grass.get_mut(&id) else {
        return 0;
    };
    let n = p.biomass.min(amount);
    p.biomass -= n;
    n
}

/// Advance deterministic bounded work. Unloaded regions pause time globally in
/// this first increment; unknown samples suspend individual decisions.
pub fn update(
    state: &mut EcologyState,
    terrain: &Volume,
    definitions: &BTreeMap<SpeciesId, SpeciesDefinition>,
    inputs: &EcologyInputs,
    config: EcologyConfig,
    elapsed_ms: u64,
) -> Result<(UpdateReport, Vec<WoodProposal>), EcologyError> {
    validate_limits(state)?;
    if config.update_interval_ms == 0 {
        return Err(EcologyError::BadInterval);
    }
    for definition in definitions.values() {
        if definition.version != SPECIES_DEFINITION_VERSION {
            return Err(EcologyError::SpeciesVersion(definition.version));
        }
        if definition.cell_growth_ms == 0
            || definition.seed_radius_cells < definition.min_spacing_cells
        {
            return Err(EcologyError::BadSpeciesDefinition);
        }
    }
    let mut report = UpdateReport::default();
    let mut proposals = Vec::new();
    if !inputs.unloaded_regions.is_empty() {
        return Ok((report, proposals));
    }
    state.time_credit_ms = state.time_credit_ms.saturating_add(elapsed_ms);
    if state.time_credit_ms < config.update_interval_ms {
        return Ok((report, proposals));
    }
    let intervals = state.time_credit_ms / config.update_interval_ms;
    let advance = intervals * config.update_interval_ms;
    let target = state.ecological_time_ms.saturating_add(advance);
    state.time_credit_ms %= config.update_interval_ms;
    let ids: Vec<u64> = state
        .pending_work
        .drain(..)
        .chain(state.grass.keys().copied())
        .chain(state.plants.keys().copied())
        .collect();
    let mut seen = BTreeSet::new();
    let ids: Vec<u64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
    let mut deferred = VecDeque::new();
    let mut work_used = 0usize;
    let mut seed_deposits = Vec::new();
    let mut ordered = ids;
    if !ordered.is_empty() {
        let rotate = (state.work_cursor as usize) % ordered.len();
        ordered.rotate_left(rotate);
    }
    for id in ordered.iter().copied() {
        if work_used >= config.max_work_per_update {
            deferred.push_back(id);
            continue;
        }
        work_used += 1;
        if let Some(g) = state.grass.get_mut(&id) {
            let soil = GlobalCell::new(g.anchor.x, g.anchor.y - 1, g.anchor.z);
            let def = definitions.get(&g.species);
            if let Some(d) = def {
                match terrain.sample(soil) {
                    Ok(Sample::Filled(m))
                        if d.accepts_soil(m)
                            && inputs
                                .moisture
                                .get(&soil)
                                .is_some_and(|v| *v >= d.min_moisture && *v <= d.max_moisture) =>
                    {
                        g.biomass = g.capacity.min(
                            g.biomass.saturating_add(
                                config
                                    .grass_regrowth_per_interval
                                    .saturating_mul(intervals.min(u64::from(u16::MAX)) as u16),
                            ),
                        )
                    }
                    Ok(Sample::Unknown(_)) => report.suspended_unknown += 1,
                    _ => {}
                }
            }
            g.last_update_ms = target;
            report.processed += 1;
            continue;
        }
        let Some(plant) = state.plants.get_mut(&id) else {
            continue;
        };
        let Some(def) = definitions.get(&plant.species) else {
            continue;
        };
        let soil = GlobalCell::new(plant.root.x, plant.root.y - 1, plant.root.z);
        let mut terrain_reads = BTreeSet::from([soil.split().0]);
        if !plant.root_alive {
            plant.health = plant.health.saturating_sub(1);
            report.processed += 1;
            continue;
        }
        match terrain.sample(soil) {
            Ok(Sample::Unknown(_)) => {
                report.suspended_unknown += 1;
                deferred.push_back(id);
                continue;
            }
            Ok(Sample::Filled(m)) if def.accepts_soil(m) => {}
            _ => {
                plant.health = plant.health.saturating_sub(1);
                report.processed += 1;
                continue;
            }
        }
        let moisture = inputs.moisture.get(&soil);
        if moisture.is_none() {
            report.suspended_unknown += 1;
            deferred.push_back(id);
            continue;
        }
        if moisture.is_some_and(|m| *m < def.min_moisture || *m > def.max_moisture) {
            plant.health = plant.health.saturating_sub(1);
            deferred.push_back(id);
            continue;
        }
        let exposure = sky_exposure(terrain, plant.root, 64, Some(&mut terrain_reads));
        match exposure {
            None => {
                report.suspended_unknown += 1;
                deferred.push_back(id);
                continue;
            }
            Some(v) if v < def.min_sky_exposure => {
                deferred.push_back(id);
                continue;
            }
            _ => {}
        }
        plant.age_ms = plant.age_ms.saturating_add(advance);
        plant.growth_credit_ms = plant.growth_credit_ms.saturating_add(advance);
        if plant.age_ms >= def.seedling_ms.saturating_add(def.juvenile_ms) {
            plant.stage = PlantStage::Mature
        } else if plant.age_ms >= def.seedling_ms {
            plant.stage = PlantStage::Juvenile
        }
        let available = plant.growth_credit_ms / def.cell_growth_ms.max(1);
        let room = plant
            .skeleton
            .iter()
            .enumerate()
            .skip(plant.committed_cells as usize)
            .filter(|(_, c)| !c.removed)
            .count();
        let count = available.min(room as u64).min(MAX_PROPOSAL_CELLS as u64) as usize;
        if count > 0 {
            let selected: Vec<_> = plant
                .skeleton
                .iter()
                .enumerate()
                .skip(plant.committed_cells as usize)
                .filter(|(_, c)| !c.removed)
                .take(count)
                .map(|(i, c)| (i, c.cell))
                .collect();
            let mut cells = Vec::new();
            let mut deps: BTreeMap<BrickCoord, Option<Revision>> = terrain_reads
                .iter()
                .map(|brick| (*brick, terrain.brick_revision(*brick).ok().flatten()))
                .collect();
            let mut valid = true;
            for (idx, cell) in selected {
                if config.bounds.is_some_and(|b| !within(cell, b))
                    || inputs.prohibited.contains(&cell)
                {
                    if idx == 0 {
                        plant.root_alive = false;
                        plant.health = 0
                    } else {
                        mark_subtree_removed(&mut plant.skeleton, idx);
                    }
                    continue;
                }
                match terrain.sample(cell) {
                    Ok(Sample::Empty { .. }) => cells.push((cell, def.wood)),
                    Ok(Sample::Unknown(_)) => {
                        valid = false;
                        report.suspended_unknown += 1;
                        break;
                    }
                    _ => {
                        if idx == 0 {
                            plant.root_alive = false;
                            plant.health = 0
                        } else {
                            mark_subtree_removed(&mut plant.skeleton, idx);
                        }
                    }
                }
                let (brick, _) = cell.split();
                deps.insert(brick, terrain.brick_revision(brick).ok().flatten());
            }
            if valid && !cells.is_empty() {
                let next = selected_end(&plant.skeleton, plant.committed_cells as usize, count);
                let required_growth_ms = (cells.len() as u64).saturating_mul(def.cell_growth_ms);
                proposals.push(WoodProposal {
                    id: proposal_id(id, plant.committed_cells, target),
                    plant_id: id,
                    plan_version: ECOLOGY_PLAN_VERSION,
                    input_revision: inputs.revision,
                    cells,
                    dependencies: deps
                        .into_iter()
                        .map(|(brick, revision)| RevisionDependency { brick, revision })
                        .collect(),
                    next_committed_cells: next,
                    acknowledged_elapsed_ms: required_growth_ms,
                });
                report.proposals += 1;
            }
        }
        if plant.stage == PlantStage::Mature
            && plant.root_alive
            && target.saturating_sub(plant.last_seed_ms) >= def.cell_growth_ms.saturating_mul(8)
            && state.seeds.len() < config.max_seed_records
        {
            let seed_key = mix(id ^ state.rng_progress ^ target);
            let (dx, dz) = seed_offset(seed_key, def.seed_radius_cells, def.min_spacing_cells);
            let cell = GlobalCell::new(plant.root.x + dx, plant.root.y, plant.root.z + dz);
            plant.last_seed_ms = target;
            seed_deposits.push((plant.species, cell, def.seed_lifetime_ms));
        }
        report.processed += 1;
    }
    for (species, cell, lifetime) in seed_deposits {
        if state.seeds.len() >= config.max_seed_records {
            break;
        }
        let seed_id = allocate_id(state);
        if config.bounds.is_some_and(|b| !within(cell, b))
            || state.seeds.values().any(|existing| existing.cell == cell)
        {
            continue;
        }
        state.seeds.insert(
            seed_id,
            SeedRecord {
                id: seed_id,
                species,
                cell,
                expires_at_ms: target.saturating_add(lifetime),
            },
        );
        *state
            .regional_seed_availability
            .entry(SeedRegion {
                x: cell.x.div_euclid(SEED_REGION_EDGE_CELLS),
                z: cell.z.div_euclid(SEED_REGION_EDGE_CELLS),
                species,
            })
            .or_default() += 1;
        report.seeds_dispersed += 1;
    }
    state.pending_work = deferred;
    report.deferred = state.pending_work.len();
    let mut seed_ids: Vec<u64> = state.seeds.keys().copied().collect();
    if !seed_ids.is_empty() {
        let rotation = state.work_cursor as usize % seed_ids.len();
        seed_ids.rotate_left(rotation);
    }
    let candidates: Vec<u64> = seed_ids
        .into_iter()
        .take(config.max_work_per_update.saturating_sub(work_used))
        .collect();
    for id in candidates {
        work_used += 1;
        report.processed += 1;
        let Some(seed) = state.seeds.get(&id).cloned() else {
            continue;
        };
        if seed.expires_at_ms <= target {
            state.seeds.remove(&id);
            decrement_region(state, &seed);
            report.seeds_expired += 1;
            continue;
        }
        let Some(def) = definitions.get(&seed.species) else {
            continue;
        };
        let (root, unknown) = germination_root(terrain, inputs, config, state, &seed, *def);
        let Some(root) = root else {
            report.suspended_unknown += usize::from(unknown);
            continue;
        };
        if state.plants.len() >= config.max_plants {
            break;
        }
        let pid = allocate_id(state);
        let plant = Plant {
            id: pid,
            species: seed.species,
            root,
            stage: PlantStage::Seedling,
            age_ms: 0,
            health: u16::MAX,
            root_alive: true,
            skeleton: tree_skeleton(root),
            committed_cells: 0,
            growth_credit_ms: 0,
            last_seed_ms: 0,
        };
        state.plants.insert(pid, plant);
        state.seeds.remove(&id);
        decrement_region(state, &seed);
        report.seeds_germinated += 1;
    }
    state.work_cursor = state.work_cursor.wrapping_add(work_used as u64);
    state.ecological_time_ms = target;
    state.rng_progress = state.rng_progress.wrapping_add(report.processed as u64);
    Ok((report, proposals))
}

fn germination_root(
    terrain: &Volume,
    inputs: &EcologyInputs,
    config: EcologyConfig,
    state: &EcologyState,
    seed: &SeedRecord,
    species: SpeciesDefinition,
) -> (Option<GlobalCell>, bool) {
    let mut unknown = false;
    for dy in -32..=32 {
        let Some(y) = seed.cell.y.checked_add(dy) else {
            continue;
        };
        let soil = GlobalCell::new(seed.cell.x, y, seed.cell.z);
        let material = match terrain.sample(soil) {
            Ok(Sample::Filled(m)) => m,
            Ok(Sample::Unknown(_)) => {
                unknown = true;
                continue;
            }
            _ => continue,
        };
        if !species.accepts_soil(material) {
            continue;
        }
        let Some(moisture) = inputs.moisture.get(&soil) else {
            unknown = true;
            continue;
        };
        if *moisture < species.min_moisture || *moisture > species.max_moisture {
            continue;
        }
        let Some(root_y) = soil.y.checked_add(1) else {
            continue;
        };
        let root = GlobalCell::new(soil.x, root_y, soil.z);
        if config.bounds.is_some_and(|b| !within(root, b))
            || inputs.prohibited.contains(&root)
            || !clear_above(terrain, root, 8)
        {
            continue;
        }
        if !clear_spacing(state, root, species.min_spacing_cells) {
            continue;
        }
        match sky_exposure(terrain, root, 64, None) {
            Some(n) if n >= species.min_sky_exposure => return (Some(root), unknown),
            None => unknown = true,
            _ => {}
        }
    }
    (None, unknown)
}

fn decrement_region(state: &mut EcologyState, seed: &SeedRecord) {
    let key = SeedRegion {
        x: seed.cell.x.div_euclid(SEED_REGION_EDGE_CELLS),
        z: seed.cell.z.div_euclid(SEED_REGION_EDGE_CELLS),
        species: seed.species,
    };
    if let Some(count) = state.regional_seed_availability.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.regional_seed_availability.remove(&key);
        }
    }
}

/// Acknowledgment is the only operation that advances persistent geometry progress.
pub fn acknowledge(state: &mut EcologyState, proposal: &WoodProposal, ack: CommitAck) -> bool {
    if ack != CommitAck::Accepted {
        return false;
    }
    let Some(p) = state.plants.get_mut(&proposal.plant_id) else {
        return false;
    };
    p.committed_cells = p.committed_cells.max(proposal.next_committed_cells);
    p.growth_credit_ms = p
        .growth_credit_ms
        .saturating_sub(proposal.acknowledged_elapsed_ms);
    true
}

/// Mark a branch cell and its descendants removed permanently.
pub fn cut_branch(state: &mut EcologyState, plant_id: u64, index: usize) -> bool {
    let Some(p) = state.plants.get_mut(&plant_id) else {
        return false;
    };
    if index == 0 || index >= p.skeleton.len() {
        return false;
    }
    mark_subtree_removed(&mut p.skeleton, index);
    true
}

fn mark_subtree_removed(skeleton: &mut [BranchCell], index: usize) {
    let mut removed = BTreeSet::from([index as u32]);
    loop {
        let n = skeleton
            .iter()
            .enumerate()
            .filter(|(_, c)| c.parent.is_some_and(|v| removed.contains(&v)))
            .map(|(i, _)| i as u32)
            .filter(|i| !removed.contains(i))
            .collect::<Vec<_>>();
        if n.is_empty() {
            break;
        }
        removed.extend(n);
    }
    for i in removed {
        skeleton[i as usize].removed = true;
    }
}

/// Root destruction disables future growth and reproduction.
pub fn destroy_root(state: &mut EcologyState, plant_id: u64) -> bool {
    let Some(p) = state.plants.get_mut(&plant_id) else {
        return false;
    };
    p.root_alive = false;
    p.health = 0;
    true
}

pub fn dependencies_current(terrain: &Volume, deps: &[RevisionDependency]) -> bool {
    deps.iter()
        .all(|d| terrain.brick_revision(d.brick).ok().flatten() == d.revision)
}

pub fn proposal_current(terrain: &Volume, inputs: &EcologyInputs, proposal: &WoodProposal) -> bool {
    proposal.input_revision == inputs.revision
        && dependencies_current(terrain, &proposal.dependencies)
}

pub fn canonical_digest(state: &EcologyState) -> Result<[u8; 32], EcologyError> {
    let bytes = encode_state(state)?;
    Ok(*blake3::hash(&bytes).as_bytes())
}

/// Explicit little-endian versioned state representation. Records are emitted
/// in BTreeMap order; decoder rejects unknown versions, excess counts and tails.
pub fn encode_state(state: &EcologyState) -> Result<Vec<u8>, EcologyError> {
    validate_limits(state)?;
    let mut b = Vec::new();
    b.extend_from_slice(b"SPAE");
    put_u16(&mut b, state.version);
    put_u64(&mut b, state.ecological_time_ms);
    put_u64(&mut b, state.time_credit_ms);
    put_u64(&mut b, state.rng_progress);
    put_u64(&mut b, state.next_id);
    put_u64(&mut b, state.work_cursor);
    put_u32(&mut b, state.grass.len() as u32);
    for p in state.grass.values() {
        put_u64(&mut b, p.id);
        put_u16(&mut b, p.species.0);
        put_cell(&mut b, p.anchor);
        put_u16(&mut b, p.biomass);
        put_u16(&mut b, p.capacity);
        put_u64(&mut b, p.last_update_ms)
    }
    put_u32(&mut b, state.plants.len() as u32);
    for p in state.plants.values() {
        put_u64(&mut b, p.id);
        put_u16(&mut b, p.species.0);
        put_cell(&mut b, p.root);
        b.push(p.stage as u8);
        put_u64(&mut b, p.age_ms);
        put_u16(&mut b, p.health);
        b.push(p.root_alive as u8);
        put_u32(&mut b, p.committed_cells);
        put_u64(&mut b, p.growth_credit_ms);
        put_u64(&mut b, p.last_seed_ms);
        put_u32(&mut b, p.skeleton.len() as u32);
        for c in &p.skeleton {
            put_cell(&mut b, c.cell);
            put_u32(&mut b, c.parent.unwrap_or(u32::MAX));
            b.push(c.removed as u8)
        }
    }
    put_u32(&mut b, state.seeds.len() as u32);
    for s in state.seeds.values() {
        put_u64(&mut b, s.id);
        put_u16(&mut b, s.species.0);
        put_cell(&mut b, s.cell);
        put_u64(&mut b, s.expires_at_ms)
    }
    put_u32(&mut b, state.regional_seed_availability.len() as u32);
    for (r, n) in &state.regional_seed_availability {
        put_i64(&mut b, r.x);
        put_i64(&mut b, r.z);
        put_u16(&mut b, r.species.0);
        put_u32(&mut b, *n)
    }
    put_u32(&mut b, state.pending_work.len() as u32);
    for id in &state.pending_work {
        put_u64(&mut b, *id)
    }
    Ok(b)
}

pub fn decode_state(bytes: &[u8]) -> Result<EcologyState, EcologyError> {
    let mut r = Reader { b: bytes, i: 0 };
    if r.take(4)? != b"SPAE" {
        return Err(EcologyError::Encoding);
    }
    let version = r.u16()?;
    if version != ECOLOGY_STATE_VERSION {
        return Err(EcologyError::Version(version));
    }
    let mut s = EcologyState {
        version,
        ecological_time_ms: r.u64()?,
        time_credit_ms: r.u64()?,
        rng_progress: r.u64()?,
        next_id: r.u64()?,
        work_cursor: r.u64()?,
        ..Default::default()
    };
    for _ in 0..r.count(MAX_PATCHES)? {
        let id = r.u64()?;
        let species = SpeciesId(r.u16()?);
        let anchor = r.cell()?;
        let biomass = r.u16()?;
        let capacity = r.u16()?;
        let last_update_ms = r.u64()?;
        s.grass.insert(
            id,
            GrassPatch {
                id,
                species,
                anchor,
                biomass,
                capacity,
                last_update_ms,
            },
        );
    }
    for _ in 0..r.count(MAX_PLANTS)? {
        let id = r.u64()?;
        let species = SpeciesId(r.u16()?);
        let root = r.cell()?;
        let stage = match r.u8()? {
            0 => PlantStage::Seedling,
            1 => PlantStage::Juvenile,
            2 => PlantStage::Mature,
            _ => return Err(EcologyError::Encoding),
        };
        let age_ms = r.u64()?;
        let health = r.u16()?;
        let root_alive = r.u8()? != 0;
        let committed_cells = r.u32()?;
        let growth_credit_ms = r.u64()?;
        let last_seed_ms = r.u64()?;
        let mut skeleton = Vec::new();
        for _ in 0..r.count(MAX_SKELETON_CELLS_PER_PLANT)? {
            let cell = r.cell()?;
            let parent = match r.u32()? {
                u32::MAX => None,
                n => Some(n),
            };
            let removed = r.u8()? != 0;
            skeleton.push(BranchCell {
                cell,
                parent,
                removed,
            })
        }
        s.plants.insert(
            id,
            Plant {
                id,
                species,
                root,
                stage,
                age_ms,
                health,
                root_alive,
                skeleton,
                committed_cells,
                growth_credit_ms,
                last_seed_ms,
            },
        );
    }
    for _ in 0..r.count(MAX_SEEDS)? {
        let id = r.u64()?;
        let species = SpeciesId(r.u16()?);
        let cell = r.cell()?;
        let expires_at_ms = r.u64()?;
        s.seeds.insert(
            id,
            SeedRecord {
                id,
                species,
                cell,
                expires_at_ms,
            },
        );
    }
    for _ in 0..r.count(MAX_SEEDS)? {
        let region = SeedRegion {
            x: r.i64()?,
            z: r.i64()?,
            species: SpeciesId(r.u16()?),
        };
        s.regional_seed_availability.insert(region, r.u32()?);
    }
    for _ in 0..r.count(MAX_PLANTS + MAX_PATCHES)? {
        s.pending_work.push_back(r.u64()?)
    }
    if r.i != bytes.len() {
        return Err(EcologyError::Encoding);
    }
    validate_limits(&s)?;
    Ok(s)
}

fn validate_limits(s: &EcologyState) -> Result<(), EcologyError> {
    let skeleton_cells: usize = s.plants.values().map(|p| p.skeleton.len()).sum();
    if s.version != ECOLOGY_STATE_VERSION {
        return Err(EcologyError::Version(s.version));
    }
    if s.grass.len() > MAX_PATCHES
        || s.plants.len() > MAX_PLANTS
        || s.seeds.len() > MAX_SEEDS
        || s.plants
            .values()
            .any(|p| p.skeleton.len() > MAX_SKELETON_CELLS_PER_PLANT)
        || skeleton_cells > MAX_TOTAL_SKELETON_CELLS
        || s.pending_work.len() > MAX_PLANTS + MAX_PATCHES
    {
        return Err(EcologyError::Capacity);
    }
    Ok(())
}
fn tree_skeleton(root: GlobalCell) -> Vec<BranchCell> {
    let mut cells = Vec::new();
    for y in 0..6 {
        cells.push(BranchCell {
            cell: GlobalCell::new(root.x, root.y + y, root.z),
            parent: (y > 0).then_some((y - 1) as u32),
            removed: false,
        });
    }
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        let parent = 4;
        let first = cells.len() as u32;
        cells.push(BranchCell {
            cell: GlobalCell::new(root.x + dx, root.y + 4, root.z + dz),
            parent: Some(parent),
            removed: false,
        });
        cells.push(BranchCell {
            cell: GlobalCell::new(root.x + 2 * dx, root.y + 4, root.z + 2 * dz),
            parent: Some(first),
            removed: false,
        });
    }
    cells
}
fn selected_end(skeleton: &[BranchCell], start: usize, count: usize) -> u32 {
    skeleton
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, c)| !c.removed)
        .take(count)
        .map(|(i, _)| i as u32 + 1)
        .last()
        .unwrap_or(start as u32)
}
fn clear_above(v: &Volume, root: GlobalCell, n: i64) -> bool {
    (0..n).all(|i| {
        matches!(
            v.sample(GlobalCell::new(root.x, root.y + i, root.z)),
            Ok(Sample::Empty { .. })
        )
    })
}
fn sky_exposure(
    v: &Volume,
    root: GlobalCell,
    n: i64,
    mut reads: Option<&mut BTreeSet<BrickCoord>>,
) -> Option<u8> {
    // Check the four adjacent columns plus the diagonals so the trunk's own
    // accepted voxels do not shadow itself. A full Unknown neighborhood
    // remains suspended instead of being interpreted as open sky.
    let offsets = [
        (1, 0),
        (-1, 0),
        (0, 1),
        (0, -1),
        (1, 1),
        (-1, 1),
        (1, -1),
        (-1, -1),
    ];
    let mut best = 0;
    let mut known = false;
    for (dx, dz) in offsets {
        let mut clear = 0;
        let mut complete = true;
        for y in root.y..root.y.saturating_add(n) {
            let cell = GlobalCell::new(root.x + dx, y, root.z + dz);
            if let Some(reads) = reads.as_deref_mut() {
                reads.insert(cell.split().0);
            }
            match v.sample(cell) {
                Ok(Sample::Empty { .. }) => clear += 1,
                Ok(Sample::Unknown(_)) => {
                    complete = false;
                    break;
                }
                _ => break,
            }
        }
        if complete {
            known = true;
            best = best.max(clear);
        }
    }
    known.then_some(best.min(255) as u8)
}
fn within(c: GlobalCell, b: (GlobalCell, GlobalCell)) -> bool {
    c.x >= b.0.x && c.y >= b.0.y && c.z >= b.0.z && c.x <= b.1.x && c.y <= b.1.y && c.z <= b.1.z
}
fn manhattan(a: GlobalCell, b: GlobalCell) -> i64 {
    (a.x - b.x).abs() + (a.y - b.y).abs() + (a.z - b.z).abs()
}
fn clear_spacing(s: &EcologyState, c: GlobalCell, d: u16) -> bool {
    s.plants
        .values()
        .all(|p| manhattan(p.root, c) >= i64::from(d))
}
fn allocate_id(s: &mut EcologyState) -> u64 {
    let id = s.next_id;
    s.next_id = s.next_id.wrapping_add(1).max(1);
    id
}
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}
fn seed_offset(key: u64, radius: u16, spacing: u16) -> (i64, i64) {
    let r = i64::from(radius.max(1));
    let min = i64::from(spacing);
    for i in 0..32 {
        let x = (mix(key.wrapping_add(i * 2)) % (2 * r as u64 + 1)) as i64 - r;
        let z = (mix(key.wrapping_add(i * 2 + 1)) % (2 * r as u64 + 1)) as i64 - r;
        if x * x + z * z >= min * min {
            return (x, z);
        }
    }
    if min <= r {
        if key & 1 == 0 { (min, 0) } else { (-min, 0) }
    } else {
        (r, 0)
    }
}
fn proposal_id(tree: u64, progress: u32, time: u64) -> u64 {
    mix(tree ^ u64::from(progress).rotate_left(21) ^ time)
}
fn put_u16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes())
}
fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes())
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes())
}
fn put_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes())
}
fn put_cell(b: &mut Vec<u8>, c: GlobalCell) {
    put_i64(b, c.x);
    put_i64(b, c.y);
    put_i64(b, c.z)
}
struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], EcologyError> {
        let e = self.i.checked_add(n).ok_or(EcologyError::Encoding)?;
        let r = self.b.get(self.i..e).ok_or(EcologyError::Encoding)?;
        self.i = e;
        Ok(r)
    }
    fn u8(&mut self) -> Result<u8, EcologyError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, EcologyError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, EcologyError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, EcologyError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, EcologyError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn cell(&mut self) -> Result<GlobalCell, EcologyError> {
        Ok(GlobalCell::new(self.i64()?, self.i64()?, self.i64()?))
    }
    fn count(&mut self, max: usize) -> Result<usize, EcologyError> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(EcologyError::Capacity);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{CellSizeCode, VolumeId};
    use spall_voxel::EditPlan;

    fn terrain() -> Volume {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 0, 0),
            GlobalCell::new(31, 0, 31),
            MaterialId(1),
        ))
        .unwrap();
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(0, 1, 0),
            GlobalCell::new(31, 80, 31),
            MaterialId::AIR,
        ))
        .unwrap();
        v
    }
    fn species() -> SpeciesDefinition {
        SpeciesDefinition {
            version: 1,
            id: SpeciesId(1),
            wood: MaterialId(3),
            soil_materials: [MaterialId(1); 4],
            min_moisture: 20,
            max_moisture: 200,
            min_sky_exposure: 8,
            min_spacing_cells: 4,
            seed_radius_cells: 8,
            seed_lifetime_ms: 5_000,
            seedling_ms: 100,
            juvenile_ms: 100,
            cell_growth_ms: 100,
        }
    }
    fn inputs() -> EcologyInputs {
        let mut i = EcologyInputs::default();
        for z in 0..32 {
            for x in 0..32 {
                i.moisture.insert(GlobalCell::new(x, 0, z), 100);
            }
        }
        i
    }
    #[test]
    fn versioned_state_roundtrip_preserves_continuation_fields() {
        let mut s = EcologyState {
            ecological_time_ms: 123,
            rng_progress: 88,
            ..EcologyState::default()
        };
        s.pending_work.push_back(7);
        place_grass_patch(&mut s, SpeciesId(1), GlobalCell::new(1, 2, 3), 10).unwrap();
        let bytes = encode_state(&s).unwrap();
        assert_eq!(decode_state(&bytes).unwrap(), s);
        assert_eq!(
            canonical_digest(&s).unwrap(),
            canonical_digest(&decode_state(&bytes).unwrap()).unwrap()
        );
    }
    #[test]
    fn rejected_or_stale_proposals_do_not_commit_growth() {
        let mut s = EcologyState::default();
        let id = allocate_id(&mut s);
        s.plants.insert(
            id,
            Plant {
                id,
                species: SpeciesId(1),
                root: GlobalCell::new(0, 0, 0),
                stage: PlantStage::Juvenile,
                age_ms: 0,
                health: 10,
                root_alive: true,
                skeleton: tree_skeleton(GlobalCell::new(0, 0, 0)),
                committed_cells: 0,
                growth_credit_ms: 100,
                last_seed_ms: 0,
            },
        );
        let p = WoodProposal {
            id: 1,
            plant_id: id,
            plan_version: 1,
            input_revision: 0,
            cells: vec![],
            dependencies: vec![],
            next_committed_cells: 1,
            acknowledged_elapsed_ms: 50,
        };
        assert!(!acknowledge(&mut s, &p, CommitAck::Stale));
        assert!(!acknowledge(&mut s, &p, CommitAck::Rejected));
        assert_eq!(s.plants[&id].committed_cells, 0);
        assert!(acknowledge(&mut s, &p, CommitAck::Accepted));
        assert_eq!(s.plants[&id].committed_cells, 1);
    }

    #[test]
    fn grass_only_regrows_on_suitable_moist_soil() {
        let v = terrain();
        let mut s = EcologyState::default();
        let at = GlobalCell::new(8, 1, 8);
        let id = place_grass_patch(&mut s, SpeciesId(1), at, 10).unwrap();
        assert_eq!(harvest_grass(&mut s, id, 6), 6);
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let cfg = EcologyConfig {
            update_interval_ms: 1000,
            grass_regrowth_per_interval: 2,
            ..Default::default()
        };
        let mut i = inputs();
        update(&mut s, &v, &defs, &i, cfg, 1000).unwrap();
        assert_eq!(s.grass[&id].biomass, 6);
        i.moisture.insert(GlobalCell::new(8, 0, 8), 0);
        update(&mut s, &v, &defs, &i, cfg, 1000).unwrap();
        assert_eq!(s.grass[&id].biomass, 6);
    }

    #[test]
    fn bounded_updates_accumulate_time_and_rotate_deferred_patches_fairly() {
        let v = terrain();
        let mut s = EcologyState::default();
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let mut i = inputs();
        let mut ids = Vec::new();
        for x in [4, 8, 12, 16] {
            let at = GlobalCell::new(x, 1, 4);
            i.moisture.insert(GlobalCell::new(x, 0, 4), 100);
            ids.push(place_grass_patch(&mut s, SpeciesId(1), at, 20).unwrap());
        }
        let cfg = EcologyConfig {
            update_interval_ms: 1000,
            max_work_per_update: 1,
            grass_regrowth_per_interval: 1,
            ..Default::default()
        };
        update(&mut s, &v, &defs, &i, cfg, 500).unwrap();
        assert_eq!(s.ecological_time_ms, 0);
        update(&mut s, &v, &defs, &i, cfg, 500).unwrap();
        assert_eq!(s.ecological_time_ms, 1000);
        assert_eq!(s.pending_work.len(), 3);
        for _ in 0..8 {
            update(&mut s, &v, &defs, &i, cfg, 1000).unwrap();
        }
        assert!(
            ids.iter().all(|id| s.grass[id].biomass > 0),
            "each deferred patch eventually regrows"
        );
    }

    #[test]
    fn rejected_stale_and_accepted_proposals_obey_geometry_acknowledgment() {
        let mut v = terrain();
        let mut s = EcologyState::default();
        let id = allocate_id(&mut s);
        let root = GlobalCell::new(10, 1, 10);
        s.plants.insert(
            id,
            Plant {
                id,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Seedling,
                age_ms: 0,
                health: 20,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 0,
            },
        );
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let cfg = EcologyConfig {
            update_interval_ms: 100,
            ..Default::default()
        };
        let (mut s1, mut s2) = (s.clone(), s);
        let (_, p1) = update(&mut s1, &v, &defs, &inputs(), cfg, 100).unwrap();
        let (_, p2) = update(&mut s2, &v, &defs, &inputs(), cfg, 100).unwrap();
        let proposal = p1[0].clone();
        assert_eq!(proposal, p2[0]);
        assert!(dependencies_current(&v, &proposal.dependencies));
        assert!(!acknowledge(&mut s1, &proposal, CommitAck::Rejected));
        v.apply_edit(&EditPlan::filled_box(
            v.id(),
            GlobalCell::new(11, 10, 11),
            GlobalCell::new(11, 10, 11),
            MaterialId(1),
        ))
        .unwrap();
        assert!(!dependencies_current(&v, &proposal.dependencies));
        assert!(!acknowledge(&mut s1, &proposal, CommitAck::Stale));
        assert_eq!(s1.plants[&id].committed_cells, 0);
        assert!(acknowledge(&mut s1, &proposal, CommitAck::Accepted));
        assert_eq!(s1.plants[&id].committed_cells, 1);
    }

    #[test]
    fn save_restore_midway_matches_uninterrupted_state_and_proposals() {
        let v = terrain();
        let mut s = EcologyState::default();
        let root = GlobalCell::new(12, 1, 12);
        let id = allocate_id(&mut s);
        s.plants.insert(
            id,
            Plant {
                id,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Juvenile,
                age_ms: 50,
                health: 20,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 0,
            },
        );
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let cfg = EcologyConfig {
            update_interval_ms: 100,
            ..Default::default()
        };
        let i = inputs();
        update(&mut s, &v, &defs, &i, cfg, 250).unwrap();
        let mut resumed = decode_state(&encode_state(&s).unwrap()).unwrap();
        let mut continuous = s;
        let a = update(&mut resumed, &v, &defs, &i, cfg, 450).unwrap();
        let b = update(&mut continuous, &v, &defs, &i, cfg, 450).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            canonical_digest(&resumed).unwrap(),
            canonical_digest(&continuous).unwrap()
        );
    }

    #[test]
    fn unknown_soil_suspends_and_unloaded_regions_pause_clock() {
        let v = Volume::new(VolumeId::new(1).unwrap(), CellSizeCode::Quarter);
        let mut s = EcologyState::default();
        let root = GlobalCell::new(2, 2, 2);
        let id = allocate_id(&mut s);
        s.plants.insert(
            id,
            Plant {
                id,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Seedling,
                age_ms: 0,
                health: 20,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 0,
            },
        );
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let (r, _) = update(
            &mut s,
            &v,
            &defs,
            &inputs(),
            EcologyConfig {
                update_interval_ms: 10,
                ..Default::default()
            },
            10,
        )
        .unwrap();
        assert!(r.proposals == 0);
        assert_eq!(s.plants[&id].age_ms, 0);
        let mut i = inputs();
        i.unloaded_regions.insert((0, 0));
        let before = s.ecological_time_ms;
        update(
            &mut s,
            &terrain(),
            &defs,
            &i,
            EcologyConfig {
                update_interval_ms: 10,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        assert_eq!(s.ecological_time_ms, before);
    }

    #[test]
    fn branch_cut_is_permanent_and_every_skeleton_link_is_face_connected() {
        let root = GlobalCell::new(10, 1, 10);
        let mut s = EcologyState::default();
        let id = allocate_id(&mut s);
        s.plants.insert(
            id,
            Plant {
                id,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Mature,
                age_ms: 0,
                health: 10,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 0,
            },
        );
        for (index, cell) in s.plants[&id].skeleton.iter().enumerate().skip(1) {
            let parent = cell.parent.unwrap() as usize;
            assert_eq!(
                manhattan(cell.cell, s.plants[&id].skeleton[parent].cell),
                1,
                "skeleton link {index} is face-adjacent"
            );
        }
        assert!(cut_branch(&mut s, id, 6));
        assert!(s.plants[&id].skeleton[6..].iter().any(|c| c.removed));
        let before = s.plants[&id].skeleton.clone();
        assert!(destroy_root(&mut s, id));
        assert!(!s.plants[&id].root_alive);
        assert_eq!(s.plants[&id].skeleton, before);
    }

    #[test]
    fn crowding_blocks_seedling_establishment() {
        let v = terrain();
        let mut s = EcologyState::default();
        let root = GlobalCell::new(10, 1, 10);
        let adult = allocate_id(&mut s);
        s.plants.insert(
            adult,
            Plant {
                id: adult,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Mature,
                age_ms: 1000,
                health: 10,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
                last_seed_ms: 1000,
            },
        );
        let seed_id = allocate_id(&mut s);
        let seed_cell = GlobalCell::new(11, 1, 10);
        s.seeds.insert(
            seed_id,
            SeedRecord {
                id: seed_id,
                species: SpeciesId(1),
                cell: seed_cell,
                expires_at_ms: 10000,
            },
        );
        let mut defs = BTreeMap::new();
        defs.insert(SpeciesId(1), species());
        let cfg = EcologyConfig {
            update_interval_ms: 100,
            ..Default::default()
        };
        update(&mut s, &v, &defs, &inputs(), cfg, 100).unwrap();
        assert_eq!(s.plants.len(), 1);
        assert!(s.seeds.contains_key(&seed_id));
    }

    #[test]
    fn mature_tree_seeds_cross_region_without_duplicate_availability() {
        let id = VolumeId::new(1).unwrap();
        let mut v = Volume::new(id, CellSizeCode::Quarter);
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(480, 0, 0),
            GlobalCell::new(543, 0, 31),
            MaterialId(1),
        ))
        .unwrap();
        v.apply_edit(&EditPlan::filled_box(
            id,
            GlobalCell::new(480, 1, 0),
            GlobalCell::new(543, 80, 31),
            MaterialId::AIR,
        ))
        .unwrap();
        let mut s = EcologyState::default();
        let root = GlobalCell::new(510, 1, 12);
        let pid = allocate_id(&mut s);
        s.plants.insert(
            pid,
            Plant {
                id: pid,
                species: SpeciesId(1),
                root,
                stage: PlantStage::Mature,
                age_ms: 500,
                last_seed_ms: 0,
                health: 20,
                root_alive: true,
                skeleton: tree_skeleton(root),
                committed_cells: 0,
                growth_credit_ms: 0,
            },
        );
        let d = species();
        let mut key = 0;
        while seed_offset(
            mix(pid ^ key ^ 1000),
            d.seed_radius_cells,
            d.min_spacing_cells,
        )
        .0 < 2
        {
            key += 1;
            s.rng_progress = key;
        }
        let mut defs = BTreeMap::new();
        defs.insert(d.id, d);
        let mut i = EcologyInputs::default();
        i.moisture.insert(GlobalCell::new(root.x, 0, root.z), 100);
        let cfg = EcologyConfig {
            update_interval_ms: 1000,
            ..Default::default()
        };
        update(&mut s, &v, &defs, &i, cfg, 1000).unwrap();
        assert_eq!(s.seeds.len(), 1);
        let seed = s.seeds.values().next().unwrap();
        assert!(
            seed.cell.x >= 512,
            "seed crossed patch boundary: {:?}",
            seed.cell
        );
        let region = SeedRegion {
            x: seed.cell.x.div_euclid(32 * 16),
            z: seed.cell.z.div_euclid(32 * 16),
            species: seed.species,
        };
        assert_eq!(s.regional_seed_availability[&region], 1);
        let mut expired = seed.clone();
        expired.expires_at_ms = 2000;
        s.seeds.insert(expired.id, expired);
        s.plants.get_mut(&pid).unwrap().last_seed_ms = 2000;
        update(&mut s, &v, &defs, &i, cfg, 1000).unwrap();
        assert!(s.seeds.is_empty());
        assert!(!s.regional_seed_availability.contains_key(&region));
    }
}
