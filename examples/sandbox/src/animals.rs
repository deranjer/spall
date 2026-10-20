//! Sandbox-owned live animals: the species catalog and the server-side spawn
//! director. The engine owns lifecycle ordering, validation and replication;
//! this module only decides which authored species exist and where they appear.

use std::collections::BTreeMap;
use std::sync::Arc;

use glam::DVec3;
use spall_asset::{Asset, decode};
use spall_core::{
    BrickCoord, CellSizeCode, CreatureGeneration, CreatureId, CreatureKey, GlobalCell, MaterialId,
    Revision, VolumeId, WorldEpoch,
};
use spall_physics::{PartTransform, Representation};
use spall_protocol::{
    CREATURE_STATE_SCHEMA, CreatureAssetIdentity, CreatureClipMode, CreatureLifecycleState,
    CreaturePartBinding, CreatureStateV1, Hash32, OwnedCreatureAsset, OwnedCreatureCatalog,
    OwnedCreatureSpecies, content_manifest_hash,
};
use spall_server::creature_service::{CreatureDirector, CreatureIdPlan};
use spall_sim::{
    AuthoritativeCreatureIntent, CreatureBirthPartInput, CreatureMoveOutcome, CreatureMoveRequest,
    Simulation,
};
use spall_voxel::{Brick, EditPlan, Sample};
use thiserror::Error;

use crate::animal_habitat::{
    AnimalHabitat, CandidateEnvelope, ValidationLimits, validate_candidate,
};

const SPECIES_BYTES: [(&str, AnimalHabitat, &[u8]); 3] = [
    (
        "deer",
        AnimalHabitat::Land,
        include_bytes!("../assets/animals/deer.spvox"),
    ),
    (
        "crow",
        AnimalHabitat::Flying,
        include_bytes!("../assets/animals/crow.spvox"),
    ),
    (
        "trout",
        AnimalHabitat::Aquatic,
        include_bytes!("../assets/animals/trout.spvox"),
    ),
];
const CELL_M: f32 = 0.0625;
/// Gap kept between an animal's lowest cell and the terrain it stands on.
pub const GROUND_CLEARANCE_M: f64 = 0.05;
/// Locomotion selectors carried in motion frames.
pub const LOCOMOTION_IDLE: u8 = 0;
pub const LOCOMOTION_MOVE: u8 = 1;
/// A creature is moved this often (7.5 Hz at 60 Hz), staggered by id.
const MOVE_PERIOD_TICKS: u64 = 8;
const MOVE_DT_S: f64 = MOVE_PERIOD_TICKS as f64 / 60.0;

