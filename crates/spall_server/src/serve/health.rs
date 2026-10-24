//! Player health glue for the tick loop (ENG-223): bind a joining capsule to
//! its account, and send each live session its own health when it changes.
use super::*;

/// Gives a joining session its capsule at `spawn` and, when the connection
/// authenticated as an account, restores that account's remembered health.
pub(super) fn join_player(
    sim: &mut Simulation,
    session: SessionId,
    spawn: [f64; 3],
    player_id: Option<spall_protocol::PlayerId>,
) {
    let entity = sim.add_player(session_player_entity(session), spawn);
    if let Some(player_id) = player_id {
        sim.bind_player_principal(entity, player_id);
    }
}

/// Sends each live session its own player's health whenever it differs from
/// what that session was last sent (so once on going live). Health changes
/// only through creature actions, so the creature glue calls this each tick.
pub(super) fn publish_player_health(sim: &Simulation, lj: &mut LateJoin, clients: &ClientMap) {
    let live: Vec<SessionId> = lj.live_sessions().collect();
    for session in live {
        let Some(health) = sim.player_health(session_player_entity(session)) else {
            continue;
        };
        if lj.health_sent.insert(session.raw(), health) == Some(health) {
            continue;
        }
        let record = WireRecord::PlayerHealth(spall_protocol::PlayerHealth {
            health,
            max_health: spall_sim::PLAYER_MAX_HEALTH,
        });
        send_to(clients, session, Outbound::Water(Arc::new(vec![record])));
    }
}
