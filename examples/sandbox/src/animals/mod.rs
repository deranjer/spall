//! Sandbox-owned live animals: the species list and the server-side spawn
//! director. The engine keeps each animal's authoritative pose and replicates
//! it; this module only decides which authored species exist, where they
//! appear and where they walk or fly.
//!
//! Each species belongs to a [`Family`] whose file owns its behaviour: its
//! gait states, its per-update step and its reaction to action outcomes. This
//! module keeps the species table, placement and the motion loop, which hands
//! every creature to its family's `step` and turns the returned [`Step`] into
//! an engine action.

mod bears;
mod birds;
mod deer;
mod fish;
mod land;
mod leopards;
mod players;
mod wolves;

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

/// Which file owns a species' behaviour.
#[derive(Debug, Clone, Copy)]
enum Family {
    Deer,
    Bird,
    Fish,
    Bear,
    Leopard,
    Wolf,
}

/// Per-species authoring and locomotion choices; everything else (envelope,
/// foot height) is measured from the asset.
struct SpeciesDef {
    key: &'static str,
    family: Family,
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
    /// A flyer that sometimes lands, hops and pecks on the ground, walking on
    /// its `capsule_m` there.
    perches: bool,
    /// Lives on or near mountains: spawns on high or steep ground and wanders
    /// no lower than a little under where it was born.
    uplands: bool,
}

