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
use spall_sim::{AuthoritativeCreatureIntent, CreatureBirthPartInput, Simulation};
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
    anchors: Vec<[f64; 3]>,
    targets: Vec<u32>,
    spawned: Vec<u32>,
    attempts: u32,
    rng: u64,
    max_per_tick: usize,
    max_attempts: u32,
    radius_m: (f64, f64),
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
        Ok(Self {
            catalog,
            species,
            foot_offsets,
            anchors,
            targets: targets.to_vec(),
            spawned: vec![0; 3],
            attempts: 0,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            max_per_tick: 2,
            max_attempts: 4_000,
            radius_m: (8.0, 78.0),
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
        let foot = f64::from(-self.foot_offsets[species]);
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
        .map_err(|e| eprintln!("DBG reject {e:?} at {center:?}"))
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
