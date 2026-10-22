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

/// Per-species authoring and locomotion choices; everything else (envelope,
/// foot height) is measured from the asset.
struct SpeciesDef {
    key: &'static str,
    habitat: AnimalHabitat,
    bytes: &'static [u8],
    /// Clip played while moving.
    move_clip: &'static str,
    /// Walking pace, metres per second (flyers set their own speed).
    walk_speed_m_s: f64,
    /// Foot capsule for walking species: radius, half cylinder height.
    capsule_m: (f32, f32),
    /// Pace when fleeing players, metres per second. Species with one notice
    /// players, stand `alert`, then flee at a `run`; others ignore players.
    flee_speed_m_s: Option<f64>,
    /// Crops the grass under its mouth (`graze` clip) now and then while resting.
    grazes: bool,
}

const SPECIES: [SpeciesDef; 5] = [
    SpeciesDef {
        key: "deer",
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../assets/animals/deer.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.4,
        capsule_m: (0.35, 0.35),
        // Faster than a sprinting player (5.8 m/s), so a chase is lost.
        flee_speed_m_s: Some(7.5),
        grazes: true,
    },
    SpeciesDef {
        key: "crow",
        habitat: AnimalHabitat::Flying,
        bytes: include_bytes!("../assets/animals/crow.spvox"),
        move_clip: "flap",
        walk_speed_m_s: 0.0,
        capsule_m: (0.0, 0.0),
        flee_speed_m_s: None,
        grazes: false,
    },
    SpeciesDef {
        key: "trout",
        habitat: AnimalHabitat::Aquatic,
        bytes: include_bytes!("../assets/animals/trout.spvox"),
        move_clip: "swim",
        walk_speed_m_s: 0.0,
        capsule_m: (0.0, 0.0),
        flee_speed_m_s: None,
        grazes: false,
    },
    SpeciesDef {
        key: "bear",
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../assets/animals/bear.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.2,
        capsule_m: (0.45, 0.1),
        flee_speed_m_s: None,
        grazes: false,
    },
    SpeciesDef {
        key: "snow_leopard",
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../assets/animals/leopard.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.1,
        capsule_m: (0.28, 0.1),
        flee_speed_m_s: None,
        grazes: false,
    },
];
/// Gap kept between an animal's lowest cell and the terrain it stands on.
pub const GROUND_CLEARANCE_M: f64 = 0.05;
/// Locomotion selectors carried in motion frames.
pub const LOCOMOTION_IDLE: u8 = 0;
pub const LOCOMOTION_MOVE: u8 = 1;
/// Standing still, watching a player (`alert` clip).
pub const LOCOMOTION_ALERT: u8 = 2;
/// Fleeing a player (`run` clip).
pub const LOCOMOTION_RUN: u8 = 3;
/// Cropping grass where it stands (`graze` clip).
pub const LOCOMOTION_GRAZE: u8 = 4;
/// The worldgen grass material; its appearance variants count as grass too.
const GRASS: spall_core::MaterialId = spall_core::MaterialId(10);
/// Chance a grazing species crops the grass when a rest ends on grass.
const GRAZE_CHANCE: f64 = 0.5;
/// Ticks between bites while grazing (1.5 s).
const BITE_TICKS: u64 = 90;
/// A player this close is noticed by a fleeing species, which stops to watch.
const NOTICE_M: f64 = 28.0;
/// A player this close, or a sprinting one inside the notice distance, makes
/// the animal bolt at once.
const BOLT_M: f64 = 14.0;
/// A fleeing animal calms down once every player is this far away.
const CALM_M: f64 = 45.0;
/// Horizontal speed above which a player counts as sprinting (walk is slower).
const SPRINT_M_S: f32 = 4.5;
/// Capsule swept around a flying animal (about a crow: small body, wings folded in).
const FLY_CAPSULE_RADIUS_M: f32 = 0.25;
const FLY_CAPSULE_HALF_HEIGHT_M: f32 = 0.1;
/// A creature is moved this often (7.5 Hz at 60 Hz), staggered by id.
const MOVE_PERIOD_TICKS: u64 = 8;
const MOVE_DT_S: f64 = MOVE_PERIOD_TICKS as f64 / 60.0;

