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

/// Farthest a player's feet may be from a creature's root for that creature's
/// hit to land (ENG-240). The server refuses a director's hit beyond it.
const HIT_PLAYER_REACH_M: f64 = 3.0;

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
    /// The creature kills `prey`, which dies as from a strike: despawned,
    /// announced to every client, and left as a terrain carcass.
    Kill { creature_id: u64, prey: u64 },
    /// The creature changes one terrain cell through the committed edit path:
    /// `place` fills it if empty, `None` clears it if solid.
    EditCell {
        creature_id: u64,
        cell: spall_core::GlobalCell,
        place: Option<spall_core::MaterialId>,
    },
    /// The creature hits `player` for `damage` health through
    /// [`Simulation::damage_player`]; refused when the player's feet are more
    /// than 3 m from the creature's root.
    HitPlayer {
        creature_id: u64,
        player: spall_core::EntityId,
        damage: u16,
    },
}

impl CreatureAction {
    fn creature_id(&self) -> u64 {
        match self {
            Self::Fly(request) => request.creature_id,
            Self::Walk(request) => request.creature_id,
            Self::Graze { creature_id, .. }
            | Self::Kill { creature_id, .. }
            | Self::EditCell { creature_id, .. }
            | Self::HitPlayer { creature_id, .. } => *creature_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CreatureActionOutcome {
    Fly(spall_sim::CreatureMoveOutcome),
    Walked(spall_sim::CreatureWalkOutcome),
    /// Whether there was cover to eat.
    Grazed(bool),
    /// The prey died; its carcass cells are queued.
    Killed(spall_sim::CreatureKill),
    /// Whether the cell edit was queued (`false`: the cell already was so).
    CellEdited(bool),
    /// The player was hurt, or killed and respawned.
    HitPlayer(spall_sim::PlayerDamage),
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

/// A creature's hit on a player, at the tick boundary. Refused for an unknown
/// creature or player, or one out of [`HIT_PLAYER_REACH_M`].
fn hit_player(
    sim: &mut Simulation,
    creature_id: u64,
    player: spall_core::EntityId,
    damage: u16,
) -> Result<spall_sim::PlayerDamage, String> {
    let root = sim
        .creature_poses()
        .into_iter()
        .find(|pose| pose.creature_id == creature_id)
        .ok_or("unknown creature")?
        .translation_m;
    let feet = sim.player_state(player).ok_or("unknown player")?.position_m;
    let distance = (0..3)
        .map(|i| (feet[i] - root[i]).powi(2))
        .sum::<f64>()
        .sqrt();
    if distance > HIT_PLAYER_REACH_M {
        return Err(format!("player out of reach ({distance:.1} m)"));
    }
    let outcome = sim.damage_player(player, damage).ok_or("unknown player")?;
    tracing::info!(
        creature = creature_id,
        player = player.get(),
        damage,
        ?outcome,
        "creature hit a player"
    );
    Ok(outcome)
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
    /// Returns the creatures its kills removed, for the caller to announce.
    pub fn run_motion(&self, sim: &mut Simulation) -> Result<CreatureRoster, String> {
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

    fn run_motion_inner(&self, sim: &mut Simulation) -> Result<CreatureRoster, String> {
        let tick = sim.current_tick().get();
        let actions = self
            .director
            .lock()
            .map_err(|_| "creature director panicked earlier".to_string())?
            .motion(tick, sim);
        let mut outcomes = Vec::with_capacity(actions.len());
        let mut roster = CreatureRoster::default();
        for action in actions {
            let creature_id = action.creature_id();
            // A creature killed earlier in this batch does nothing more.
            if roster.despawned.contains(&creature_id) {
                continue;
            }
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
                CreatureAction::Kill { prey, .. } => sim
                    .kill_creature_by(creature_id, prey)
                    .map(|kill| {
                        tracing::info!(
                            creature = creature_id,
                            prey,
                            species = %kill.record.species,
                            carcass_cells = kill.carcass.len(),
                            "creature killed by a creature"
                        );
                        roster.despawned.push(prey);
                        CreatureActionOutcome::Killed(kill)
                    })
                    .map_err(|error| error.to_string()),
                CreatureAction::EditCell { cell, place, .. } => sim
                    .creature_edit_cell(creature_id, cell, place)
                    .map(CreatureActionOutcome::CellEdited),
                CreatureAction::HitPlayer { player, damage, .. } => {
                    hit_player(sim, creature_id, player, damage)
                        .map(CreatureActionOutcome::HitPlayer)
                }
            };
            match result {
                Ok(outcome) => outcomes.push((creature_id, outcome)),
                Err(error) => {
                    tracing::warn!(creature = creature_id, %error, "creature action rejected");
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
        Ok(roster)
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

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::player_entity_for;
    use spall_sim::{PLAYER_MAX_HEALTH, SimulationConfig, fixtures};

    /// Spawns one creature, then hits both players for 30 on every motion call.
    struct Hitter;

    impl CreatureDirector for Hitter {
        fn plan(
            &mut self,
            tick: u64,
            _: &Simulation,
            ids: &mut CreatureIdPlan,
        ) -> Vec<CreatureRecord> {
            if tick > 0 {
                return Vec::new();
            }
            vec![CreatureRecord {
                creature_id: ids.creature_id(),
                species: "bear".into(),
                translation_m: [4.0, 0.5, 4.0],
                rotation_xyzw: [0.0, 0.0, 0.0, 1.0],
                locomotion_tag: 0,
                clip_phase_tick: 0,
            }]
        }

        fn motion(&mut self, _: u64, sim: &Simulation) -> Vec<CreatureAction> {
            let Some(pose) = sim.creature_poses().first().copied() else {
                return Vec::new();
            };
            [0, 1]
                .map(|slot| CreatureAction::HitPlayer {
                    creature_id: pose.creature_id,
                    player: player_entity_for(slot),
                    damage: 30,
                })
                .into()
        }
    }

    #[test]
    fn a_creature_hits_only_a_player_within_reach() {
        let mut sim =
            Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).unwrap();
        sim.initialize_creature_host().unwrap();
        let near = sim.add_player(player_entity_for(0), [5.5, 0.5, 4.0]);
        let far = sim.add_player(player_entity_for(1), [7.5, 0.5, 4.0]);
        let service = CreatureService::new(Box::new(Hitter));
        service.run_tick(&mut sim).unwrap();
        service.run_motion(&mut sim).unwrap();
        assert_eq!(sim.player_health(near), Some(PLAYER_MAX_HEALTH - 30));
        assert_eq!(
            sim.player_health(far),
            Some(PLAYER_MAX_HEALTH),
            "3.5 m away"
        );
        for _ in 0..3 {
            service.run_motion(&mut sim).unwrap();
        }
        // The fourth hit was lethal: full health again, back at the spawn.
        assert_eq!(sim.player_health(near), Some(PLAYER_MAX_HEALTH));
        assert_eq!(sim.player_health(far), Some(PLAYER_MAX_HEALTH));
    }
}
