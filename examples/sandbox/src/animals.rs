//! Sandbox-owned live animals: the species list and the server-side spawn
//! director. The engine keeps each animal's authoritative pose and replicates
//! it; this module only decides which authored species exist, where they
//! appear and where they walk or fly.

use glam::DVec3;
use spall_asset::{Asset, decode};
use spall_core::GlobalCell;
use spall_protocol::CreatureRecord;
use spall_server::creature_service::{
    CreatureAction, CreatureActionOutcome, CreatureDirector, CreatureIdPlan,
};
use spall_sim::{CreatureMoveOutcome, CreatureMoveRequest, CreatureWalkRequest, Simulation};
use spall_voxel::Sample;
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
/// Gap kept between an animal's lowest cell and the terrain it stands on.
pub const GROUND_CLEARANCE_M: f64 = 0.05;
/// Locomotion selectors carried in motion frames.
pub const LOCOMOTION_IDLE: u8 = 0;
pub const LOCOMOTION_MOVE: u8 = 1;
/// Footprint capsule for walking animals (about a deer: 1.06 m tall, 0.56 m wide).
const WALK_CAPSULE_RADIUS_M: f32 = 0.28;
const WALK_CAPSULE_HALF_HEIGHT_M: f32 = 0.25;
/// Capsule swept around a flying animal (about a crow: small body, wings folded in).
const FLY_CAPSULE_RADIUS_M: f32 = 0.25;
const FLY_CAPSULE_HALF_HEIGHT_M: f32 = 0.1;
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
    /// Heading the animal was last asked to face (about +Y).
    yaw: f64,
    /// Where the animal last stood, for choosing a detour when stuck.
    at: [f64; 3],
    /// Horizontal step asked for last update, metres.
    last_desired: f64,
    /// Consecutive updates that achieved far less than asked.
    stuck: u32,
    /// Detours tried since the animal last made progress.
    detours: u32,
}

struct Placement {
    species: usize,
    root_m: DVec3,
    yaw_rad: f64,
}

/// Seeded, bounded director: spawns a fixed starting population around the
/// world's spawn points, a few animals per tick.
pub struct AnimalDirector {
    species: Vec<AnimalSpecies>,
    /// Root-to-lowest-foot offset over every pose of the idle and move clips.
    walk_feet: Vec<f32>,
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
        let walk_feet = species
            .iter()
            .zip(&visuals)
            .map(|(species, visual)| {
                let mut lowest = f32::INFINITY;
                for clip in [
                    "idle",
                    locomotion_clip(species.key, LOCOMOTION_MOVE).unwrap_or("idle"),
                ] {
                    let duration = visual.clip_duration(clip).unwrap_or(1).max(1);
                    for phase in (0..duration).step_by(2) {
                        if let Ok(cells) = visual.sample(clip, f64::from(phase)) {
                            for cell in cells {
                                lowest = lowest.min(cell.center.y - cell.size.y * 0.5);
                            }
                        }
                    }
                }
                if lowest.is_finite() { lowest } else { -0.5 }
            })
            .collect();
        Ok(Self {
            species,
            walk_feet,
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
        let foot = f64::from(-self.walk_feet[species]) + GROUND_CLEARANCE_M;
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

    fn birth(&self, placement: &Placement, ids: &mut CreatureIdPlan) -> CreatureRecord {
        let (sin, cos) = (
            (placement.yaw_rad * 0.5).sin(),
            (placement.yaw_rad * 0.5).cos(),
        );
        CreatureRecord {
            creature_id: ids.creature_id(),
            species: self.species[placement.species].key.to_owned(),
            translation_m: placement.root_m.to_array(),
            rotation_xyzw: [0.0, sin as f32, 0.0, cos as f32],
            locomotion_tag: LOCOMOTION_IDLE,
            clip_phase_tick: 0,
        }
    }
}

impl CreatureDirector for AnimalDirector {
    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<CreatureRecord> {
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
            {
                let record = self.birth(&placement, ids);
                {
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
                        record.creature_id,
                        Walker {
                            species,
                            home,
                            gait,
                            clip_phase: 0,
                            moving: false,
                            last_desired: 0.0,
                            stuck: 0,
                            detours: 0,
                            yaw: placement.yaw_rad,
                            at: placement.root_m.to_array(),
                        },
                    );
                }
                self.spawned[species] += 1;
                tracing::info!(
                    species = self.species[species].key,
                    x = placement.root_m.x,
                    y = placement.root_m.y,
                    z = placement.root_m.z,
                    "animal placed"
                );
                intents.push(record);
            }
        }
        intents
    }

    fn on_world_reset(&mut self) {
        self.spawned = vec![0; self.species.len()];
        self.attempts = 0;
        self.walkers.clear();
    }

    fn on_action_outcome(&mut self, creature_id: u64, outcome: CreatureActionOutcome) {
        let Some(walker) = self.walkers.get_mut(&creature_id) else {
            return;
        };
        match outcome {
            CreatureActionOutcome::Fly(CreatureMoveOutcome::Blocked) => {
                self.stats[walker.species].1 += 1;
                // A bird whose circle hits something solid (a tree) flies higher.
                if matches!(walker.gait, Gait::Orbit { .. }) {
                    walker.home[1] += 1.5;
                }
            }
            CreatureActionOutcome::Fly(CreatureMoveOutcome::Moved) => {
                self.stats[walker.species].0 += 1;
            }
            CreatureActionOutcome::Walked(walked) => {
                self.stats[walker.species].0 += 1;
                let moved = walked.moved_m[0].hypot(walked.moved_m[2]);
                if walker.last_desired > 0.02 && moved < walker.last_desired * 0.35 {
                    walker.stuck += 1;
                    self.stats[walker.species].1 += 1;
                } else if walker.last_desired > 0.02 {
                    walker.stuck = 0;
                    walker.detours = 0;
                }
            }
        }
    }

