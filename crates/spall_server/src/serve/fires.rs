//! Fires for the tick loop (ENG-266): setting a fire pit down from the pack
//! and feeding a fire with E. The fire itself is a world item with fuel
//! (`spall_sim::world_items`); burning, replication and saving are the world
//! item paths.
use super::*;
use spall_protocol::WorldItem;

/// The game's side of fires. Both run on the authoritative thread between
/// ticks, so they must be quick, and both either take from the player's pack
/// and answer `Ok`, or refuse with a short reason and change nothing.
pub trait FireRules: Send {
    /// Takes one `kind` from the pack to set down as a fire and returns its
    /// starting fuel, seconds (above zero: it is lit). Refuses a kind that is
    /// not a fire or is not in the pack.
    fn place(
        &mut self,
        session: SessionId,
        player: Option<spall_protocol::PlayerId>,
        kind: u16,
    ) -> Result<f32, &'static str>;

    /// Takes fuel from the pack for `fire` (lit or not) and returns the
    /// seconds it adds, above zero. Refuses with no fuel in the pack, or a
    /// fire the game holds full.
    fn refuel(
        &mut self,
        session: SessionId,
        player: Option<spall_protocol::PlayerId>,
        fire: &WorldItem,
    ) -> Result<f32, &'static str>;
}

/// Cloneable handle to the game's [`FireRules`], so `ServeConfig` keeps its
/// derives. Without one nothing can be set down or fed.
#[derive(Clone)]
pub struct Fires(Arc<Mutex<Box<dyn FireRules>>>);

impl Fires {
    pub fn new(rules: impl FireRules + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(rules))))
    }

    fn rules(&self) -> std::sync::MutexGuard<'_, Box<dyn FireRules>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for Fires {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fires")
    }
}

fn status(req: &ActionRequest, outcome: ActionOutcome) -> ActionStatus {
    ActionStatus {
        request_id: req.request_id,
        outcome,
    }
}

fn rejected(req: &ActionRequest, reason: &str) -> ActionStatus {
    status(
        req,
        ActionOutcome::Rejected {
            reason: reason.to_string(),
        },
    )
}

/// A success goes in the admission ledger, so a retry of the same request id
/// is answered from there and never sets down or feeds twice.
fn queued(sim: &mut Simulation, req: &ActionRequest) -> ActionStatus {
    let queued = status(req, ActionOutcome::Queued);
    sim.record_action_status(queued.clone());
    queued
}

/// The session's authoritative eye and account, if it has a player.
fn player_eye(
    sim: &Simulation,
    session: SessionId,
) -> Option<([f64; 3], Option<spall_protocol::PlayerId>)> {
    let player = sim.world().player(session_player_entity(session))?;
    let mut eye = player.state.position_m;
    eye[1] += f64::from(player.params.total_height_m()) * 0.9;
    Some((eye, player.principal))
}

/// A `PlaceItem` request sets the pack item `req.tool` down as a fire where
/// [`spall_sim::world_items::fire_spot`] finds open ground from the server's
/// own player eye along the aim direction (the claimed origin is ignored).
/// `None` for any other action. The spot is checked before the pack is
/// touched, so a refused spot costs nothing.
pub(super) fn place_fire(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    fires: &Fires,
) -> Option<ActionStatus> {
    if req.action != ActionKind::PlaceItem {
        return None;
    }
    let Some((eye, principal)) = player_eye(sim, session) else {
        return Some(rejected(req, "placing requires a player"));
    };
    let Some(spot) =
        spall_sim::world_items::fire_spot(&sim.world().terrain().volume, eye, req.aim_dir)
    else {
        return Some(rejected(req, "no open ground in reach to set that down"));
    };
    let fuel_s = match fires.rules().place(session, principal, req.tool) {
        Ok(fuel_s) if fuel_s.is_finite() && fuel_s >= 0.0 => fuel_s,
        Ok(_) => return Some(rejected(req, "the game gave the fire no usable fuel")),
        Err(reason) => return Some(rejected(req, reason)),
    };
    // Facing the player.
    let yaw = (eye[0] - spot[0]).atan2(eye[2] - spot[2]) as f32;
    let fire = sim
        .place_fire(req.tool, spot, yaw, fuel_s)
        .expect("a fire at a checked spot with checked fuel is valid");
    tracing::info!(
        session = session.raw(),
        item = fire.item_id,
        kind = fire.kind,
        fuel_s,
        "fire set down"
    );
    Some(queued(sim, req))
}

