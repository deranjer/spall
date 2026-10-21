//! [`Simulation`] — the authoritative loop that ties staging, commit, and
//! physics together behind a single [`Simulation::tick`].
//!
//! One `tick` runs the in-scope steps of the `docs/architecture.md` tick order:
//! drain accepted intents, validate and commit prepared transactions in
//! server-assigned order with matching collision / ownership updates, advance
//! physics one fixed step, refresh the extracted body poses, and advance the
//! player capsules (T19). Contact-to-intent conversion (T21), replication (T10)
//! and persistence (T16) are handled by the integrator, not here.

use std::collections::{HashSet, VecDeque};

use glam::DVec3;
use spall_core::{
    BRUSH_UNIT, BrushPoint, EntityId, GlobalCell, IdError, MaterialId, PlayerInput, SphereBrush,
    Tick,
};
use spall_physics::PhysicsOrigin;
use spall_physics::{CharacterParams, CharacterState};
use spall_protocol::{ActionStatus, InputSeq, RequestId};

use crate::commit::CommitError;
use crate::contact_damage::{ContactDamagePlan, ContactDamagePolicy, ContactEvent};
use crate::dormancy::{ActiveRegion, BodyDormancyInput, DormancyPlan, DormancyPolicy};
use crate::intent::{EditIntent, EditKind, EditTarget, IntentError};
use crate::journal::JournalSink;
use crate::player::transaction_world_box;
use crate::schedule::{EditPipeline, TickReport};
use crate::world::{SimWorld, TerrainColliderMode, WorldSetup};
use spall_voxel::Sample;

#[path = "creature_motion.rs"]
mod creature_motion;
pub use creature_motion::{
    CreatureMoveOutcome, CreatureMoveRequest, CreaturePose, CreatureWalkOutcome,
    CreatureWalkRequest,
};

/// Reserved high bit for a server-authored [`RequestId`]. Contact-damage cuts
/// (T21) are minted by the server, not a client, so their request ids sit in a
/// band a wire `RequestId` never reaches — a client action claiming a value at
/// or above this is rejected at ingress.
pub const SERVER_REQUEST_ID_BAND: u64 = 1 << 62;

/// Journal provenance for a server-authored contact-damage cut. Authority is the
/// server's; `actor` is only journal provenance. Sits above the per-session
/// `actor_for` band and below the reserved player band (`1 << 48`).
pub const CONTACT_DAMAGE_ACTOR_ID: u64 = 1 << 40;

/// Journal provenance for a server-authored dam-gate toggle (an admin command,
/// not a player edit). Its own reserved id beside [`CONTACT_DAMAGE_ACTOR_ID`],
/// both below the reserved player band (`1 << 48`).
pub const DAM_GATE_ACTOR_ID: u64 = (1 << 40) + 1;

/// Largest single covering sphere [`Simulation::set_dam_gate`] will cut or
/// fill, in cells. A dam gate is typically authored as a flat, wide notch —
/// far from spherical — so covering it with clusters this size (rather than
/// one sphere sized to the whole shape) keeps the cut close to the authored
/// outline instead of ballooning out to the shape's own diagonal.
/// Snow tiles (4 m each) scanned around every player: 24 tiles = 96 m, the default view
/// distance; 30 tiles covers the 120 m maximum.
const SNOW_DISCOVERY_RING_TILES: i32 = 30;
const DAM_GATE_MAX_SPHERE_RADIUS_CELLS: i64 = 5;

/// The fixed server timestep: 60 Hz (`docs/architecture.md`).
pub const TICK_DT_S: f32 = 1.0 / 60.0;

/// Tunables for a [`Simulation`].
pub struct SimulationConfig {
    pub world: WorldSetup,
    /// Per-brick collision is the adopted default. Whole-terrain is retained
    /// only for explicit comparison runs.
    pub terrain_collider_mode: TerrainColliderMode,
    /// Maximum accepted-but-unstaged intents held before backpressure.
    pub max_pending_intents: usize,
    /// Consecutive commit conflicts on one region before it is routed through
    /// the serial queue.
    pub serialize_threshold: u32,
    /// Stacked-column water (ENG-167). `None` preserves non-fluid fixtures.
    pub column_water: Option<crate::column_water::ColumnWaterSetup>,
    /// Step the column water on its own thread (a server's choice; tests and
    /// tools keep the deterministic inline step).
    pub column_water_worker: bool,
    pub vegetation: Option<spall_ecology::living::LivingState>,
    pub weather_seed: u64,
    /// The game's snow material: near players, snow then also stacks as real terrain cells.
    pub snow_layer_material: Option<spall_core::MaterialId>,
}

