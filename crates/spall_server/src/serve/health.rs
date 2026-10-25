//! Player health glue for the tick loop (ENG-223): bind a joining capsule to
//! its account, and send each live session its own health, warmth and
//! wetness (ENG-264/265) when they change.
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

/// The client's view of `player`'s body.
fn record(player: &spall_sim::Player) -> spall_protocol::PlayerHealth {
    let q8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    spall_protocol::PlayerHealth {
        health: player.health,
        max_health: spall_sim::PLAYER_MAX_HEALTH,
        warmth_q8: q8(player.body.warmth),
        wetness_q8: q8(player.body.wetness),
    }
}

/// Sends each live session its own player's health record whenever it
/// differs from what that session was last sent (so once on going live).
/// Warmth and wetness change at most once a second, so this costs at most one
/// small record per player per second. Called every tick.
pub(super) fn publish_player_health(sim: &Simulation, lj: &mut LateJoin, clients: &ClientMap) {
    let live: Vec<SessionId> = lj.live_sessions().collect();
    for session in live {
        let Some(player) = sim.world().player(session_player_entity(session)) else {
            continue;
        };
        let health = record(player);
        if lj.health_sent.insert(session.raw(), health) == Some(health) {
            continue;
        }
        let record = WireRecord::PlayerHealth(health);
        send_to(clients, session, Outbound::Water(Arc::new(vec![record])));
    }
}