/// The clip a species plays for a locomotion tag.
pub fn locomotion_clip(species: &str, tag: u8) -> Option<&'static str> {
    if tag == LOCOMOTION_IDLE {
        return Some("idle");
    }
    let def = SPECIES.iter().find(|def| def.key == species)?;
    match tag {
        LOCOMOTION_MOVE => Some(def.move_clip),
        LOCOMOTION_ALERT if def.flee_speed_m_s.is_some() => Some("alert"),
        LOCOMOTION_RUN if def.flee_speed_m_s.is_some() => Some("run"),
        LOCOMOTION_GRAZE if def.grazes => Some("graze"),
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
    pub walk_speed_m_s: f64,
    /// Foot capsule (radius, half cylinder height) for walking species.
    pub capsule_m: (f32, f32),
    /// Pace when fleeing players; `None` for species that ignore them.
    pub flee_speed_m_s: Option<f64>,
    pub grazes: bool,
}

/// Decodes every authored species.
pub fn species() -> Result<Vec<AnimalSpecies>, AnimalError> {
    SPECIES
        .iter()
        .map(|def| {
            let asset =
                decode(def.bytes).map_err(|e| AnimalError::Asset(def.key, e.to_string()))?;
            Ok(AnimalSpecies {
                key: def.key,
                habitat: def.habitat,
                asset,
                walk_speed_m_s: def.walk_speed_m_s,
                capsule_m: def.capsule_m,
                flee_speed_m_s: def.flee_speed_m_s,
                grazes: def.grazes,
            })
        })
        .collect()
}

/// Root-relative bounds of every cell over the idle and move clips, metres.
fn pose_bounds(visual: &crate::animal_visual::AnimalVisual, key: &str) -> (glam::Vec3, glam::Vec3) {
    clip_bounds(
        visual,
        &[
            "idle",
            locomotion_clip(key, LOCOMOTION_MOVE).unwrap_or("idle"),
        ],
    )
}