impl SimulationConfig {
    /// Default accepted-but-unstaged intent backlog.
    pub const DEFAULT_MAX_PENDING_INTENTS: usize = 256;
    /// Default consecutive-conflict count before a region is serialized.
    pub const DEFAULT_SERIALIZE_THRESHOLD: u32 = 3;

    pub fn new(world: WorldSetup) -> Self {
        Self {
            world,
            terrain_collider_mode: TerrainColliderMode::PerBrick,
            max_pending_intents: Self::DEFAULT_MAX_PENDING_INTENTS,
            serialize_threshold: Self::DEFAULT_SERIALIZE_THRESHOLD,
            column_water: None,
            column_water_worker: false,
            vegetation: None,
            weather_seed: 0,
            snow_layer_material: None,
        }
    }
}

/// Error advancing the simulation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TickError {
    #[error(transparent)]
    Commit(#[from] CommitError),
    #[error("tick counter exhausted")]
    TickExhausted,
    #[error("authoritative water tick failed: {0}")]
    Water(String),
    #[error("authoritative snow tick failed: {0}")]
    Snow(String),
}

/// What one [`Simulation::apply_contact_damage`] pass did: the pure
/// [`ContactDamagePlan`] plus how its cuts fared at admission.
#[derive(Debug, Clone, Default)]
pub struct ContactDamageReport {
    /// The policy's decision for this tick.
    pub plan: ContactDamagePlan,
    /// Planned cuts accepted into the edit pipeline.
    pub submitted: usize,
    /// Planned cuts refused admission (pipeline queue full) — bounded
    /// backpressure, not an error.
    pub rejected: usize,
}

/// The authoritative simulation.
pub struct Simulation {
    pub(crate) world: SimWorld,
    pipeline: EditPipeline,
    journal: JournalSink,
    pub(crate) tick: Tick,
    next_control_seq: u64,
    next_damage_seq: u64,
    column_water: Option<crate::column_water::AuthoritativeColumnWater>,
    /// Whether the column water steps on its own thread.
    column_water_worker: bool,
    /// Sample points of each body for buoyancy, rebuilt when a body changes.
    buoyancy_cache: crate::world::BuoyancyCache,
    pub(crate) vegetation: Option<crate::vegetation::AuthoritativeVegetation>,
    pub(crate) weather: spall_protocol::WeatherState,
    pub(crate) weather_dirty: bool,
    weather_seed: u64,
    pub(crate) snow: crate::snow::SnowState,
    pub(crate) snow_layer: snow_layer::SnowLayer,
    snow_priority_tiles: Vec<(i32, i32)>,
    snow_priority_players: Vec<(i32, i32)>,
    snow_priority_refreshed_tick: u64,
    /// Per-player stride credit; proposals are validated by the snow owner.
    motion_footprints: crate::footprints::MotionFootprints,
    footfall_candidates: Vec<crate::footprints::FootfallCandidate>,
    /// The world a reset replaced, being freed on another thread (see [`Retiring`]).
    retiring: Retiring,
    /// Empty unless the owning application explicitly installs a creature host.
    creatures: Option<crate::creatures::CreatureHost>,
}

/// Frees a large value on a background thread. Dropping a full-size world (hundreds of thousands
/// of bricks and colliders) took about nine seconds on the tick thread during a world reset, with
/// every client waiting. A panic while dropping is not lost: it is re-raised on the owning thread
/// the next time the value is retired or the handle is finished.
#[derive(Default)]
struct Retiring(Option<std::thread::JoinHandle<()>>);

impl Retiring {
    /// Waits for the previous retirement, re-raising its panic, if any.
    fn finish(&mut self) {
        if let Some(handle) = self.0.take()
            && let Err(panic) = handle.join()
        {
            std::panic::resume_unwind(panic);
        }
    }

    fn retire<T: Send + 'static>(&mut self, old: T) {
        self.finish();
        // Moved into the thread through a slot so a failed spawn still frees it, inline.
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Some(old)));
        let thread_slot = std::sync::Arc::clone(&slot);
        match std::thread::Builder::new()
            .name("spall-retire-world".into())
            .spawn(move || {
                drop(
                    thread_slot
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .take(),
                );
            }) {
            Ok(handle) => self.0 = Some(handle),
            Err(_) => drop(slot.lock().unwrap_or_else(|p| p.into_inner()).take()),
        }
    }
}

mod construction;
mod contact_dormancy;
mod edit_queries;
mod helpers;
mod players;
mod snow_glue;
pub(crate) mod snow_layer;
mod tick;
mod water;
mod weather;
use helpers::*;

#[cfg(test)]
mod bounding_sphere_brush_tests;
#[cfg(test)]
mod contact_normal_tests;
#[cfg(test)]
mod covering_spheres_tests;
#[cfg(test)]
mod dam_gate_tests;
#[cfg(test)]
mod retiring_tests;
#[cfg(test)]
mod weather_clock_tests;
