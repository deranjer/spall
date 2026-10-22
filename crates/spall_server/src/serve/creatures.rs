//! Creature glue for the tick loop: roster changes and motion frames.
use super::*;

/// Applies the director's despawns and spawns and announces them, then its
/// movement, then sends 20 Hz presentation frames to live sessions. An error
/// must stop the server.
pub(super) fn run_creatures(
    service: &crate::creature_service::CreatureService,
    sim: &mut Simulation,
    lj: &mut LateJoin,
    clients_for_sim: &ClientMap,
) -> Result<(), String> {
    match service.run_tick(sim) {
        Ok(roster) if roster.is_empty() => {}
        Ok(roster) => {
            tracing::info!(
                tick = sim.current_tick().get(),
                spawned = roster.spawned.len(),
                despawned = roster.despawned.len(),
                "creature roster changed"
            );
            let records = roster.into_wire_records();
            lj.fan_out_creature_records(records, clients_for_sim);
        }
        Err(error) => return Err(format!("creature roster update failed: {error}")),
    }
    service.run_motion(sim)?;
    // Presentation frames at 20 Hz, to live sessions only, limited to
    // creatures near that session's player.
    if sim.current_tick().get() % CREATURE_MOTION_PERIOD_TICKS == 0 {
        for session in lj.live_sessions().collect::<Vec<_>>() {
            if water_backlogged(clients_for_sim, session) {
                continue;
            }
            let center = sim
                .player_state(session_player_entity(session))
                .map(|player| player.position_m);
            let include = move |position: [f64; 3]| match center {
                Some(center) => {
                    let dx = position[0] - center[0];
                    let dz = position[2] - center[2];
                    dx * dx + dz * dz <= CREATURE_MOTION_RADIUS_M * CREATURE_MOTION_RADIUS_M
                }
                None => true,
            };
            if let Some(frame) = sim.creature_motion_frame(&include) {
                send_to(
                    clients_for_sim,
                    session,
                    Outbound::Water(Arc::new(vec![WireRecord::CreatureMotionFrame(frame)])),
                );
            }
        }
    }
    Ok(())
}
