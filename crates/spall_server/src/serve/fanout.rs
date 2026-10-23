use super::*;

/// T20: a [`BodyDigest`] per authoritative body **and** per player capsule,
/// keyed by entity id, for this motion batch. The body centre is its pose
/// origin and its interest radius is half the diagonal of the collider region
/// box — a conservative bound (the origin need not be the box centre), which
/// only ever keeps a body *more* relevant.
pub(super) fn build_body_digests(world: &SimWorld) -> HashMap<u64, BodyDigest> {
    let mut digests: HashMap<u64, BodyDigest> = HashMap::new();
    for body in world.bodies() {
        let Some(entity) = body.entity else { continue };
        let (lo, hi) = body.collider_region;
        let cs = body.cell_size().metres();
        let ex = (hi.x - lo.x + 1) as f64 * cs;
        let ey = (hi.y - lo.y + 1) as f64 * cs;
        let ez = (hi.z - lo.z + 1) as f64 * cs;
        digests.insert(
            entity.get(),
            BodyDigest {
                key: entity.get(),
                center_m: body.pose.translation_m,
                radius_m: 0.5 * (ex * ex + ey * ey + ez * ez).sqrt(),
                is_player: false,
                sleeping: body.sleeping,
            },
        );
    }
    for player in world.players() {
        digests.insert(
            player.entity.get(),
            BodyDigest {
                key: player.entity.get(),
                center_m: player.state.position_m,
                radius_m: PLAYER_INTEREST_RADIUS_M,
                is_player: true,
                sleeping: false,
            },
        );
    }
    digests
}

/// Run totals for the T20 motion path, accumulated across every batch and
/// client. All zero (and untouched) unless `motion_interest` is configured.
#[derive(Debug, Default)]
pub(super) struct MotionEgress {
    pub(super) snapshots_sent: u64,
    pub(super) interest_culled: u64,
    pub(super) budget_deferred: u64,
    pub(super) max_client_batch_bytes: u64,
}

/// T20: route one 20 Hz motion batch to each live client filtered by its
/// interest set and bandwidth budget. Committed topology is untouched — only
/// motion, which the next batch supersedes, is ever withheld here.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_motion_by_interest(
    mi: &MotionInterest,
    scene: Scene,
    batch_index: u64,
    snaps: &[MotionSnapshot],
    sim: &Simulation,
    lj: &LateJoin,
    clients: &ClientMap,
    client_repl: &mut HashMap<u64, ClientReplication>,
    egress: &mut MotionEgress,
) {
    let world = sim.world();
    let digests = build_body_digests(world);
    let budget = MotionBudget {
        far_interval: mi.far_interval,
        per_batch_bytes: mi.per_client_budget_bytes,
    };

    for session in lj.live_sessions() {
        // The client's interest anchor: its own player capsule on a player
        // scene, else the configured static anchor, else unfiltered.
        let own_entity = scene.has_players().then(|| session_player_entity(session));
        let own_player_key = own_entity.map(|e| e.get());
        let anchor = match own_entity {
            Some(entity) => world
                .players()
                .find(|p| p.entity == entity)
                .map(|p| p.state.position_m),
            None => mi.static_anchor_m,
        };
        let interest = match anchor {
            Some(center_m) => InterestSet::Anchored {
                center_m,
                near_radius_m: mi.near_radius_m,
                far_radius_m: mi.far_radius_m,
            },
            None => InterestSet::Global,
        };

        let repl = client_repl.entry(session.raw()).or_default();
        repl.set_interest(interest);
        let kept = repl.select(batch_index, own_player_key, snaps, &digests, &budget);
        let outcome = repl.last_outcome();
        egress.snapshots_sent += outcome.kept;
        egress.interest_culled += outcome.interest_culled;
        egress.budget_deferred += outcome.budget_deferred;
        egress.max_client_batch_bytes = egress.max_client_batch_bytes.max(outcome.bytes as u64);
        if !kept.is_empty() {
            send_to(clients, session, Outbound::Motion(Arc::new(kept)));
        }
    }
}

/// The playground's two timed debris emitters over `sim`'s dormant pools.
pub(super) fn playground_drop_schedules(
    sim: &Simulation,
) -> (spall_sim::DropSchedule, spall_sim::DropSchedule) {
    let pools = spall_sim::pending_drop_pools(sim.world());
    const SHOWCASE_INTERVAL_TICKS: u64 = 300; // 5 s at 60 Hz
    const PLINKO_INTERVAL_TICKS: u64 = 60; // 1 s at 60 Hz
    (
        spall_sim::DropSchedule::new(pools.showcase, SHOWCASE_INTERVAL_TICKS).at_height(7.0),
        spall_sim::DropSchedule::new(pools.plinko, PLINKO_INTERVAL_TICKS)
            .at_height(10.5)
            .with_restitution(spall_sim::PLINKO_RESTITUTION),
    )
}

/// Sends a single shared, supersedable motion batch to the supplied sessions.
/// The caller chooses those sessions from [`LateJoin::live_sessions`] so a
/// joining replica cannot consume link capacity before its baseline barrier.
pub(super) fn send_motion_to_sessions(
    clients: &ClientMap,
    recipients: impl IntoIterator<Item = SessionId>,
    batch: Arc<Vec<MotionSnapshot>>,
) {
    for session in recipients {
        send_to(clients, session, Outbound::Motion(Arc::clone(&batch)));
    }
}

/// Fans one message to every client. A client whose reliable backlog blew its
/// bound ([`OutboundOverflow`]) is dropped from the fan-out set here; its
/// writer task flushes the already-accepted backlog and then closes the
/// connection, so committed topology is never silently discarded.
pub(super) fn broadcast(clients: &ClientMap, msg: Outbound) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    let mut overflowed: Vec<u64> = Vec::new();
    for (id, handle) in guard.iter() {
        if handle.push(msg.clone()).is_err() {
            overflowed.push(*id);
        }
    }
    for id in overflowed {
        guard.remove(&id);
    }
}

/// Whether water for `session` should be skipped this tick (see
/// [`OutboundHandle::water_backlogged`]).
pub(super) fn water_backlogged(clients: &ClientMap, session: SessionId) -> bool {
    clients
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session.raw())
        .is_some_and(OutboundHandle::water_backlogged)
}

pub(super) fn send_to(clients: &ClientMap, session: SessionId, msg: Outbound) {
    let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(handle) = guard.get(&session.raw())
        && handle.push(msg).is_err()
    {
        guard.remove(&session.raw());
    }
}

pub(super) fn reject(clients: &ClientMap, session: SessionId, request: RequestId, reason: &str) {
    send_to(
        clients,
        session,
        Outbound::Status(Arc::new(ActionStatus {
            request_id: request,
            outcome: ActionOutcome::Rejected {
                reason: reason.to_string(),
            },
        })),
    );
}

pub(super) fn progression_rejected(
    request: &spall_protocol::ProgressionRequest,
    code: spall_protocol::ProgressionRejectCode,
) -> spall_protocol::ProgressionResponse {
    spall_protocol::ProgressionResponse {
        request_id: request.request_id,
        catalog_version: request.catalog_version.max(1),
        inventory_revision: request.expected_inventory_revision,
        outcome: spall_protocol::ProgressionOutcome::Rejected(code),
        inventory: Vec::new(),
        sites: Vec::new(),
    }
}
