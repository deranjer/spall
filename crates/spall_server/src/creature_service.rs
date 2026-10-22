//! Game-owned creature director hook for the authoritative server.
//!
//! The game decides which creatures exist and where they go; the engine keeps
//! their authoritative poses and replicates them. A [`CreatureDirector`] is called
//! on the simulation thread with read-only access to the simulation. The records
//! it returns are spawned as one batch and announced to every client in order;
//! the ids it returns for despawn are removed the same way.

use std::sync::{Arc, Mutex};

use spall_protocol::{CreatureDespawn, CreatureRecord, CreatureSpawn, WIRE_SCHEMA_VERSION};
use spall_sim::Simulation;

/// Sequential id source for one batch of spawns.
#[derive(Debug, Clone, Copy)]
pub struct CreatureIdPlan {
    next_creature_id: u64,
}

impl CreatureIdPlan {
    pub fn new(next_creature_id: u64) -> Self {
        Self { next_creature_id }
    }

    /// Next creature id; call once per spawn, in the order the records are returned.
    pub fn creature_id(&mut self) -> u64 {
        let id = self.next_creature_id;
        self.next_creature_id += 1;
        id
    }
}

/// A movement the director wants applied this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum CreatureAction {
    /// A flying creature's step.
    Fly(spall_sim::CreatureMoveRequest),
    /// A walking creature's step.
    Walk(spall_sim::CreatureWalkRequest),
    /// A creature eats the ground cover in one terrain column (cells), through
    /// the same destroyed-cover record a surface edit makes.
    Graze { creature_id: u64, column: [i64; 2] },
}

impl CreatureAction {
    fn creature_id(&self) -> u64 {
        match self {
            Self::Fly(request) => request.creature_id,
            Self::Walk(request) => request.creature_id,
            Self::Graze { creature_id, .. } => *creature_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreatureActionOutcome {
    Fly(spall_sim::CreatureMoveOutcome),
    Walked(spall_sim::CreatureWalkOutcome),
    /// Whether there was cover to eat.
    Grazed(bool),
}

pub trait CreatureDirector: Send {
    /// The world was replaced (admin reset). All creatures are gone; forget any
    /// per-world bookkeeping.
    fn on_world_reset(&mut self) {}

    /// Movement for the creatures that exist, applied after this tick's spawns.
    /// Default: none.
    fn motion(&mut self, _tick: u64, _sim: &Simulation) -> Vec<CreatureAction> {
        Vec::new()
    }

    /// What the engine did with an action this director requested.
    fn on_action_outcome(&mut self, _creature_id: u64, _outcome: CreatureActionOutcome) {}

    /// Spawns after `tick`, with ids from `ids`. Returning nothing is the common case.
    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<CreatureRecord>;

    /// Creatures to remove after `tick`. Default: none.
    fn despawns(&mut self, _tick: u64, _sim: &Simulation) -> Vec<u64> {
        Vec::new()
    }
}

/// What one tick changed in the roster.
#[derive(Debug, Default)]
pub struct CreatureRoster {
    pub despawned: Vec<u64>,
    pub spawned: Vec<CreatureRecord>,
}

impl CreatureRoster {
    pub fn is_empty(&self) -> bool {
        self.despawned.is_empty() && self.spawned.is_empty()
    }

    /// The change as ordered wire records: despawns first, then spawns.
    pub fn into_wire_records(self) -> Vec<spall_net::WireRecord> {
        let despawns = self.despawned.into_iter().map(|creature_id| {
            spall_net::WireRecord::CreatureDespawn(CreatureDespawn {
                schema: WIRE_SCHEMA_VERSION,
                creature_id,
            })
        });
        let spawns = self.spawned.into_iter().map(|creature| {
            spall_net::WireRecord::CreatureSpawn(CreatureSpawn {
                schema: WIRE_SCHEMA_VERSION,
                creature,
            })
        });
        despawns.chain(spawns).collect()
    }
}

/// Cloneable handle so `ServeConfig` keeps its derives.
#[derive(Clone)]
pub struct CreatureService {
    director: Arc<Mutex<Box<dyn CreatureDirector>>>,
    motion_stats: Arc<Mutex<MotionStats>>,
}

/// Owning-thread cost of the movement hook, reported periodically.
#[derive(Debug, Default)]
struct MotionStats {
    ticks: u64,
    total: std::time::Duration,
    worst: std::time::Duration,
}

const MOTION_REPORT_TICKS: u64 = 600;

impl std::fmt::Debug for CreatureService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatureService").finish_non_exhaustive()
    }
}

impl CreatureService {
    pub fn new(director: Box<dyn CreatureDirector>) -> Self {
        Self {
            director: Arc::new(Mutex::new(director)),
            motion_stats: Arc::new(Mutex::new(MotionStats::default())),
        }
    }