/// An `Interact` whose nearest visible item (the pickup rule) is a fire feeds
/// it from the pack instead of picking it up: fires stay where they were set.
/// `None` when the request is not an interaction or the nearest item is not
/// a fire, so the caller carries on to pickup and doors.
pub(super) fn feed_fire(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    fires: Option<&Fires>,
) -> Option<ActionStatus> {
    if req.action != ActionKind::Interact {
        return None;
    }
    let (eye, principal) = player_eye(sim, session)?;
    let fire = sim.nearest_world_item(eye, spall_sim::world_items::ITEM_PICKUP_REACH_M)?;
    fire.fire_fuel_s?;
    let Some(fires) = fires else {
        return Some(rejected(req, "fires cannot be fed here"));
    };
    let fuel_s = match fires.rules().refuel(session, principal, &fire) {
        Ok(fuel_s) => fuel_s,
        Err(reason) => return Some(rejected(req, reason)),
    };
    let Some(fed) = sim.add_fire_fuel(fire.item_id, fuel_s) else {
        tracing::error!(
            item = fire.item_id,
            fuel_s,
            "the game took fuel from the pack but returned an unusable amount"
        );
        return Some(rejected(req, "that fuel would not burn"));
    };
    tracing::info!(
        session = session.raw(),
        item = fed.item_id,
        fuel_s,
        "fire fed"
    );
    Some(queued(sim, req))
}

