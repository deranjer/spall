//! Game-owned creature director hook for the authoritative server.
//!
//! The engine owns ordering, validation and replication of creature lifecycle
//! transactions; the game decides what to spawn. A [`CreatureDirector`] is
//! called once per tick on the simulation thread with read-only access to the
//! simulation and an [`CreatureIdPlan`] that hands out the exact creature and
//! source-volume ids the lifecycle reducer requires. The returned intents are
//! committed as one ordered transaction and fanned out like any other commit.

use std::sync::{Arc, Mutex};

use spall_protocol::OwnedCreatureCatalog;
use spall_sim::{AuthoritativeCreatureIntent, Simulation};

/// Sequential id source for one batch of births.
#[derive(Debug, Clone, Copy)]
pub struct CreatureIdPlan {
    next_creature_id: u64,
    next_volume_id: u64,
}

impl CreatureIdPlan {
    pub fn new(next_creature_id: u64, next_volume_id: u64) -> Self {
        Self {
            next_creature_id,
            next_volume_id,
        }
    }

    /// Next creature id; call once per birth, in intent order.
    pub fn creature_id(&mut self) -> u64 {
        let id = self.next_creature_id;
        self.next_creature_id += 1;
        id
    }

    /// Next shared source-volume id; call once per source part, in part order.
    pub fn volume_id(&mut self) -> u64 {
        let id = self.next_volume_id;
        self.next_volume_id += 1;
        id
    }
}

/// A movement the director wants applied this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum CreatureAction {
    /// Exact swept pose and root transition (flying creatures, small populations).
    Swept(spall_sim::CreatureMoveRequest),
    /// Character-capsule locomotion with the pose staged without terrain casts.
    Walk(spall_sim::CreatureWalkRequest),
}