    fn motion(&mut self, tick: u64, sim: &Simulation) -> Vec<CreatureAction> {
        self.last_tick = tick;
        if tick.is_multiple_of(1200) {
            tracing::info!(
                deer = ?self.stats[0],
                crow = ?self.stats[1],
                "animal moves (applied, stuck or blocked) per species"
            );
        }
        let wrap = |angle: f64| {
            (angle + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
        };
        let mut actions = Vec::new();
        for alive in sim.creature_poses() {
            let id = alive.creature_id;
            if !(tick + id).is_multiple_of(MOVE_PERIOD_TICKS) {
                continue;
            }
            let Some(mut walker) = self.walkers.get(&id).copied() else {
                continue;
            };
            let here = alive.translation_m;
            walker.at = here;
            let flying = matches!(walker.gait, Gait::Orbit { .. });
            // `desired` is the horizontal step for a walker; `target` the root target
            // for a flyer. Neither is set while resting.
            let mut desired = [0.0_f64; 2];
            let mut fly_target = here;
            let mut yaw = walker.yaw;
            let mut moving = false;
            match walker.gait {
                Gait::Rest { until } => {
                    if tick >= until {
                        let angle = self.unit() * std::f64::consts::TAU;
                        let distance = 4.0 + self.unit() * 14.0;
                        walker.gait = Gait::Walk {
                            target: [
                                here[0] + distance * angle.cos(),
                                here[2] - distance * angle.sin(),
                            ],
                            until: tick + 60 * 20,
                        };
                    }
                }
                Gait::Walk { target, until } => {
                    let (dx, dz) = (target[0] - here[0], target[1] - here[2]);
                    let distance = dx.hypot(dz);
                    if distance < 0.4 || tick >= until {
                        walker.gait = Gait::Rest {
                            until: tick + 60 + (self.next() % 240),
                        };
                    } else if walker.stuck >= 3 {
                        // Not getting anywhere: try another heading, wider each time,
                        // and stand for a while if every one fails.
                        walker.stuck = 0;
                        walker.detours += 1;
                        if walker.detours > 6 {
                            walker.detours = 0;
                            walker.gait = Gait::Rest { until: tick + 180 };
                        } else {
                            const TURNS: [f64; 6] = [0.8, -0.8, 1.6, -1.6, 2.4, -2.4];
                            let angle = walker.yaw + TURNS[(walker.detours as usize - 1) % 6];
                            walker.gait = Gait::Walk {
                                target: [here[0] + 5.0 * angle.cos(), here[2] - 5.0 * angle.sin()],
                                until,
                            };
                        }
                    } else {
                        let want = f64::atan2(-dz, dx);
                        let turn = wrap(want - walker.yaw);
                        moving = true;
                        if turn.abs() > 0.35 {
                            // Turn toward the target in place, a bounded angle per step.
                            yaw = walker.yaw + turn.clamp(-0.35, 0.35);
                        } else {
                            yaw = walker.yaw + turn;
                            let step = (1.4 * MOVE_DT_S).min(distance);
                            desired = [dx / distance * step, dz / distance * step];
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
                    fly_target = [
                        walker.home[0] + radius * theta.cos(),
                        walker.home[1],
                        walker.home[2] + radius * theta.sin(),
                    ];
                    // Tangent of the circle, nose along the direction of travel.
                    yaw = f64::atan2(-theta.cos() * omega.signum(), -theta.sin() * omega.signum());
                    moving = true;
                }
            }
            if moving {
                walker.clip_phase += MOVE_PERIOD_TICKS;
            }
            let was_moving = walker.moving;
            walker.moving = moving;
            walker.last_desired = desired[0].hypot(desired[1]);
            walker.yaw = yaw;
            self.walkers.insert(id, walker);
            if !moving && !was_moving {
                continue;
            }
            // A creature that just stopped sends one stationary pose so clients
            // switch from the move clip back to idle.
            let (tag, phase) = if moving {
                (LOCOMOTION_MOVE, walker.clip_phase)
            } else {
                (LOCOMOTION_IDLE, 0)
            };
            let (sin, cos) = ((yaw * 0.5).sin(), (yaw * 0.5).cos());
            let rotation = [0.0, sin as f32, 0.0, cos as f32];
            if flying {
                actions.push(CreatureAction::Fly(CreatureMoveRequest {
                    creature_id: id,
                    target_translation_m: fly_target,
                    target_rotation_xyzw: rotation,
                    locomotion_tag: tag,
                    clip_phase_tick: phase,
                    feet_offset_m: self.walk_feet[walker.species],
                    capsule_radius_m: FLY_CAPSULE_RADIUS_M,
                    capsule_half_height_m: FLY_CAPSULE_HALF_HEIGHT_M,
                }));
            } else {
                actions.push(CreatureAction::Walk(CreatureWalkRequest {
                    creature_id: id,
                    desired_step_m: [desired[0] as f32, desired[1] as f32],
                    target_rotation_xyzw: rotation,
                    locomotion_tag: tag,
                    clip_phase_tick: phase,
                    feet_offset_m: self.walk_feet[walker.species],
                    capsule_radius_m: WALK_CAPSULE_RADIUS_M,
                    capsule_half_height_m: WALK_CAPSULE_HALF_HEIGHT_M,
                }));
            }
        }
        actions
    }
}
