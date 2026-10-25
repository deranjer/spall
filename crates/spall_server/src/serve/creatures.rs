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
    // Creatures one killed this tick leave every client like any despawn.
    let killed = service.run_motion(sim)?;
    if !killed.is_empty() {
        lj.fan_out_creature_records(killed.into_wire_records(), clients_for_sim);
    }
    // Presentation frames at 20 Hz, to live sessions only, limited to
    // creatures near that session's player.
    if sim
        .current_tick()
        .get()
        .is_multiple_of(CREATURE_MOTION_PERIOD_TICKS)
    {
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

/// ENG-237: a cutting tool whose aim meets a creature before any geometry
/// kills it. The despawn is announced to every client and the carcass is
/// queued, its first cell under the strike's request id. `None` when the
/// request did not strike a creature.
pub(super) fn strike_creature(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    catalog: &ToolCatalog,
    lj: &mut LateJoin,
    clients_for_sim: &ClientMap,
) -> Option<Result<ActionStatus, ActionReject>> {
    let creature_id = creature_strike(sim, req, catalog)?;
    let kill = match sim.kill_creature(creature_id, Some(req.request_id), actor_for(session)) {
        Ok(kill) => kill,
        Err(_) => {
            return Some(Err(ActionReject::Unsupported(
                "overloaded: no room to queue the carcass, retry",
            )));
        }
    };
    tracing::info!(
        creature = creature_id,
        species = %kill.record.species,
        carcass_cells = kill.carcass.len(),
        "creature killed"
    );
    lj.fan_out_creature_records(
        vec![WireRecord::CreatureDespawn(
            spall_protocol::CreatureDespawn {
                schema: spall_protocol::WIRE_SCHEMA_VERSION,
                creature_id,
            },
        )],
        clients_for_sim,
    );
    Some(kill.first_status.ok_or(ActionReject::Unsupported(
        "the creature died but left no room for a carcass",
    )))
}
