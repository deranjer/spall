//! The game's per-tick rules (ENG-251): after each tick the game may look at
//! the world, change it (respawn items), and announce [`GameEvent`]s to one
//! live session or to all of them. The engine carries the events and never
//! interprets them.
use super::*;
use spall_protocol::GameEvent;

/// Who a [`GameEvent`] goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Every session that is live now.
    All,
    /// One session.
    Session(SessionId),
}

/// The game's side. Runs on the authoritative thread at the end of every
/// tick, after that tick's damage, so it must be quick.
pub trait TickRules: Send {
    /// `live` lists the sessions that are live this tick (their players are
    /// `spall_protocol::session_player_entity(session)`). Returns the events
    /// to send, in order.
    fn after_tick(
        &mut self,
        sim: &mut Simulation,
        live: &[SessionId],
    ) -> Vec<(Audience, GameEvent)>;
}

/// Cloneable handle to the game's [`TickRules`], so `ServeConfig` keeps its
/// derives. Without one no game event is ever sent.
#[derive(Clone)]
pub struct GameTick(Arc<Mutex<Box<dyn TickRules>>>);

impl GameTick {
    pub fn new(rules: impl TickRules + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(rules))))
    }
}

impl std::fmt::Debug for GameTick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GameTick")
    }
}

/// Runs the game's rules for this tick and sends what they announce.
pub(super) fn run_tick_rules(
    rules: Option<&GameTick>,
    sim: &mut Simulation,
    lj: &LateJoin,
    clients: &ClientMap,
) {
    let Some(rules) = rules else {
        return;
    };
    let live: Vec<SessionId> = lj.live_sessions().collect();
    let events = (rules.0.lock().unwrap_or_else(|e| e.into_inner())).after_tick(sim, &live);
    for (audience, event) in events {
        let record = Arc::new(vec![WireRecord::GameEvent(event)]);
        let targets = match audience {
            Audience::All => live.clone(),
            Audience::Session(session) => vec![session],
        };
        for session in targets {
            send_to(clients, session, Outbound::Water(record.clone()));
        }
    }
}
