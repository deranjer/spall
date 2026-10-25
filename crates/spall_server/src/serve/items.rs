//! World item glue for the tick loop (ENG-257): E-key pickup into the game's
//! pack, and replication of item spawns and removals.
use super::*;
use spall_protocol::WorldItem;

/// The game's pack: given the picking session, its authenticated account (if
/// any) and the item, either puts the item in that player's inventory and
/// returns `Ok`, or refuses with a short reason (a full pack) and changes
/// nothing. It runs on the authoritative thread between ticks, so it must be
/// quick.
pub type ItemPickupFn = dyn FnMut(SessionId, Option<spall_protocol::PlayerId>, &WorldItem) -> Result<(), &'static str>
    + Send;

/// Cloneable handle to the game's [`ItemPickupFn`], so `ServeConfig` keeps its
/// derives. Without one, the server has no pack and nothing can be picked up.
#[derive(Clone)]
pub struct ItemPickup(Arc<Mutex<Box<ItemPickupFn>>>);

impl ItemPickup {
    pub fn new(
        grant: impl FnMut(
            SessionId,
            Option<spall_protocol::PlayerId>,
            &WorldItem,
        ) -> Result<(), &'static str>
        + Send
        + 'static,
    ) -> Self {
        Self(Arc::new(Mutex::new(Box::new(grant))))
    }

    fn grant(
        &self,
        session: SessionId,
        player: Option<spall_protocol::PlayerId>,
        item: &WorldItem,
    ) -> Result<(), &'static str> {
        (self.0.lock().unwrap_or_else(|e| e.into_inner()))(session, player, item)
    }
}

impl std::fmt::Debug for ItemPickup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ItemPickup")
    }
}

/// An `Interact` request picks up the nearest visible item within
/// [`spall_sim::world_items::ITEM_PICKUP_REACH_M`] of the session's
/// authoritative eye; the client's aim is ignored. `None` when the request is
/// not a pickup (another action, no player, no item in reach), so the caller
/// carries on (a nearby door). Otherwise the answer: `Queued` once the item is
/// in the pack and gone from the world, or a refusal with the item left where
/// it was: the game's (a full pack), or "buried under snow" when more than
/// [`spall_sim::world_items::ITEM_BURIED_SNOW_M`] of thin snow lies on it.
/// Requests are handled one at a time on the owning
/// thread, so an item goes to exactly one player. A pickup's `Queued` goes in
/// the simulation's admission ledger, the one door and edit retries replay
/// from, so a retry of the same request id is answered from there and never
/// reaches a door the item was lying beside.
pub(super) fn pick_up_item(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    pickup: &ItemPickup,
) -> Option<ActionStatus> {
    if req.action != ActionKind::Interact {
        return None;
    }
    let player = sim.world().player(session_player_entity(session))?;
    let principal = player.principal;
    let mut eye = player.state.position_m;
    eye[1] += f64::from(player.params.total_height_m()) * 0.9;
    let item = sim.nearest_world_item(eye, spall_sim::world_items::ITEM_PICKUP_REACH_M)?;
    let status = |outcome| ActionStatus {
        request_id: req.request_id,
        outcome,
    };
    if sim.snow_depth_m_at(item.translation_m) > spall_sim::world_items::ITEM_BURIED_SNOW_M {
        return Some(status(ActionOutcome::Rejected {
            reason: "buried under snow".into(),
        }));
    }
    Some(match pickup.grant(session, principal, &item) {
        Ok(()) => {
            sim.remove_world_item(item.item_id)
                .expect("the item was found this call");
            tracing::info!(
                session = session.raw(),
                item = item.item_id,
                kind = item.kind,
                "world item picked up"
            );
            let queued = status(ActionOutcome::Queued);
            sim.record_action_status(queued.clone());
            queued
        }
        Err(reason) => status(ActionOutcome::Rejected {
            reason: reason.to_string(),
        }),
    })
}

/// [`pick_up_item`] for the tick loop: answers the client and returns `true`
/// when the request was a pickup.
pub(super) fn try_pick_up(
    sim: &mut Simulation,
    session: SessionId,
    req: &ActionRequest,
    pickup: &Option<ItemPickup>,
    clients: &ClientMap,
) -> bool {
    let Some(status) = pickup
        .as_ref()
        .and_then(|pickup| pick_up_item(sim, session, req, pickup))
    else {
        return false;
    };
    send_to(clients, session, Outbound::Status(Arc::new(status)));
    true
}