    /// One `CreatureSpawn` per creature currently held, captured on the
    /// simulation thread at the same instant a joining client's ordered queue
    /// starts.
    pub fn join_records(&self, sim: &Simulation) -> Vec<CreatureSpawn> {
        sim.creature_records()
            .into_iter()
            .map(|creature| CreatureSpawn {
                schema: WIRE_SCHEMA_VERSION,
                creature,
            })
            .collect()
    }

    /// Collects the director's movement for this tick and applies it. A request
    /// the engine rejects is logged and skipped; it never stops the server.
    pub fn run_motion(&self, sim: &mut Simulation) -> Result<(), String> {
        let started = std::time::Instant::now();
        let result = self.run_motion_inner(sim);
        let elapsed = started.elapsed();
        if let Ok(mut stats) = self.motion_stats.lock() {
            stats.ticks += 1;
            stats.total += elapsed;
            stats.worst = stats.worst.max(elapsed);
            if stats.ticks >= MOTION_REPORT_TICKS {
                tracing::info!(
                    avg_ms = stats.total.as_secs_f64() * 1e3 / stats.ticks as f64,
                    worst_ms = stats.worst.as_secs_f64() * 1e3,
                    ticks = stats.ticks,
                    "creature motion hook cost"
                );
                *stats = MotionStats::default();
            }
        }
        result
    }

    fn run_motion_inner(&self, sim: &mut Simulation) -> Result<(), String> {
        let tick = sim.current_tick().get();
        let actions = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?
            .motion(tick, sim);
        let mut outcomes = Vec::with_capacity(actions.len());
        for action in actions {
            let creature_id = action.creature_id();
            let result = match action {
                CreatureAction::Fly(request) => {
                    sim.move_creature(request).map(CreatureActionOutcome::Fly)
                }
                CreatureAction::Walk(request) => sim
                    .walk_creature(request)
                    .map(CreatureActionOutcome::Walked),
                CreatureAction::Graze { column, .. } => Ok(CreatureActionOutcome::Grazed(
                    sim.graze_cover(column[0], column[1]),
                )),
            };
            match result {
                Ok(outcome) => outcomes.push((creature_id, outcome)),
                Err(error) => {
                    tracing::warn!(creature = creature_id, %error, "creature move rejected");
                }
            }
        }
        let mut director = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?;
        for (creature_id, outcome) in outcomes {
            director.on_action_outcome(creature_id, outcome);
        }
        Ok(())
    }

    /// Tells the director the world was replaced.
    pub fn on_world_reset(&self) -> Result<(), String> {
        self.director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?
            .on_world_reset();
        Ok(())
    }

    /// Applies this tick's despawns and spawns. Returns what changed, for the
    /// caller to announce. A director or simulation failure is returned to the
    /// caller, which must stop the server loudly.
    pub fn run_tick(&self, sim: &mut Simulation) -> Result<CreatureRoster, String> {
        let Some(next_creature_id) = sim.next_creature_id() else {
            return Err("creature director requires an initialized creature host".into());
        };
        let tick = sim.current_tick().get();
        let mut director = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?;
        let mut roster = CreatureRoster::default();
        for creature_id in director.despawns(tick, sim) {
            if sim.despawn_creature(creature_id) {
                roster.despawned.push(creature_id);
            }
        }
        let mut ids = CreatureIdPlan::new(next_creature_id);
        let spawned = director.plan(tick, sim, &mut ids);
        if !spawned.is_empty() {
            sim.spawn_creatures(spawned.clone())
                .map_err(|error| format!("creature spawn failed: {error}"))?;
            roster.spawned = spawned;
        }
        Ok(roster)
    }
}