/// [`place_fire`] and [`feed_fire`] for the tick loop: answers the client and
/// returns `true` when the request was handled here. A `PlaceItem` with no
/// game fire rules is refused rather than reaching the edit path.
pub(super) fn try_fire(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    fires: &Option<Fires>,
    clients: &ClientMap,
    client_request_id: RequestId,
) -> bool {
    let answer = match fires {
        Some(fires) => place_fire(sim, session, req, fires),
        None => (req.action == ActionKind::PlaceItem)
            .then(|| rejected(req, "nothing can be set down here")),
    }
    .or_else(|| feed_fire(sim, session, req, fires.as_ref()));
    let Some(mut status) = answer else {
        return false;
    };
    status.request_id = client_request_id;
    send_to(clients, session, Outbound::Status(Arc::new(status)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIT: u16 = 6;
    const STICK: u16 = 5;

    /// A pack with `pits` fire pits and `sticks` sticks; a pit starts with
    /// 60 s of fuel and a stick adds 180 s.
    struct Pack {
        pits: u32,
        sticks: u32,
    }

    impl FireRules for Arc<Mutex<Pack>> {
        fn place(
            &mut self,
            _: SessionId,
            _: Option<spall_protocol::PlayerId>,
            kind: u16,
        ) -> Result<f32, &'static str> {
            let mut pack = self.lock().unwrap();
            if kind != PIT || pack.pits == 0 {
                return Err("no fire pit in your pack");
            }
            pack.pits -= 1;
            Ok(60.0)
        }

        fn refuel(
            &mut self,
            _: SessionId,
            _: Option<spall_protocol::PlayerId>,
            _: &WorldItem,
        ) -> Result<f32, &'static str> {
            let mut pack = self.lock().unwrap();
            if pack.sticks == 0 {
                return Err("no fuel in your pack");
            }
            pack.sticks -= 1;
            Ok(180.0)
        }
    }

    fn request(id: u64, action: ActionKind, tool: u16, aim_dir: [f32; 3]) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(id),
            input_seq: spall_protocol::InputSeq(id),
            action,
            tool,
            // A claimed origin far away; the server uses its own player.
            aim_origin_m: [500.0, 500.0, 500.0],
            aim_dir,
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: SphereBrush::new(BrushPoint::from_units(0, 0, 0), 0).unwrap(),
        }
    }

    /// The walk arena (floor top at 1 m) with a player at `feet`.
    fn setup(
        feet: [f64; 3],
        pits: u32,
        sticks: u32,
    ) -> (Simulation, SessionId, Fires, Arc<Mutex<Pack>>) {
        let mut sim = Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::walk_arena_setup(),
        ))
        .unwrap();
        let session = SessionId::from_parts(SlotId(0), 1);
        sim.add_player(session_player_entity(session), feet);
        let pack = Arc::new(Mutex::new(Pack { pits, sticks }));
        (sim, session, Fires::new(pack.clone()), pack)
    }

    const DOWN_AHEAD: [f32; 3] = [1.0, -1.0, 0.0];

    #[test]
    fn a_pit_is_set_down_lit_in_front_of_the_player_once() {
        let (mut sim, session, fires, pack) = setup([2.0, 1.0, 2.0], 2, 0);
        let req = request(1, ActionKind::PlaceItem, PIT, DOWN_AHEAD);
        let placed = place_fire(&mut sim, session, &req, &fires).unwrap();
        assert_eq!(placed.outcome, ActionOutcome::Queued);
        let fire = *sim.world_items().next().expect("a fire");
        assert_eq!(fire.kind, PIT);
        assert_eq!(fire.fire_fuel_s, Some(60.0));
        assert!(fire.is_lit());
        assert_eq!(fire.translation_m[1], 1.0, "on the floor");
        assert!(fire.translation_m[0] > 2.0, "in front: {fire:?}");
        assert_eq!(pack.lock().unwrap().pits, 1);
        // A retry is answered from the ledger the tick loop checks first.
        assert_eq!(replay_admitted_status(&sim, RequestId(1)), Some(placed));
        // Other actions are not placements.
        let cut = request(2, ActionKind::Cut, PIT, DOWN_AHEAD);
        assert_eq!(place_fire(&mut sim, session, &cut, &fires), None);
    }

    #[test]
    fn a_pit_is_refused_out_of_reach_or_without_one_in_the_pack() {
        // Looking straight up: no ground to set it on, and the pack is untouched.
        let (mut sim, session, fires, pack) = setup([2.0, 1.0, 2.0], 1, 0);
        let up = request(1, ActionKind::PlaceItem, PIT, [0.0, 1.0, 0.0]);
        let status = place_fire(&mut sim, session, &up, &fires).unwrap();
        assert!(matches!(status.outcome, ActionOutcome::Rejected { .. }));
        assert_eq!(pack.lock().unwrap().pits, 1);
        assert_eq!(sim.world_items().count(), 0);
        // A kind the game does not set down as a fire.
        let stick = request(2, ActionKind::PlaceItem, STICK, DOWN_AHEAD);
        let status = place_fire(&mut sim, session, &stick, &fires).unwrap();
        assert_eq!(
            status.outcome,
            ActionOutcome::Rejected {
                reason: "no fire pit in your pack".into()
            }
        );
        assert_eq!(sim.world_items().count(), 0);
        // A refusal reserves nothing.
        assert_eq!(replay_admitted_status(&sim, RequestId(2)), None);
    }

    #[test]
    fn e_feeds_a_nearby_fire_from_the_pack_and_relights_it() {
        let (mut sim, session, fires, pack) = setup([2.0, 1.0, 2.0], 1, 1);
        let place = request(1, ActionKind::PlaceItem, PIT, DOWN_AHEAD);
        place_fire(&mut sim, session, &place, &fires).unwrap();
        // Burn it out.
        for _ in 0..(61 * 60) {
            sim.tick().unwrap();
            if !sim.world_items().next().unwrap().is_lit() {
                break;
            }
        }
        let out = *sim.world_items().next().unwrap();
        assert_eq!(out.fire_fuel_s, Some(0.0), "out, and the pit stays");
        let fed = feed_fire(
            &mut sim,
            session,
            &request(2, ActionKind::Interact, 0, DOWN_AHEAD),
            Some(&fires),
        )
        .unwrap();
        assert_eq!(fed.outcome, ActionOutcome::Queued);
        let lit = *sim.world_items().next().unwrap();
        assert!(lit.is_lit());
        assert_eq!(lit.fire_fuel_s, Some(180.0));
        assert_eq!(pack.lock().unwrap().sticks, 0);
        // No more fuel: refused, the fire unchanged and still not picked up.
        let refused = feed_fire(
            &mut sim,
            session,
            &request(3, ActionKind::Interact, 0, DOWN_AHEAD),
            Some(&fires),
        )
        .unwrap();
        assert!(matches!(refused.outcome, ActionOutcome::Rejected { .. }));
        assert_eq!(sim.world_items().next().unwrap().fire_fuel_s, Some(180.0));
    }

    #[test]
    fn e_beside_an_ordinary_item_is_left_to_pickup() {
        let (mut sim, session, fires, _) = setup([2.0, 1.0, 2.0], 0, 1);
        sim.spawn_world_item(40, [3.0, 1.0, 2.0], 0.0).unwrap();
        let interact = request(1, ActionKind::Interact, 0, DOWN_AHEAD);
        assert_eq!(feed_fire(&mut sim, session, &interact, Some(&fires)), None);
    }
}