/// Root-relative bounds of every cell over the named clips, metres.
fn clip_bounds(
    visual: &crate::animal_visual::AnimalVisual,
    clips: &[&str],
) -> (glam::Vec3, glam::Vec3) {
    let (mut lo, mut hi) = (
        glam::Vec3::splat(f32::INFINITY),
        glam::Vec3::splat(f32::NEG_INFINITY),
    );
    for &clip in clips {
        let duration = visual.clip_duration(clip).unwrap_or(1).max(1);
        for phase in (0..duration).step_by(2) {
            if let Ok(cells) = visual.sample(clip, f64::from(phase)) {
                for cell in cells {
                    lo = lo.min(cell.center - cell.size * 0.5);
                    hi = hi.max(cell.center + cell.size * 0.5);
                }
            }
        }
    }
    if lo.is_finite() && hi.is_finite() {
        (lo, hi)
    } else {
        (
            glam::Vec3::new(-0.5, -0.5, -0.5),
            glam::Vec3::new(0.5, 0.5, 0.5),
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum Gait {
    /// Stand still until this tick.
    Rest { until: u64 },
    /// Walk toward `target` (x, z) until reached, blocked, or `until`.
    Walk { target: [f64; 2], until: u64 },
    /// Fly a circle around `home` at angle `theta`, rate `omega` rad/s.
    Orbit { theta: f64, omega: f64, radius: f64 },
    /// Stand facing a player who came near, until `until`.
    Alert { until: u64 },
    /// Run away from players until clear of them or `until`.
    Flee { until: u64 },
    /// Stand cropping the grass under the mouth, a bite every [`BITE_TICKS`],
    /// until `until` or the grass in reach is gone.
    Graze { until: u64, next_bite: u64 },
}

/// The nearest player to an animal, horizontally.
#[derive(Debug, Clone, Copy)]
struct Threat {
    distance_m: f64,
    /// Unit vector from the player toward the animal (x, z).
    away: [f64; 2],
    sprinting: bool,
}

fn nearest_threat(players: &[([f64; 3], bool)], here: [f64; 3]) -> Option<Threat> {
    players
        .iter()
        .map(|&(at, sprinting)| {
            let (dx, dz) = (here[0] - at[0], here[2] - at[2]);
            let distance_m = dx.hypot(dz);
            let away = if distance_m > 1.0e-6 {
                [dx / distance_m, dz / distance_m]
            } else {
                [1.0, 0.0]
            };
            Threat {
                distance_m,
                away,
                sprinting,
            }
        })
        .min_by(|a, b| a.distance_m.total_cmp(&b.distance_m))
}

#[derive(Debug, Clone, Copy)]
struct Walker {
    species: usize,
    home: [f64; 3],
    gait: Gait,
    clip_phase: u64,
    /// Locomotion tag sent last update.
    tag: u8,
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
    /// Placement envelope over the same poses, metres.
    envelope_m: Vec<DVec3>,
    /// How far ahead of the root the mouth reaches the ground while grazing.
    mouth_m: Vec<f64>,
    /// Terrain materials that count as grass.
    grass: Vec<spall_core::MaterialId>,
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
    /// `targets` is the desired count per species, in [`species`] order;
    /// species past the end of the slice get none.
    pub fn new(anchors: Vec<[f64; 3]>, targets: &[u32], seed: u64) -> Result<Self, AnimalError> {
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
        let (walk_feet, envelope_m) = species
            .iter()
            .zip(&visuals)
            .map(|(species, visual)| {
                let (lo, hi) = pose_bounds(visual, species.key);
                // Yaw-invariant envelope with a little slack around the body,
                // snapped up to whole terrain cells: an off-grid height leaves the
                // envelope's floor a rounding error inside the ground cell.
                let snap = |m: f32| ((f64::from(m) + 0.2) / 0.25).ceil() * 0.25;
                let reach = snap((hi.x - lo.x).max(hi.z - lo.z));
                let envelope = DVec3::new(reach, snap(hi.y - lo.y), reach);
                (lo.y, envelope)
            })
            .unzip();
        // The head points along +X; its forward reach in the graze pose is
        // where the muzzle meets the grass.
        let mouth_m = visuals
            .iter()
            .map(|visual| f64::from(clip_bounds(visual, &["graze"]).1.x.max(0.0)))
            .collect();
        let grass = std::iter::once(GRASS)
            .chain(
                crate::appearance::variants()
                    .into_iter()
                    .filter(|variant| variant.base == GRASS)
                    .map(|variant| variant.id),
            )
            .collect();
        let count = species.len();
        Ok(Self {
            species,
            walk_feet,
            envelope_m,
            mouth_m,
            grass,
            anchors,
            targets: (0..count)
                .map(|i| targets.get(i).copied().unwrap_or(0))
                .collect(),
            spawned: vec![0; count],
            attempts: 0,
            rng: seed ^ 0x9E37_79B9_7F4A_7C15,
            max_per_tick: 2,
            max_attempts: 4_000,
            radius_m: (6.0, 50.0),
            walkers: std::collections::BTreeMap::new(),
            last_tick: 0,
            stats: vec![(0, 0, 0); count],
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
        Self::ground(sim, x_cell, z_cell).map(|(y, _)| y)
    }

    /// [`Self::ground_y_m`] and the material of that top cell.
    fn ground(sim: &Simulation, x_cell: i64, z_cell: i64) -> Option<(f64, spall_core::MaterialId)> {
        let body = sim.world().terrain();
        let (min, max) = body.collider_region;
        if x_cell < min.x || x_cell > max.x || z_cell < min.z || z_cell > max.z {
            return None;
        }
        let mut y = max.y;
        while y >= min.y {
            match body.volume.sample(GlobalCell::new(x_cell, y, z_cell)) {
                Ok(Sample::Filled(material)) => return Some(((y + 1) as f64 * 0.25, material)),
                Ok(Sample::Empty { .. }) => y -= 1,
                _ => return None,
            }
        }
        None
    }

    /// The terrain column under the mouth of a grazing animal at `here` facing
    /// `yaw`, shifted `side` cells to its left (+) or right (-).
    fn mouth_column(&self, species: usize, here: [f64; 3], yaw: f64, side: i64) -> [i64; 2] {
        let reach = self.mouth_m[species] * 0.9;
        let side_m = side as f64 * 0.25;
        let x = here[0] + reach * yaw.cos() - side_m * yaw.sin();
        let z = here[2] - reach * yaw.sin() - side_m * yaw.cos();
        [(x / 0.25).floor() as i64, (z / 0.25).floor() as i64]
    }

    /// Whether the column carries grass that is not cropped or regrowing. A
    /// world without vegetation has none to eat.
    fn uncropped_grass(&self, sim: &Simulation, column: [i64; 2]) -> bool {
        let Some(vegetation) = sim.vegetation_state() else {
            return false;
        };
        let (Ok(x), Ok(z)) = (u16::try_from(column[0]), u16::try_from(column[1])) else {
            return false;
        };
        let eaten = vegetation
            .cleared
            .binary_search_by_key(&(x, z), |c| (c.x, c.z))
            .is_ok();
        !eaten
            && Self::ground(sim, column[0], column[1])
                .is_some_and(|(_, material)| self.grass.contains(&material))
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
        let size = self.envelope_m[species];
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

    /// Moves a species that flees players between its calm gaits, `Alert` and
    /// `Flee`, from where the nearest player is.
    fn react_to_players(&mut self, walker: &mut Walker, threat: Option<Threat>, tick: u64) {
        let bolts = |t: &Threat| t.distance_m < BOLT_M || (t.sprinting && t.distance_m < NOTICE_M);
        let flee = Gait::Flee {
            until: tick + 60 * 20,
        };
        match walker.gait {
            Gait::Flee { until } => {
                if tick >= until || threat.is_none_or(|t| t.distance_m > CALM_M) {
                    walker.detours = 0;
                    walker.gait = Gait::Rest {
                        until: tick + 120 + (self.next() % 240),
                    };
                }
            }
            Gait::Alert { until } => match threat {
                Some(t) if bolts(&t) => walker.gait = flee,
                // Watched long enough: leave if the player is still around.
                Some(t) if tick >= until && t.distance_m < NOTICE_M => walker.gait = flee,
                _ if tick >= until => {
                    walker.gait = Gait::Rest {
                        until: tick + 60 + (self.next() % 120),
                    };
                }
                _ => {}
            },
            // A player interrupts grazing like any other calm moment.
            Gait::Rest { .. } | Gait::Walk { .. } | Gait::Graze { .. } => match threat {
                Some(t) if bolts(&t) => {
                    walker.detours = 0;
                    walker.gait = flee;
                }
                Some(t) if t.distance_m < NOTICE_M => {
                    walker.gait = Gait::Alert {
                        until: tick + 90 + (self.next() % 90),
                    };
                }
                _ => {}
            },
            Gait::Orbit { .. } => {}
        }
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
                            tag: LOCOMOTION_IDLE,
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
            CreatureActionOutcome::Grazed(ate) => {
                // The grass in reach is gone: stop and move on.
                if !ate && matches!(walker.gait, Gait::Graze { .. }) {
                    walker.gait = Gait::Rest {
                        until: self.last_tick,
                    };
                }
            }
        }
    }

    fn motion(&mut self, tick: u64, sim: &Simulation) -> Vec<CreatureAction> {
        self.last_tick = tick;
        if tick.is_multiple_of(1200) {
            for (species, stats) in self.species.iter().zip(&self.stats) {
                if stats.0 + stats.1 > 0 {
                    tracing::info!(
                        species = species.key,
                        moves = ?stats,
                        "animal moves (applied, stuck or blocked)"
                    );
                }
            }
        }
        let wrap = |angle: f64| {
            (angle + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
        };
        let players: Vec<([f64; 3], bool)> = sim
            .world()
            .players()
            .map(|player| {
                let [vx, _, vz] = player.state.velocity_m_s;
                (player.state.position_m, vx.hypot(vz) > SPRINT_M_S)
            })
            .collect();
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
            let mut tag = LOCOMOTION_IDLE;
            let flee_speed = self.species[walker.species].flee_speed_m_s;
            let threat = flee_speed.and_then(|_| nearest_threat(&players, here));
            if flee_speed.is_some() {
                self.react_to_players(&mut walker, threat, tick);
            }
            match walker.gait {
                Gait::Rest { until } => {
                    let grazes = self.species[walker.species].grazes
                        && tick >= until
                        && self
                            .uncropped_grass(sim, self.mouth_column(walker.species, here, yaw, 0))
                        && self.unit() < GRAZE_CHANCE;
                    if grazes {
                        walker.gait = Gait::Graze {
                            until: tick + 300 + self.next() % 420,
                            next_bite: tick,
                        };
                    } else if tick >= until {
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
                        tag = LOCOMOTION_MOVE;
                        if turn.abs() > 0.35 {
                            // Turn toward the target in place, a bounded angle per step.
                            yaw = walker.yaw + turn.clamp(-0.35, 0.35);
                        } else {
                            yaw = walker.yaw + turn;
                            let speed = self.species[walker.species].walk_speed_m_s;
                            let step = (speed * MOVE_DT_S).min(distance);
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
                    tag = LOCOMOTION_MOVE;
                }
                Gait::Alert { .. } => {
                    // Turn to watch the player, a bounded angle per step.
                    if let Some(threat) = threat {
                        let want = f64::atan2(threat.away[1], -threat.away[0]);
                        yaw = walker.yaw + wrap(want - walker.yaw).clamp(-0.6, 0.6);
                    }
                    tag = LOCOMOTION_ALERT;
                }
                Gait::Flee { .. } => {
                    // Straight away from the nearest player; a blocked runner
                    // veers further off each time until it gets going again.
                    let away = threat.map_or([yaw.cos(), -yaw.sin()], |t| t.away);
                    const VEERS: [f64; 6] = [0.9, -0.9, 1.6, -1.6, 2.3, -2.3];
                    if walker.stuck >= 2 {
                        walker.stuck = 0;
                        walker.detours += 1;
                    }
                    let veer = match walker.detours {
                        0 => 0.0,
                        n => VEERS[(n as usize - 1) % VEERS.len()],
                    };
                    let want = f64::atan2(-away[1], away[0]) + veer;
                    // Turns sharply, but keeps running through the turn.
                    yaw = walker.yaw + wrap(want - walker.yaw).clamp(-1.2, 1.2);
                    let step = flee_speed.unwrap_or(0.0) * MOVE_DT_S;
                    desired = [step * yaw.cos(), -step * yaw.sin()];
                    tag = LOCOMOTION_RUN;
                }
                Gait::Graze { .. } => {}
            }
            // Grazing is checked after the match so a rest that just turned
            // into grazing takes its first bite this update.
            if let Gait::Graze { until, next_bite } = walker.gait {
                tag = LOCOMOTION_GRAZE;
                if tick >= until {
                    // Had enough here: walk on after this update.
                    walker.gait = Gait::Rest { until: tick };
                } else if tick >= next_bite {
                    let bite = [0, 1, -1]
                        .into_iter()
                        .map(|side| self.mouth_column(walker.species, here, yaw, side))
                        .find(|&column| self.uncropped_grass(sim, column));
                    match bite {
                        Some(column) => {
                            walker.gait = Gait::Graze {
                                until,
                                next_bite: tick + BITE_TICKS,
                            };
                            actions.push(CreatureAction::Graze {
                                creature_id: id,
                                column,
                            });
                        }
                        // Nothing left in reach.
                        None => walker.gait = Gait::Rest { until: tick },
                    }
                }
            }
            // A new activity starts its clip from the beginning.
            if tag != walker.tag {
                walker.clip_phase = 0;
            } else if tag != LOCOMOTION_IDLE {
                walker.clip_phase += MOVE_PERIOD_TICKS;
            }
            let was = walker.tag;
            walker.tag = tag;
            walker.last_desired = desired[0].hypot(desired[1]);
            walker.yaw = yaw;
            self.walkers.insert(id, walker);
            // A creature that just stopped sends one stationary pose so clients
            // switch back to idle; a resting one sends nothing.
            if tag == LOCOMOTION_IDLE && was == LOCOMOTION_IDLE {
                continue;
            }
            let phase = if tag == LOCOMOTION_IDLE {
                0
            } else {
                walker.clip_phase
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
                    capsule_radius_m: self.species[walker.species].capsule_m.0,
                    capsule_half_height_m: self.species[walker.species].capsule_m.1,
                }));
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tag a species can send names a clip its asset has, or clients
    /// would draw nothing while it does that; and the behaviours a species is
    /// given come with their tags.
    #[test]
    fn every_locomotion_tag_names_an_authored_clip() {
        for species in species().unwrap() {
            for tag in 0..=u8::MAX {
                if let Some(clip) = locomotion_clip(species.key, tag) {
                    assert!(
                        species.asset.clips.iter().any(|c| c.name == clip),
                        "{} has no {clip} clip for tag {tag}",
                        species.key
                    );
                }
            }
            let mut sends = vec![LOCOMOTION_IDLE, LOCOMOTION_MOVE];
            if species.flee_speed_m_s.is_some() {
                sends.extend([LOCOMOTION_ALERT, LOCOMOTION_RUN]);
            }
            if species.grazes {
                sends.push(LOCOMOTION_GRAZE);
            }
            for tag in sends {
                assert!(
                    locomotion_clip(species.key, tag).is_some(),
                    "{} sends tag {tag} with no clip",
                    species.key
                );
            }
        }
    }

    /// A walker's foot capsule must fit inside its own body: a capsule wider or
    /// taller than the animal blocks it where its body would pass, and one with
    /// no size lets it sink into terrain.
    #[test]
    fn walking_capsules_fit_their_bodies() {
        let materials = crate::animal_visual::material_mapping(&crate::game::manifest());
        for species in species().unwrap() {
            if species.habitat != AnimalHabitat::Land {
                continue;
            }
            let visual = crate::animal_visual::AnimalVisual::new(
                species.asset.clone(),
                &materials,
                crate::animal_visual::MAX_ANIMAL_VISUAL_CELLS,
            )
            .unwrap();
            let (lo, hi) = pose_bounds(&visual, species.key);
            let (radius, half_height) = species.capsule_m;
            let reach = (hi.x - lo.x).max(hi.z - lo.z);
            assert!(radius > 0.0 && half_height > 0.0, "{} capsule", species.key);
            assert!(
                2.0 * radius <= reach,
                "{} capsule wider than body",
                species.key
            );
            assert!(
                2.0 * (radius + half_height) <= hi.y - lo.y,
                "{} capsule taller than body",
                species.key
            );
            assert!(species.walk_speed_m_s > 0.0, "{} cannot walk", species.key);
        }
    }
}