const SPECIES: [SpeciesDef; 6] = [
    SpeciesDef {
        key: "deer",
        family: Family::Deer,
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../../assets/animals/deer.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.4,
        capsule_m: (0.35, 0.35),
        // Faster than a sprinting player (5.8 m/s), so a chase is lost.
        flee_speed_m_s: Some(7.5),
        grazes: true,
        perches: false,
        uplands: false,
    },
    SpeciesDef {
        key: "crow",
        family: Family::Bird,
        habitat: AnimalHabitat::Flying,
        bytes: include_bytes!("../../assets/animals/crow.spvox"),
        move_clip: "flap",
        // Hopping pace on the ground.
        walk_speed_m_s: 0.8,
        // Feet and folded body, for hopping on the ground.
        capsule_m: (0.08, 0.04),
        flee_speed_m_s: None,
        grazes: false,
        perches: true,
        uplands: false,
    },
    SpeciesDef {
        key: "trout",
        family: Family::Fish,
        habitat: AnimalHabitat::Aquatic,
        bytes: include_bytes!("../../assets/animals/trout.spvox"),
        move_clip: "swim",
        walk_speed_m_s: 0.0,
        capsule_m: (0.0, 0.0),
        flee_speed_m_s: None,
        grazes: false,
        perches: false,
        uplands: false,
    },
    SpeciesDef {
        key: "bear",
        family: Family::Bear,
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../../assets/animals/bear.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.2,
        capsule_m: (0.5, 0.12),
        flee_speed_m_s: None,
        grazes: false,
        perches: false,
        uplands: false,
    },
    SpeciesDef {
        key: "snow_leopard",
        family: Family::Leopard,
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../../assets/animals/leopard.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.1,
        capsule_m: (0.28, 0.1),
        flee_speed_m_s: None,
        grazes: false,
        perches: false,
        uplands: true,
    },
    SpeciesDef {
        key: "wolf",
        family: Family::Wolf,
        habitat: AnimalHabitat::Land,
        bytes: include_bytes!("../../assets/animals/wolf.spvox"),
        move_clip: "walk",
        walk_speed_m_s: 1.4,
        capsule_m: (0.22, 0.1),
        flee_speed_m_s: None,
        grazes: false,
        perches: false,
        uplands: false,
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
/// A perching flyer descending to the ground (`land` clip).
pub const LOCOMOTION_LAND: u8 = 5;
/// Hopping along the ground (`hop` clip).
pub const LOCOMOTION_HOP: u8 = 6;
/// Pecking at the ground where it stands (`peck` clip).
pub const LOCOMOTION_PECK: u8 = 7;
/// Climbing from the ground back to its circle (`takeoff` clip).
pub const LOCOMOTION_TAKEOFF: u8 = 8;
/// A deer lying asleep at night (`sleep` clip).
pub const LOCOMOTION_SLEEP: u8 = 9;
/// A crow sitting in its nest for the night (its `idle` clip).
pub const LOCOMOTION_ROOST: u8 = 10;
/// A predator creeping up on prey (a bear's slow `walk`).
pub const LOCOMOTION_STALK: u8 = 20;
/// A predator's killing blow (a bear's `swipe`).
pub const LOCOMOTION_POUNCE: u8 = 21;
/// A bear eating a carcass (`eat` clip).
pub const LOCOMOTION_EAT: u8 = 22;
/// A bear rearing up at a player who came too close (`stand` clip).
pub const LOCOMOTION_REAR: u8 = 23;
/// A wolf howling, muzzle to the sky (`howl` clip).
pub const LOCOMOTION_HOWL: u8 = 24;
/// A wolf running in at its prey (`run` clip).
pub const LOCOMOTION_CHASE: u8 = 25;
/// A wolf backing away from a bear or a fire (`run` clip).
pub const LOCOMOTION_SHY: u8 = 26;
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
        LOCOMOTION_LAND if def.perches => Some("land"),
        LOCOMOTION_HOP if def.perches => Some("hop"),
        LOCOMOTION_PECK if def.perches => Some("peck"),
        LOCOMOTION_TAKEOFF if def.perches => Some("takeoff"),
        LOCOMOTION_ROOST if def.perches => Some("idle"),
        LOCOMOTION_SLEEP if matches!(def.family, Family::Deer) => Some("sleep"),
        LOCOMOTION_RUN | LOCOMOTION_STALK | LOCOMOTION_POUNCE | LOCOMOTION_EAT
        | LOCOMOTION_REAR
            if matches!(def.family, Family::Bear) =>
        {
            bears::clip(tag)
        }
        LOCOMOTION_RUN | LOCOMOTION_STALK | LOCOMOTION_POUNCE | LOCOMOTION_EAT
        | LOCOMOTION_HOWL | LOCOMOTION_CHASE | LOCOMOTION_SHY
            if matches!(def.family, Family::Wolf) =>
        {
            wolves::clip(tag)
        }
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
    pub perches: bool,
    pub uplands: bool,
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
                perches: def.perches,
                uplands: def.uplands,
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

/// What a creature is doing. The calm land gaits and the player reactions are
/// shared; each family with states of its own carries them in one variant
/// whose enum lives in that family's file.
#[derive(Debug, Clone, Copy)]
enum Gait {
    /// Stand still until this tick.
    Rest {
        until: u64,
    },
    /// Walk toward `target` (x, z) until reached, blocked, or `until`.
    Walk {
        target: [f64; 2],
        until: u64,
    },
    /// Stand facing a player who came near, until `until`.
    Alert {
        until: u64,
    },
    /// Run away from players until clear of them or `until`.
    Flee {
        until: u64,
    },
    Deer(deer::DeerGait),
    Bird(birds::BirdGait),
    Fish(fish::FishGait),
    Bear(bears::BearGait),
    Wolf(wolves::WolfGait),
}

fn wrap_angle(angle: f64) -> f64 {
    (angle + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU) - std::f64::consts::PI
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
    /// Highest a circling bird's `home` may climb (ENG-230).
    max_home_y: f64,
}

/// What one creature sees this update, handed to its family's `step`.
struct Cx<'a> {
    sim: &'a Simulation,
    id: u64,
    here: [f64; 3],
    tick: u64,
    clock: spall_core::time_of_day::DayClock,
    /// The nearest player, horizontally.
    threat: Option<players::Threat>,
}

/// What one creature does this update; the motion loop turns it into an
/// engine action.
struct Step {
    tag: u8,
    yaw: f64,
    /// Horizontal step for a walker, metres.
    desired: [f64; 2],
    /// Root target for a flyer.
    fly_target: [f64; 3],
    /// Set for a flying step, `Some(true)` near the ground, on the small
    /// ground capsule; `None` walks.
    fly: Option<bool>,
}

impl Step {
    /// Standing where it is, facing the way it faces.
    fn still(walker: &Walker, here: [f64; 3]) -> Self {
        Self {
            tag: LOCOMOTION_IDLE,
            yaw: walker.yaw,
            desired: [0.0; 2],
            fly_target: here,
            fly: None,
        }
    }
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
    /// Root-to-lowest-foot offset over the ground clips (idle, hop, peck) of a
    /// perching flyer, whose wingbeats reach lower than its feet.
    perch_feet: Vec<f32>,
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
    /// Hunger and the carcass being eaten, per bear.
    bears: std::collections::HashMap<u64, bears::Bear>,
    /// Nest site and building progress, per crow.
    crows: std::collections::HashMap<u64, birds::Crow>,
    /// Wolf packs and their shared hunt, meal and howl.
    wolves: wolves::Packs,
    /// Day length and start hour, shared with the clients (the server day clock).
    clock: spall_core::time_of_day::DayClock,
    last_tick: u64,
    /// Per species: (staged moves, blocked moves, walks started), logged periodically.
    stats: Vec<(u64, u64, u64)>,
}

impl AnimalDirector {
    /// Sleep, roost and howl by this day clock instead of the default one, so
    /// animals agree with the sky the clients draw.
    pub fn with_day_clock(mut self, clock: spall_core::time_of_day::DayClock) -> Self {
        self.clock = clock;
        self
    }

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
        let perch_feet = species
            .iter()
            .zip(&visuals)
            .zip(&walk_feet)
            .map(|((species, visual), &feet)| {
                if species.perches {
                    clip_bounds(visual, &["idle", "hop", "peck"]).0.y
                } else {
                    feet
                }
            })
            .collect();
        let count = species.len();
        Ok(Self {
            species,
            walk_feet,
            envelope_m,
            mouth_m,
            perch_feet,
            grass: deer::grass_materials(),
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
            bears: std::collections::HashMap::new(),
            crows: std::collections::HashMap::new(),
            wolves: wolves::Packs::default(),
            clock: Default::default(),
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

    fn find_placement(&mut self, sim: &Simulation, species: usize) -> Option<Placement> {
        if self.anchors.is_empty() {
            return None;
        }
        if let Family::Fish = SPECIES[species].family {
            return fish::find_placement(self, sim, species);
        }
        let habitat = self.species[species].habitat;
        let pick = (self.next() as usize) % self.anchors.len();
        let anchor = self.anchors[pick];
        let uplands = self.species[species].uplands;
        let reach = if uplands {
            self.radius_m.1 * leopards::UPLAND_SEARCH_SCALE
        } else {
            self.radius_m.1
        };
        let angle = self.unit() * std::f64::consts::TAU;
        let radius = self.radius_m.0 + self.unit() * (reach - self.radius_m.0);
        let (mut x, mut z) = (
            anchor[0] + radius * angle.cos(),
            anchor[2] + radius * angle.sin(),
        );
        // Wolves are born beside the rest of their pack.
        if let Family::Wolf = SPECIES[species].family
            && let Some([den_x, den_z]) = wolves::den_spot(self)
        {
            (x, z) = (den_x, den_z);
        }
        let (x_cell, z_cell) = ((x / 0.25).floor() as i64, (z / 0.25).floor() as i64);
        let ground = Self::ground_y_m(sim, x_cell, z_cell)?;
        if uplands && !leopards::upland_birthplace(sim, x_cell, z_cell, ground, anchor[1]) {
            return None;
        }
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
            // Species unfilled so far take turns, so one that is hard to place
            // (snow leopards with no mountain near) cannot use up every attempt
            // and starve the species after it.
            let count = self.species.len();
            let first = self.attempts as usize % count;
            let Some(species) = (0..count)
                .map(|i| (first + i) % count)
                .find(|&i| self.spawned[i] < self.targets[i])
            else {
                break;
            };
            let Some(placement) = self.find_placement(sim, species) else {
                continue;
            };
            let record = self.birth(&placement, ids);
            let root = placement.root_m.to_array();
            // The family picks the first gait and the home it keeps.
            let (gait, home) = match SPECIES[species].family {
                Family::Bird => birds::start(self, sim, root),
                Family::Fish => fish::start(self, tick, root),
                Family::Deer | Family::Bear | Family::Leopard | Family::Wolf => {
                    (land::start(self, tick), root)
                }
            };
            if let Family::Wolf = SPECIES[species].family {
                wolves::join(self, record.creature_id, tick);
            }
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
                    at: root,
                    max_home_y: home[1] + birds::MAX_ORBIT_CLIMB_M,
                },
            );
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
        intents
    }

    fn on_world_reset(&mut self) {
        self.spawned = vec![0; self.species.len()];
        self.attempts = 0;
        self.walkers.clear();
        self.bears.clear();
        self.crows.clear();
        self.wolves.clear();
    }

    /// Fish a bear caught: gone whole, no carcass.
    fn despawns(&mut self, _tick: u64, _sim: &Simulation) -> Vec<u64> {
        bears::caught_fish(&mut self.bears)
    }

    fn on_action_outcome(&mut self, creature_id: u64, outcome: CreatureActionOutcome) {
        let Some(walker) = self.walkers.get_mut(&creature_id) else {
            return;
        };
        match outcome {
            CreatureActionOutcome::Fly(CreatureMoveOutcome::Blocked) => {
                self.stats[walker.species].1 += 1;
                birds::on_fly_blocked(walker);
            }
            CreatureActionOutcome::Fly(CreatureMoveOutcome::Moved) => {
                self.stats[walker.species].0 += 1;
                birds::on_fly_moved(walker);
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
            CreatureActionOutcome::Grazed(ate) => deer::on_grazed(walker, ate, self.last_tick),
            outcome @ (CreatureActionOutcome::Killed(_) | CreatureActionOutcome::HitPlayer(_))
                if matches!(SPECIES[walker.species].family, Family::Wolf) =>
            {
                wolves::on_outcome(&mut self.wolves, creature_id, outcome, self.last_tick)
            }
            CreatureActionOutcome::Killed(kill) => {
                bears::on_killed(&mut self.bears, creature_id, kill, self.last_tick)
            }
            CreatureActionOutcome::CellEdited(_) | CreatureActionOutcome::HitPlayer(_) => {}
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
        let players = players::threat_sources(self, sim);
        let poses = sim.creature_poses();
        // A killed animal leaves the roster; forget its walker with it.
        self.walkers
            .retain(|id, _| poses.iter().any(|pose| pose.creature_id == *id));
        let walkers = &self.walkers;
        self.bears.retain(|id, _| walkers.contains_key(id));
        self.crows.retain(|id, _| walkers.contains_key(id));
        self.wolves.retain(|id| walkers.contains_key(&id));
        let mut actions = Vec::new();
        for alive in poses {
            let id = alive.creature_id;
            if !(tick + id).is_multiple_of(MOVE_PERIOD_TICKS) {
                continue;
            }
            let Some(mut walker) = self.walkers.get(&id).copied() else {
                continue;
            };
            let here = alive.translation_m;
            walker.at = here;
            let cx = Cx {
                sim,
                id,
                here,
                tick,
                clock: self.clock,
                threat: players::nearest_threat(&players, here),
            };
            let Step {
                tag,
                yaw,
                desired,
                fly_target,
                fly,
            } = match SPECIES[walker.species].family {
                Family::Deer => deer::step(self, &mut walker, &cx, &mut actions),
                Family::Bird => birds::step(self, &mut walker, &cx, &mut actions),
                Family::Fish => fish::step(self, &mut walker, &cx, &mut actions),
                Family::Bear => bears::step(self, &mut walker, &cx, &mut actions),
                Family::Leopard => leopards::step(self, &mut walker, &cx, &mut actions),
                Family::Wolf => wolves::step(self, &mut walker, &cx, &mut actions),
            };
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
            // switch back to idle; a resting one sends nothing, and neither does
            // a sleeper after lying down (clients run its clip on their own).
            if tag == was && (tag == LOCOMOTION_IDLE || tag == LOCOMOTION_SLEEP) {
                continue;
            }
            let phase = if tag == LOCOMOTION_IDLE {
                0
            } else {
                walker.clip_phase
            };
            let (sin, cos) = ((yaw * 0.5).sin(), (yaw * 0.5).cos());
            let rotation = [0.0, sin as f32, 0.0, cos as f32];
            let (radius_m, half_height_m) = self.species[walker.species].capsule_m;
            // Near the ground a perching bird folds its wings: its feet and
            // capsule are the ground ones.
            let ground_feet = self.perch_feet[walker.species];
            actions.push(match fly {
                Some(false) => CreatureAction::Fly(CreatureMoveRequest {
                    creature_id: id,
                    target_translation_m: fly_target,
                    target_rotation_xyzw: rotation,
                    locomotion_tag: tag,
                    clip_phase_tick: phase,
                    feet_offset_m: self.walk_feet[walker.species],
                    capsule_radius_m: FLY_CAPSULE_RADIUS_M,
                    capsule_half_height_m: FLY_CAPSULE_HALF_HEIGHT_M,
                }),
                Some(true) => CreatureAction::Fly(CreatureMoveRequest {
                    creature_id: id,
                    target_translation_m: fly_target,
                    target_rotation_xyzw: rotation,
                    locomotion_tag: tag,
                    clip_phase_tick: phase,
                    feet_offset_m: ground_feet,
                    capsule_radius_m: radius_m,
                    capsule_half_height_m: half_height_m,
                }),
                None => CreatureAction::Walk(CreatureWalkRequest {
                    creature_id: id,
                    desired_step_m: [desired[0] as f32, desired[1] as f32],
                    target_rotation_xyzw: rotation,
                    locomotion_tag: tag,
                    clip_phase_tick: phase,
                    feet_offset_m: ground_feet,
                    capsule_radius_m: radius_m,
                    capsule_half_height_m: half_height_m,
                }),
            });
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
            if species.key == "deer" {
                sends.push(LOCOMOTION_SLEEP);
            }
            if species.key == "bear" {
                sends.extend([
                    LOCOMOTION_RUN,
                    LOCOMOTION_STALK,
                    LOCOMOTION_POUNCE,
                    LOCOMOTION_EAT,
                    LOCOMOTION_REAR,
                ]);
            }
            if species.key == "wolf" {
                sends.extend([
                    LOCOMOTION_RUN,
                    LOCOMOTION_STALK,
                    LOCOMOTION_POUNCE,
                    LOCOMOTION_EAT,
                    LOCOMOTION_HOWL,
                    LOCOMOTION_CHASE,
                    LOCOMOTION_SHY,
                ]);
            }
            if species.perches {
                sends.extend([
                    LOCOMOTION_LAND,
                    LOCOMOTION_HOP,
                    LOCOMOTION_PECK,
                    LOCOMOTION_TAKEOFF,
                    LOCOMOTION_ROOST,
                ]);
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
            if species.habitat != AnimalHabitat::Land && !species.perches {
                continue;
            }
            let visual = crate::animal_visual::AnimalVisual::new(
                species.asset.clone(),
                &materials,
                crate::animal_visual::MAX_ANIMAL_VISUAL_CELLS,
            )
            .unwrap();
            // A perching bird walks with its wings folded.
            let (lo, hi) = if species.perches {
                clip_bounds(&visual, &["idle", "hop", "peck"])
            } else {
                pose_bounds(&visual, species.key)
            };
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