/// Announces this tick's item spawns and removals: live clients now, joining
/// clients after their baseline.
pub(super) fn publish_world_items(sim: &mut Simulation, lj: &mut LateJoin, clients: &ClientMap) {
    let updates = sim.take_world_item_changes();
    if !updates.is_empty() {
        let records = updates.into_iter().map(WireRecord::WorldItemUpdate);
        lj.fan_out_creature_records(records.collect(), clients);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOVEL: u16 = 40;

    fn request(id: u64, action: ActionKind) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(id),
            input_seq: spall_protocol::InputSeq(id),
            action,
            tool: 5,
            // Claimed aim points straight at the item; the server ignores it.
            aim_origin_m: [3.0, 1.6, 3.0],
            aim_dir: [0.0, -1.0, 0.0],
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: SphereBrush::new(BrushPoint::from_units(0, 0, 0), 0).unwrap(),
        }
    }

    /// Who got which item: (slot, item id).
    type Granted = Arc<Mutex<Vec<(u32, u64)>>>;

    /// A walk arena with one shovel on the floor at (3, y, 3) and a pack that
    /// records who got what and refuses `refuse_slot`.
    fn setup(refuse_slot: Option<u32>) -> (Simulation, WorldItem, ItemPickup, Granted) {
        let mut sim = Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::walk_arena_setup(),
        ))
        .unwrap();
        let floor = walk_floor_m(&sim);
        let item = sim
            .spawn_world_item(SHOVEL, [3.0, floor, 3.0], 0.0)
            .unwrap();
        let granted = Arc::new(Mutex::new(Vec::new()));
        let log = granted.clone();
        let pickup = ItemPickup::new(move |session, _, item| {
            if Some(session.slot().0) == refuse_slot {
                return Err("your pack is full");
            }
            log.lock().unwrap().push((session.slot().0, item.item_id));
            Ok(())
        });
        (sim, item, pickup, granted)
    }

    /// Top of the walk arena's floor, metres.
    fn walk_floor_m(sim: &Simulation) -> f64 {
        let volume = &sim.world().terrain().volume;
        let cell = (0..64)
            .rev()
            .find(|&y| {
                matches!(
                    volume.sample(GlobalCell::new(12, y, 12)),
                    Ok(spall_voxel::Sample::Filled(_))
                )
            })
            .expect("arena has a floor");
        (cell + 1) as f64 * 0.25
    }

    fn add_player(sim: &mut Simulation, slot: u32, feet: [f64; 3]) -> SessionId {
        let session = SessionId::from_parts(SlotId(slot), 1);
        sim.add_player(session_player_entity(session), feet);
        session
    }

    #[test]
    fn pickup_uses_the_authoritative_player_not_claimed_aim() {
        let (mut sim, item, pickup, granted) = setup(None);
        let floor = item.translation_m[1];
        // Far from the item: the claimed aim right above it does not help.
        let far = add_player(&mut sim, 0, [3.0, floor, 9.0]);
        assert_eq!(
            pick_up_item(&mut sim, far, &request(1, ActionKind::Interact), &pickup),
            None,
            "out of reach"
        );
        assert!(sim.world_items().any(|i| i.item_id == item.item_id));
        let near = add_player(&mut sim, 1, [3.0, floor, 4.0]);
        // Only Interact picks up.
        assert_eq!(
            pick_up_item(&mut sim, near, &request(2, ActionKind::Cut), &pickup),
            None
        );
        let status = pick_up_item(&mut sim, near, &request(3, ActionKind::Interact), &pickup)
            .expect("in reach");
        assert_eq!(status.outcome, ActionOutcome::Queued);
        // Recorded where the tick loop looks for retries before a door.
        assert_eq!(replay_admitted_status(&sim, RequestId(3)), Some(status));
        assert_eq!(*granted.lock().unwrap(), vec![(1, item.item_id)]);
        assert!(!sim.world_items().any(|i| i.item_id == item.item_id));
        let changes = sim.take_world_item_changes();
        // Spawned and taken before any announcement: nothing to say.
        assert!(
            changes
                .iter()
                .all(|u| u.spawned.is_empty() && u.removed.is_empty())
        );
    }

    #[test]
    fn two_players_reaching_for_one_item_one_gets_it() {
        let (mut sim, item, pickup, granted) = setup(None);
        let floor = item.translation_m[1];
        sim.take_world_item_changes();
        let a = add_player(&mut sim, 0, [3.0, floor, 4.0]);
        let b = add_player(&mut sim, 1, [4.0, floor, 3.0]);
        // Same tick, same item, plus a duplicate of the winner's request.
        let first = pick_up_item(&mut sim, a, &request(7, ActionKind::Interact), &pickup);
        let second = pick_up_item(&mut sim, b, &request(7, ActionKind::Interact), &pickup);
        let again = pick_up_item(&mut sim, a, &request(7, ActionKind::Interact), &pickup);
        assert_eq!(first.unwrap().outcome, ActionOutcome::Queued);
        assert_eq!((second, again), (None, None), "nothing left in reach");
        assert_eq!(*granted.lock().unwrap(), vec![(0, item.item_id)]);
        let changes = sim.take_world_item_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].removed, vec![item.item_id]);
    }

    #[test]
    fn a_full_pack_refuses_and_the_item_stays() {
        let (mut sim, item, pickup, granted) = setup(Some(0));
        let floor = item.translation_m[1];
        let full = add_player(&mut sim, 0, [3.0, floor, 4.0]);
        let status = pick_up_item(&mut sim, full, &request(1, ActionKind::Interact), &pickup)
            .expect("in reach");
        assert_eq!(
            status.outcome,
            ActionOutcome::Rejected {
                reason: "your pack is full".into()
            }
        );
        assert!(granted.lock().unwrap().is_empty());
        assert_eq!(sim.world_items().collect::<Vec<_>>(), vec![&item]);
        // A refusal reserves nothing: the item is still there to refuse again.
        assert_eq!(replay_admitted_status(&sim, RequestId(1)), None);
    }
}