impl CreatureAction {
    fn key(&self) -> spall_core::CreatureKey {
        match self {
            Self::Swept(request) => request.key,
            Self::Walk(request) => request.key,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CreatureActionOutcome {
    Swept(spall_sim::CreatureMoveOutcome),
    /// `None`: a previous pose is not installed yet.
    Walked(Option<spall_sim::CreatureWalkOutcome>),
}

pub trait CreatureDirector: Send {
    fn catalog(&self) -> Arc<OwnedCreatureCatalog>;

    /// The world was replaced (admin reset). All creatures are gone and the
    /// allocator epoch advanced; forget any per-world bookkeeping.
    fn on_world_reset(&mut self) {}

    /// Movement for creatures that are alive, applied after this tick's births
    /// through the engine's swept articulated-pose path. Default: none.
    fn motion(&mut self, _tick: u64, _sim: &Simulation) -> Vec<CreatureAction> {
        Vec::new()
    }

    /// What the engine did with an action this director requested.
    fn on_action_outcome(
        &mut self,
        _key: spall_core::CreatureKey,
        _outcome: CreatureActionOutcome,
    ) {
    }

    /// Births to commit after `tick`. Returning nothing is the common case.
    fn plan(
        &mut self,
        tick: u64,
        sim: &Simulation,
        ids: &mut CreatureIdPlan,
    ) -> Vec<AuthoritativeCreatureIntent>;
}

/// Cloneable handle so `ServeConfig` keeps its derives.
#[derive(Clone)]
pub struct CreatureService {
    director: Arc<Mutex<Box<dyn CreatureDirector>>>,
    catalog: Arc<OwnedCreatureCatalog>,
    motion_stats: Arc<Mutex<MotionStats>>,
}

/// Owning-thread cost of the movement hook, reported periodically.
#[derive(Debug, Default)]
struct MotionStats {
    /// Creature id -> (blocked moves, last blocking part, last time of impact).
    blocked: std::collections::BTreeMap<u64, (u64, Option<u32>, Option<f32>)>,
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
        let catalog = director.catalog();
        Self {
            director: Arc::new(Mutex::new(director)),
            catalog,
            motion_stats: Arc::new(Mutex::new(MotionStats::default())),
        }
    }

    pub fn catalog(&self) -> &Arc<OwnedCreatureCatalog> {
        &self.catalog
    }

    /// One `CreatureSync` per creature currently held, captured on the
    /// simulation thread at the same instant a joining client's ordered
    /// transaction queue starts.
    pub fn join_sync(&self, sim: &Simulation) -> Result<Vec<spall_protocol::CreatureSync>, String> {
        let snapshot = self
            .catalog
            .with_view(|view| sim.capture_creature_snapshot(view))
            .map_err(|error| format!("creature catalog is invalid: {error}"))?
            .map_err(|error| format!("creature capture failed: {error:?}"))?;
        let Some(snapshot) = snapshot else {
            return Ok(Vec::new());
        };
        Ok(snapshot
            .owners
            .records
            .into_iter()
            .map(|record| spall_protocol::CreatureSync {
                schema: spall_protocol::CREATURE_SYNC_SCHEMA,
                allocator: snapshot.allocator,
                next_volume_id: snapshot.next_volume_id,
                record,
            })
            .collect())
    }

    /// Collects the director's movement for this tick and stages it. A request
    /// the engine rejects (stale or invalid) is logged and skipped; it never
    /// stops the server.
    pub fn run_motion(&self, sim: &mut Simulation) -> Result<(), String> {
        let started = std::time::Instant::now();
        let result = self.run_motion_inner(sim);
        let elapsed = started.elapsed();
        if let Ok(mut stats) = self.motion_stats.lock() {
            stats.ticks += 1;
            stats.total += elapsed;
            stats.worst = stats.worst.max(elapsed);
            if stats.ticks >= MOTION_REPORT_TICKS {
                let timing = sim.take_creature_move_timing();
                tracing::info!(
                    moves = timing.moves,
                    blocked = timing.blocked,
                    terrain_candidates = timing.terrain_candidates,
                    cell_casts = timing.cell_casts,
                    query_sync_ms = timing.query_sync.as_secs_f64() * 1e3,
                    candidate_query_ms = timing.candidate_query.as_secs_f64() * 1e3,
                    exact_cast_ms = timing.exact_cast.as_secs_f64() * 1e3,
                    "creature move transitions"
                );
                let mut worst: Vec<_> = stats.blocked.iter().collect();
                worst.sort_by_key(|(_, (count, _, _))| std::cmp::Reverse(*count));
                tracing::info!(worst_blocked = ?worst.iter().take(5).collect::<Vec<_>>(), "creature move blockers (id -> count, part, toi)");
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
            let key = action.key();
            let result = match action {
                CreatureAction::Swept(request) => {
                    sim.move_creature(request).map(CreatureActionOutcome::Swept)
                }
                CreatureAction::Walk(request) => sim
                    .walk_creature(request)
                    .map(CreatureActionOutcome::Walked),
            };
            match result {
                Ok(outcome) => outcomes.push((key, outcome)),
                Err(error) => {
                    tracing::warn!(creature = key.creature_id.get(), %error, "creature move rejected");
                }
            }
        }
        let mut director = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?;
        if let Ok(mut stats) = self.motion_stats.lock() {
            for (key, outcome) in &outcomes {
                if let CreatureActionOutcome::Swept(spall_sim::CreatureMoveOutcome::Blocked {
                    part_id,
                    time_of_impact,
                }) = outcome
                {
                    let entry = stats
                        .blocked
                        .entry(key.creature_id.get())
                        .or_insert((0, None, None));
                    entry.0 += 1;
                    entry.1 = *part_id;
                    entry.2 = *time_of_impact;
                }
            }
        }
        for (key, outcome) in outcomes {
            director.on_action_outcome(key, outcome);
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

    /// Plans and commits this tick's births. Returns the committed lifecycle
    /// transaction, if any intent was produced. A director or reducer failure
    /// is returned to the caller, which must stop the server loudly.
    pub fn run_tick(
        &self,
        sim: &mut Simulation,
    ) -> Result<Option<spall_sim::CommittedCreatureLifecycle>, String> {
        let Some((next_creature, next_volume)) = sim.creature_birth_ids() else {
            return Err("creature director requires an initialized creature host".into());
        };
        let mut ids = CreatureIdPlan::new(next_creature, next_volume);
        let tick = sim.current_tick().get();
        let intents = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?
            .plan(tick, sim, &mut ids);
        if intents.is_empty() {
            return Ok(None);
        }
        self.catalog
            .with_view(|view| sim.commit_creature_lifecycle_now(intents, view))
            .map_err(|error| format!("creature catalog is invalid: {error}"))?
            .map(Some)
    }
}
