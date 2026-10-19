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

pub trait CreatureDirector: Send {
    fn catalog(&self) -> Arc<OwnedCreatureCatalog>;

    /// The world was replaced (admin reset). All creatures are gone and the
    /// allocator epoch advanced; forget any per-world bookkeeping.
    fn on_world_reset(&mut self) {}

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
}

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