/// The clip a species plays while moving.
pub fn locomotion_clip(species: &str, tag: u8) -> Option<&'static str> {
    if tag == LOCOMOTION_IDLE {
        return Some("idle");
    }
    match species {
        "deer" => Some("walk"),
        "crow" => Some("flap"),
        "trout" => Some("swim"),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum AnimalError {
    #[error("animal asset {0}: {1}")]
    Asset(&'static str, String),
}

/// One decoded authored species.
#[derive(Debug, Clone)]
pub struct AnimalSpecies {
    pub key: &'static str,
    pub habitat: AnimalHabitat,
    pub asset: Asset,
}

impl AnimalSpecies {
    pub fn identity(&self) -> CreatureAssetIdentity {
        CreatureAssetIdentity {
            uuid: self.asset.uuid,
            logical_hash: Hash32(self.asset.content_hash),
        }
    }
}

/// Decodes every authored species.
pub fn species() -> Result<Vec<AnimalSpecies>, AnimalError> {
    SPECIES_BYTES
        .iter()
        .map(|(key, habitat, bytes)| {
            let asset = decode(bytes).map_err(|e| AnimalError::Asset(key, e.to_string()))?;
            Ok(AnimalSpecies {
                key,
                habitat: *habitat,
                asset,
            })
        })
        .collect()
}

/// The validation catalog both server and client lend to the engine.
pub fn catalog(species: &[AnimalSpecies]) -> Arc<OwnedCreatureCatalog> {
    let materials = crate::game::manifest();
    let mapping = crate::animal_visual::material_mapping(&materials);
    Arc::new(OwnedCreatureCatalog {
        assets: species
            .iter()
            .map(|s| {
                let mut parts: Vec<u32> = s.asset.parts.iter().map(|p| p.id).collect();
                parts.sort_unstable();
                OwnedCreatureAsset {
                    uuid: s.asset.uuid,
                    logical_hash: Hash32(s.asset.content_hash),
                    part_ids: parts,
                    clip_names: s.asset.clips.iter().map(|c| c.name.clone()).collect(),
                    portable_material_keys: s.asset.material_keys.clone(),
                }
            })
            .collect(),
        species: species
            .iter()
            .map(|s| OwnedCreatureSpecies {
                key: s.key.to_owned(),
                version: 1,
                enabled: true,
                asset: s.identity(),
            })
            .collect(),
        materials,
        material_mapping_version: 1,
        material_mapping: mapping.into_iter().collect(),
    })
}

/// Complete sampled local pose of every part, in part-id order, as the physics
/// articulated-pose API requires.
pub fn pose_parts(
    asset: &Asset,
    clip: &str,
    tick: f64,
) -> Result<Vec<(u32, PartTransform)>, String> {
    let sampled = spall_asset::sample_clip(asset, clip, tick).map_err(|e| e.to_string())?;
    Ok(sampled
        .into_iter()
        .map(|(id, pose)| {
            (
                id,
                PartTransform {
                    translation_m: (pose.transform.translation * CELL_M).to_array(),
                    rotation_xyzw: pose.transform.rotation.normalize().to_array(),
                },
            )
        })
        .collect())
}

/// Part-local occupied cells (6.25 cm) with their mapped manifest material.
fn part_cells(
    asset: &Asset,
    mapping: &BTreeMap<String, MaterialId>,
) -> BTreeMap<u32, Vec<([i32; 3], MaterialId)>> {
    let mut cells: BTreeMap<u32, Vec<([i32; 3], MaterialId)>> =
        asset.parts.iter().map(|p| (p.id, Vec::new())).collect();
    for run in &asset.runs {
        let material = mapping[&asset.material_keys[usize::from(run.material_slot - 1)]];
        if let Some(list) = cells.get_mut(&run.part_id) {
            for dx in 0..run.length_x {
                list.push((
                    [run.start[0] + dx as i32, run.start[1], run.start[2]],
                    material,
                ));
            }
        }
    }
    cells
}

/// Lowest point of the animal in its idle pose, relative to the root, in metres.
fn idle_foot_offset_m(asset: &Asset) -> f32 {
    let visual = crate::animal_visual::AnimalVisual::new(
        asset.clone(),
        &crate::animal_visual::material_mapping(&crate::game::manifest()),
        crate::animal_visual::MAX_ANIMAL_VISUAL_CELLS,
    );
    visual
        .ok()
        .and_then(|v| v.sample("idle", 0.0).ok())
        .and_then(|cells| {
            cells
                .iter()
                .map(|c| c.center.y - c.size.y * 0.5)
                .reduce(f32::min)
        })
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Copy)]
enum Gait {
    /// Stand still until this tick.
    Rest { until: u64 },
    /// Walk toward `target` (x, z) until reached, blocked, or `until`.
    Walk { target: [f64; 2], until: u64 },
    /// Fly a circle around `home` at angle `theta`, rate `omega` rad/s.
    Orbit { theta: f64, omega: f64, radius: f64 },
}

#[derive(Debug, Clone, Copy)]
struct Walker {
    species: usize,
    home: [f64; 3],
    gait: Gait,
    clip_phase: u64,
    moving: bool,
    /// Consecutive blocked moves, for back-off.
    blocked_streak: u32,
    /// Heading the animal was last asked to face (about +Y).
    yaw: f64,
}

struct Placement {
    species: usize,
    root_m: DVec3,
    yaw_rad: f64,
}

/// Seeded, bounded director: spawns a fixed starting population around the
/// world's spawn points, a few animals per tick.
pub struct AnimalDirector {
    catalog: Arc<OwnedCreatureCatalog>,
    species: Vec<AnimalSpecies>,
    foot_offsets: Vec<f32>,
    visuals: Vec<crate::animal_visual::AnimalVisual>,
    anchors: Vec<[f64; 3]>,
    targets: Vec<u32>,
    spawned: Vec<u32>,
    attempts: u32,
    rng: u64,
    max_per_tick: usize,
    max_attempts: u32,
    radius_m: (f64, f64),
    walkers: std::collections::BTreeMap<u64, Walker>,
    last_tick: u64,
    /// Per species: (staged moves, blocked moves, walks started), logged periodically.
    stats: Vec<(u64, u64, u64)>,
}

impl AnimalDirector {
    /// `targets` is the desired count per species, in [`species`] order.
    pub fn new(anchors: Vec<[f64; 3]>, targets: [u32; 3], seed: u64) -> Result<Self, AnimalError> {
        let species = species()?;
        let catalog = catalog(&species);
        let foot_offsets = species
            .iter()
            .map(|s| idle_foot_offset_m(&s.asset))
            .collect();
        let materials = crate::animal_visual::material_mapping(&crate::game::manifest());
        let visuals = species
            .iter()
            .map(|s| {
                crate::animal_visual::AnimalVisual::new(
                    s.asset.clone(),
                    &materials,
                    crate::animal_visual::MAX_ANIMAL_VISUAL_CELLS,
                )
                .map_err(|e| AnimalError::Asset(s.key, e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            catalog,
            species,
            foot_offsets,
            visuals,
            anchors,
            targets: targets.to_vec(),
            spawned: vec![0; 3],
            attempts: 0,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            max_per_tick: 2,
            max_attempts: 4_000,
            radius_m: (6.0, 50.0),
            walkers: std::collections::BTreeMap::new(),
            last_tick: 0,
            stats: vec![(0, 0, 0); 3],
        })
    }

    /// Spawn ring around each anchor, in metres.
    pub fn with_radius_m(mut self, min: f64, max: f64) -> Self {
        self.radius_m = (min, max);
        self
    }

    fn next(&mut self) -> u64 {
        // splitmix64: deterministic and dependency-free.
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn done(&self) -> bool {
        self.spawned
            .iter()
            .zip(&self.targets)
            .all(|(have, want)| have >= want)
            || self.attempts >= self.max_attempts
    }

    /// Height of the top solid cell's upper face at the given column, if a
    /// fully known column has one.
    fn ground_y_m(sim: &Simulation, x_cell: i64, z_cell: i64) -> Option<f64> {
        let body = sim.world().terrain();
        let (min, max) = body.collider_region;
        if x_cell < min.x || x_cell > max.x || z_cell < min.z || z_cell > max.z {
            return None;
        }
        let mut y = max.y;
        while y >= min.y {
            match body.volume.sample(GlobalCell::new(x_cell, y, z_cell)) {
                Ok(Sample::Filled(_)) => return Some((y + 1) as f64 * 0.25),
                Ok(Sample::Empty { .. }) => y -= 1,
                _ => return None,
            }
        }
        None
    }

    /// Root height at which the pose's lowest cells clear the ground under each
    /// of them by [`GROUND_CLEARANCE_M`], with the pose turned by `yaw`. The
    /// ground is sampled under every foot cell, not under the root, so a leg
    /// that reaches over a 0.25 m terrain step is accounted for.
    fn support_y(
        sim: &Simulation,
        cells: &[crate::animal_visual::AnimalCell],
        root_xz: [f64; 2],
        yaw: f64,
    ) -> Option<f64> {
        let lowest = cells
            .iter()
            .map(|c| c.center.y - c.size.y * 0.5)
            .reduce(f32::min)?;
        let (sin, cos) = yaw.sin_cos();
        let mut columns = std::collections::BTreeMap::new();
        let mut needed = f64::NEG_INFINITY;
        for cell in cells {
            let bottom = cell.center.y - cell.size.y * 0.5;
            if bottom > lowest + 0.15 {
                continue;
            }
            let (x, z) = (f64::from(cell.center.x), f64::from(cell.center.z));
            let world_x = root_xz[0] + x * cos + z * sin;
            let world_z = root_xz[1] - x * sin + z * cos;
            let key = (
                (world_x / 0.25).floor() as i64,
                (world_z / 0.25).floor() as i64,
            );
            let ground = match columns.get(&key) {
                Some(ground) => *ground,
                None => {
                    let ground = Self::ground_y_m(sim, key.0, key.1)?;
                    columns.insert(key, ground);
                    ground
                }
            };
            needed = needed.max(ground - f64::from(bottom));
        }
        needed.is_finite().then_some(needed + GROUND_CLEARANCE_M)
    }

    fn find_placement(&mut self, sim: &Simulation, species: usize) -> Option<Placement> {
        if self.anchors.is_empty() {
            return None;
        }
        let habitat = self.species[species].habitat;
        if habitat == AnimalHabitat::Aquatic {
            // Water placement needs the live column-water query; not wired yet.
            return None;
        }
        let pick = (self.next() as usize) % self.anchors.len();
        let anchor = self.anchors[pick];
        let angle = self.unit() * std::f64::consts::TAU;
        let radius = self.radius_m.0 + self.unit() * (self.radius_m.1 - self.radius_m.0);
        let x = anchor[0] + radius * angle.cos();
        let z = anchor[2] + radius * angle.sin();
        let (x_cell, z_cell) = ((x / 0.25).floor() as i64, (z / 0.25).floor() as i64);
        let ground = Self::ground_y_m(sim, x_cell, z_cell)?;
        // Hover a few centimetres above the ground: a swept pose that starts in
        // exact contact with terrain reports an impact at time zero.
        let foot = f64::from(-self.foot_offsets[species]) + GROUND_CLEARANCE_M;
        let height_above_ground = match habitat {
            AnimalHabitat::Flying => 3.0 + self.unit() * 6.0,
            _ => 0.0,
        };
        let root = DVec3::new(x, ground + height_above_ground + foot, z);
        // Envelope around the whole idle body; generous and axis aligned.
        let size = DVec3::new(1.2, 1.4, 1.2);
        let center = DVec3::new(x, ground + height_above_ground + size.y * 0.5, z);
        validate_candidate(
            &sim.world().terrain().volume,
            None,
            habitat,
            CandidateEnvelope {
                center_m: center,
                size_m: size,
            },
            ValidationLimits {
                max_support_height_delta_m: 0.3,
                ..ValidationLimits::default()
            },
        )
        .ok()?;
        Some(Placement {
            species,
            root_m: root,
            yaw_rad: self.unit() * std::f64::consts::TAU,
        })
    }

    fn birth(
        &self,
        sim: &Simulation,
        placement: &Placement,
        ids: &mut CreatureIdPlan,
    ) -> Option<AuthoritativeCreatureIntent> {
        let species = &self.species[placement.species];
        let asset = &species.asset;
        let materials = sim.world().materials();
        let mapping = crate::animal_visual::material_mapping(materials);
        let mut cells = part_cells(asset, &mapping);
        let creature_id = ids.creature_id();
        let mut bindings = Vec::new();
        let mut parts = Vec::new();
        let mut asset_parts = asset.parts.clone();
        asset_parts.sort_by_key(|p| p.id);
        for part in &asset_parts {
            let volume_id = VolumeId::new(ids.volume_id()).ok()?;
            let part_cells = cells.remove(&part.id)?;
            let mut volume = spall_voxel::Volume::new(volume_id, CellSizeCode::Sixteenth);
            // Every brick the part's box touches must be resident, or the
            // physics occupancy grid would read absent bricks as unknown.
            let (mut lo, mut hi) = ([i32::MAX; 3], [i32::MIN; 3]);
            for (at, _) in &part_cells {
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(at[axis]);
                    hi[axis] = hi[axis].max(at[axis]);
                }
            }
            let brick = |v: i32| i64::from(v).div_euclid(32);
            for bz in brick(lo[2])..=brick(hi[2]) {
                for by in brick(lo[1])..=brick(hi[1]) {
                    for bx in brick(lo[0])..=brick(hi[0]) {
                        volume
                            .insert_brick(
                                BrickCoord::new(bx, by, bz),
                                Brick::uniform(MaterialId::AIR, Revision(1)),
                            )
                            .ok()?;
                    }
                }
            }
            let mut plan = EditPlan::new(volume_id);
            for (at, material) in part_cells {
                plan.set(
                    GlobalCell::new(at[0].into(), at[1].into(), at[2].into()),
                    material,
                );
            }
            volume.apply_edit(&plan).ok()?;
            bindings.push(CreaturePartBinding {
                part_id: part.id,
                source_volume_id: volume_id,
                source_revision: volume.next_revision().get().saturating_sub(1).max(1),
            });
            parts.push(CreatureBirthPartInput {
                part_id: part.id,
                parent_id: (part.parent_id != 0).then_some(part.parent_id),
                local_transform: PartTransform {
                    translation_m: (part.rest.translation * CELL_M).to_array(),
                    rotation_xyzw: part.rest.rotation.normalize().to_array(),
                },
                volume,
            });
        }
        let (sin, cos) = (
            (placement.yaw_rad * 0.5).sin(),
            (placement.yaw_rad * 0.5).cos(),
        );
        let mapping_hash = self
            .catalog
            .with_view(|view| view.material_mapping_hash)
            .ok()?;
        let state = CreatureStateV1 {
            schema: CREATURE_STATE_SCHEMA,
            key: CreatureKey::new(
                sim.creature_allocator_snapshot()?.world_id,
                WorldEpoch::new(sim.creature_allocator_snapshot()?.world_epoch).ok()?,
                CreatureId::new(creature_id).ok()?,
                CreatureGeneration::INITIAL,
            )
            .ok()?,
            state_revision: 1,
            lifecycle: CreatureLifecycleState::Alive,
            species_key: species.key.to_owned(),
            species_version: 1,
            asset: species.identity(),
            material_manifest_hash: content_manifest_hash(materials),
            material_mapping_version: 1,
            material_mapping_hash: mapping_hash,
            clip_name: "idle".into(),
            clip_mode: CreatureClipMode::Playing,
            clip_phase_tick: 0,
            clip_subtick_remainder: 0,
            clip_epoch: 1,
            source_bindings: bindings,
            aggregate_topology_revision: 1,
            pose_epoch: 1,
            root_translation_m: placement.root_m.to_array(),
            root_rotation_xyzw: [0.0, sin as f32, 0.0, cos as f32],
            linear_velocity_m_s: [0.0; 3],
            angular_velocity_rad_s: [0.0; 3],
            locomotion_tag: 0,
        };
        Some(AuthoritativeCreatureIntent::Birth {
            state: Box::new(state),
            parts,
            representation: Representation::MergedCuboids,
        })
    }
}

impl CreatureDirector for AnimalDirector {
    fn catalog(&self) -> Arc<OwnedCreatureCatalog> {
        self.catalog.clone()
    }

    fn on_world_reset(&mut self) {
        self.spawned = vec![0; self.species.len()];
        self.attempts = 0;
        self.walkers.clear();
    }

    fn on_move_outcome(&mut self, key: spall_core::CreatureKey, outcome: CreatureMoveOutcome) {
        let Some(walker) = self.walkers.get_mut(&key.creature_id.get()) else {
            return;
        };
        match outcome {
            CreatureMoveOutcome::Blocked { .. } => {
                self.stats[walker.species].1 += 1;
                // The pose did not change. Birds turn around; walkers stop and re-plan.
                walker.gait = match walker.gait {
                    Gait::Orbit {
                        theta,
                        omega,
                        radius,
                    } => {
                        // Something solid (a tree) is in the circle: fly higher.
                        walker.home[1] += 1.5;
                        Gait::Orbit {
                            theta,
                            omega,
                            radius,
                        }
                    }
                    _ => {
                        walker.blocked_streak = walker.blocked_streak.saturating_add(1);
                        Gait::Rest {
                            // Back off so a creature wedged against terrain does not
                            // pay for a failed sweep every few ticks.
                            until: self.last_tick + 120 * u64::from(walker.blocked_streak.min(8)),
                        }
                    }
                };
            }
            CreatureMoveOutcome::Staged => {
                self.stats[walker.species].0 += 1;
                walker.blocked_streak = 0;
            }
            CreatureMoveOutcome::Busy => {}
        }
    }

    fn motion(&mut self, tick: u64, sim: &Simulation) -> Vec<CreatureMoveRequest> {
        self.last_tick = tick;
        if tick.is_multiple_of(1200) {
            tracing::info!(
                deer = ?self.stats[0],
                crow = ?self.stats[1],
                "animal moves (staged, blocked, walks started) per species"
            );
        }
        let mut requests = Vec::new();
        for alive in sim.alive_creatures() {
            let id = alive.key.creature_id.get();
            if !(tick + id).is_multiple_of(MOVE_PERIOD_TICKS) {
                continue;
            }
            let Some(mut walker) = self.walkers.get(&id).copied() else {
                continue;
            };
            let species_key = self.species[walker.species].key;
            // The pose about to be requested; foot heights come from the ground
            // under each of its lowest cells (see `support_y`).
            let next_phase = walker.clip_phase + MOVE_PERIOD_TICKS;
            let move_clip = locomotion_clip(species_key, LOCOMOTION_MOVE).unwrap_or("idle");
            let here = alive.translation_m;
            // Decide the next root target and whether the animal is moving.
            let (target, moving, yaw) = match walker.gait {
                Gait::Rest { until } => {
                    if tick >= until {
                        let angle = self.unit() * std::f64::consts::TAU;
                        let distance = 4.0 + self.unit() * 14.0;
                        walker.gait = Gait::Walk {
                            target: [
                                here[0] + distance * angle.cos(),
                                here[2] + distance * angle.sin(),
                            ],
                            until: tick + 60 * 20,
                        };
                    }
                    (here, false, None)
                }
                Gait::Walk { target, until } => {
                    let (dx, dz) = (target[0] - here[0], target[1] - here[2]);
                    let distance = dx.hypot(dz);
                    let visual = &self.visuals[walker.species];
                    let cells = visual
                        .sample(move_clip, next_phase as f64)
                        .unwrap_or_default();
                    let needed_here = Self::support_y(sim, &cells, [here[0], here[2]], walker.yaw);
                    let wrap = |angle: f64| {
                        (angle + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
                            - std::f64::consts::PI
                    };
                    let want = f64::atan2(-dz, dx);
                    let turn = wrap(want - walker.yaw);
                    let rest = |walker: &mut Walker, until: u64| {
                        walker.gait = Gait::Rest { until };
                    };
                    if distance < 0.4 || tick >= until {
                        rest(&mut walker, tick + 60 + (self.next() % 240));
                        (here, false, None)
                    } else if let Some(needed) = needed_here
                        && here[1] > needed + 0.02
                    {
                        // Over a ledge: drop onto the ground before going on.
                        ([here[0], needed, here[2]], true, Some(walker.yaw))
                    } else if let Some(needed) = needed_here
                        && turn.abs() > 0.35
                    {
                        // Turn toward the target in place, a bounded angle per step.
                        (
                            [here[0], needed, here[2]],
                            true,
                            Some(walker.yaw + turn.clamp(-0.35, 0.35)),
                        )
                    } else {
                        let step = (1.4 * MOVE_DT_S).min(distance);
                        let next = [
                            here[0] + dx / distance * step,
                            here[2] + dz / distance * step,
                        ];
                        let yaw_next = walker.yaw + turn;
                        let needed_next = Self::support_y(sim, &cells, next, yaw_next);
                        match (needed_here, needed_next) {
                            (Some(from), Some(to)) if (to - from).abs() <= 0.5 => {
                                if to > here[1] + 0.02 {
                                    // Step up: rise first so the feet clear the edge.
                                    ([here[0], to, here[2]], true, Some(walker.yaw))
                                } else {
                                    // Level or step down: move on at this height; the drop
                                    // happens on the next update.
                                    ([next[0], from.max(to), next[1]], true, Some(yaw_next))
                                }
                            }
                            _ => {
                                // Too steep or off the known world: stop and rest.
                                rest(&mut walker, tick + 60 + (self.next() % 180));
                                (here, false, None)
                            }
                        }
                    }
                }
                Gait::Orbit {
                    theta,
                    omega,
                    radius,
                } => {
                    let theta = theta + omega * MOVE_DT_S;
                    walker.gait = Gait::Orbit {
                        theta,
                        omega,
                        radius,
                    };
                    (
                        [
                            walker.home[0] + radius * theta.cos(),
                            walker.home[1],
                            walker.home[2] + radius * theta.sin(),
                        ],
                        true,
                        // Tangent of the circle, nose along the direction of travel.
                        Some(f64::atan2(
                            -theta.cos() * omega.signum(),
                            -theta.sin() * omega.signum(),
                        )),
                    )
                }
            };
            if moving {
                walker.clip_phase += MOVE_PERIOD_TICKS;
            }
            let was_moving = walker.moving;
            walker.moving = moving;
            self.walkers.insert(id, walker);
            if !moving && !was_moving {
                continue;
            }
            // A creature that just stopped sends one stationary pose so clients
            // switch from the move clip back to idle.
            let (clip, tag, phase) = if moving {
                let clip = locomotion_clip(species_key, LOCOMOTION_MOVE).unwrap_or("idle");
                (clip, LOCOMOTION_MOVE, walker.clip_phase)
            } else {
                ("idle", LOCOMOTION_IDLE, 0)
            };
            let Ok(parts) = pose_parts(&self.species[walker.species].asset, clip, phase as f64)
            else {
                continue;
            };
            // Standing still keeps the current heading (rotation about Y only).
            let yaw = yaw.unwrap_or_else(|| {
                let [_, y, _, w] = alive.rotation_xyzw;
                f64::from(2.0 * y.atan2(w))
            });
            walker.yaw = yaw;
            self.walkers.insert(id, walker);
            let (sin, cos) = ((yaw * 0.5).sin(), (yaw * 0.5).cos());
            requests.push(CreatureMoveRequest {
                key: alive.key,
                target_translation_m: target,
                target_rotation_xyzw: [0.0, sin as f32, 0.0, cos as f32],
                parts,
                locomotion_tag: tag,
                clip_phase_tick: phase,
            });
        }
        requests
    }

    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<AuthoritativeCreatureIntent> {
        // Let the world settle for a moment before the first births.
        if tick < 10 || self.done() {
            return Vec::new();
        }
        let mut intents = Vec::new();
        while intents.len() < self.max_per_tick && !self.done() {
            self.attempts += 1;
            let Some(species) =
                (0..self.species.len()).find(|&i| self.spawned[i] < self.targets[i])
            else {
                break;
            };
            let Some(placement) = self.find_placement(sim, species) else {
                continue;
            };
            let mut trial = *ids;
            if let Some(intent) = self.birth(sim, &placement, &mut trial) {
                if let AuthoritativeCreatureIntent::Birth { state, .. } = &intent {
                    let gait = match self.species[species].habitat {
                        AnimalHabitat::Flying => Gait::Orbit {
                            theta: self.unit() * std::f64::consts::TAU,
                            omega: 0.35 + self.unit() * 0.3,
                            radius: 6.0 + self.unit() * 8.0,
                        },
                        _ => Gait::Rest {
                            until: tick + 60 + (self.next() % 240),
                        },
                    };
                    let home = placement.root_m.to_array();
                    // A circling bird centres its orbit so it starts where it was placed,
                    // and flies above the highest ground under the whole circle.
                    let home = match gait {
                        Gait::Orbit { theta, radius, .. } => {
                            let centre = [
                                home[0] - radius * theta.cos(),
                                home[1],
                                home[2] - radius * theta.sin(),
                            ];
                            let mut ceiling = home[1];
                            for step in 0..16 {
                                let angle = f64::from(step) * std::f64::consts::TAU / 16.0;
                                let x = ((centre[0] + radius * angle.cos()) / 0.25).floor() as i64;
                                let z = ((centre[2] + radius * angle.sin()) / 0.25).floor() as i64;
                                if let Some(ground) = Self::ground_y_m(sim, x, z) {
                                    ceiling = ceiling.max(ground + 4.0);
                                }
                            }
                            [centre[0], ceiling, centre[2]]
                        }
                        _ => home,
                    };
                    self.walkers.insert(
                        state.key.creature_id.get(),
                        Walker {
                            species,
                            home,
                            gait,
                            clip_phase: 0,
                            moving: false,
                            blocked_streak: 0,
                            yaw: placement.yaw_rad,
                        },
                    );
                }
                *ids = trial;
                self.spawned[species] += 1;
                tracing::info!(
                    species = self.species[species].key,
                    x = placement.root_m.x,
                    y = placement.root_m.y,
                    z = placement.root_m.z,
                    "animal placed"
                );
                intents.push(intent);
            }
        }
        intents
    }
}
