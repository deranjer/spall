use super::*;

/// A stable per-session actor id. Authority is the server's; this is only
/// journal provenance.
pub(super) fn actor_for(session: SessionId) -> EntityId {
    EntityId::new(1 + u64::from(session.slot().0)).unwrap_or(EntityId::new(1).unwrap())
}

/// **Development-scenario path** (`ServeConfig::dev_unvalidated_actions`). Trusts
/// `claimed_target` / `claimed_brush` verbatim so a fixture harness can script
/// arbitrary cuts. `None` only for a genuinely unrepresentable target.
pub(super) fn dev_intent_from_request(
    session: SessionId,
    req: &ActionRequest,
) -> Option<EditIntent> {
    let target = match req.claimed_target {
        ClaimedTarget::Terrain => EditTarget::Terrain,
        ClaimedTarget::Body(entity) => EditTarget::Body(entity),
    };
    let kind = match req.action {
        ActionKind::Cut => EditKind::Cut,
        ActionKind::Place => EditKind::Place(spall_voxel::fixtures::STONE),
    };
    Some(EditIntent {
        request_id: req.request_id,
        actor: actor_for(session),
        target,
        kind,
        brush: req.claimed_brush,
        explosion: None,
    })
}

// --- ENG-47: server-side action-claim validation -------------------------------

/// Why an [`ActionRequest`] was refused before it could become an [`EditIntent`].
/// The `&'static str` variants keep the `ActionStatus` reason strings stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionReject {
    UnknownTool(u16),
    ToolActionMismatch,
    RadiusTooLarge {
        requested_cells: i64,
        max_cells: i64,
    },
    BadAim,
    NoAuthoritativeHit,
    TargetMismatch,
    StaleTarget,
    Unsupported(&'static str),
}

impl ActionReject {
    pub(super) fn reason(&self) -> String {
        match self {
            ActionReject::UnknownTool(t) => format!("tool {t} is not an approved tool"),
            ActionReject::ToolActionMismatch => {
                "requested action is not permitted for this tool".to_string()
            }
            ActionReject::RadiusTooLarge {
                requested_cells,
                max_cells,
            } => format!(
                "brush radius {requested_cells} cells exceeds this tool's limit of {max_cells}"
            ),
            ActionReject::BadAim => "aim origin/direction is not a usable ray".to_string(),
            ActionReject::NoAuthoritativeHit => {
                "aim does not hit authoritative geometry within reach".to_string()
            }
            ActionReject::TargetMismatch => {
                "claimed target does not match the authoritative hit".to_string()
            }
            ActionReject::StaleTarget => "claimed target no longer exists".to_string(),
            ActionReject::Unsupported(s) => s.to_string(),
        }
    }
}

/// One authoritative ray hit, resolved against committed geometry.
pub(super) struct Struck {
    pub(super) target: EditTarget,
    /// Struck solid cell, in the hit volume's **local cell space**.
    pub(super) cell: GlobalCell,
    /// Empty cell against the struck face (where a placement lands), same space.
    pub(super) placement_cell: GlobalCell,
    /// Distance from the aim origin, metres.
    pub(super) t_m: f64,
}

/// The nearest solid cell an aim ray meets across the terrain and every detached
/// body, or `None` if nothing solid is within `reach_m`. This is the server's
/// authoritative hit: a client-supplied hit point / body id is only a claim
/// (`docs/architecture.md`: "The server determines the hit against its current
/// authoritative geometry").
pub(super) fn nearest_hit(world: &SimWorld, ray: Ray, reach_m: f64) -> Option<Struck> {
    let mut best: Option<Struck> = None;
    let mut consider = |target: EditTarget, body: &Body| {
        let xform = body.pose.xform(body.cell_size());
        if let Ok(RayOutcome::Hit(hit)) = cast_ray_world(&body.volume, &xform, ray, reach_m)
            && hit.t <= reach_m
            && best.as_ref().is_none_or(|b| hit.t < b.t_m)
        {
            best = Some(Struck {
                target,
                cell: hit.cell,
                placement_cell: hit.placement_cell,
                t_m: hit.t,
            });
        }
    };
    consider(EditTarget::Terrain, world.terrain());
    for body in world.bodies() {
        if let Some(entity) = body.entity {
            consider(EditTarget::Body(entity), body);
        }
    }
    best
}

pub(super) fn target_matches(claimed: ClaimedTarget, struck: EditTarget) -> bool {
    match (claimed, struck) {
        (ClaimedTarget::Terrain, EditTarget::Terrain) => true,
        (ClaimedTarget::Body(a), EditTarget::Body(b)) => a == b,
        _ => false,
    }
}

/// Validates an `ActionRequest`'s claims against the authoritative world and
/// turns it into an [`EditIntent`] whose brush is re-derived in the accepted
/// target frame from the server's own raycast — never copied from the client.
///
/// Checks, in order: the tool id is approved; the requested action fits the
/// tool; the requested radius is within the tool's cap; a named body still
/// exists; the aim ray hits solid authoritative geometry within reach; and the
/// struck volume matches `claimed_target`. The `session` is already known-live
/// (the caller rejects a superseded generation first).
#[cfg(test)]
pub(super) fn resolve_intent(
    world: &SimWorld,
    session: SessionId,
    req: &ActionRequest,
) -> Result<EditIntent, ActionReject> {
    resolve_intent_with_catalog(world, session, req, &ToolCatalog::default())
}

pub(super) fn resolve_intent_with_catalog(
    world: &SimWorld,
    session: SessionId,
    req: &ActionRequest,
    catalog: &ToolCatalog,
) -> Result<EditIntent, ActionReject> {
    let spec = catalog
        .get(req.tool)
        .ok_or(ActionReject::UnknownTool(req.tool))?;

    let action_fits = matches!(
        (req.action, spec.kind),
        (ActionKind::Cut, EditKind::Cut) | (ActionKind::Place, EditKind::Place(_))
    );
    if !action_fits {
        return Err(ActionReject::ToolActionMismatch);
    }
    if let EditKind::Place(material) = spec.kind
        && world.materials().get(material).is_none()
    {
        return Err(ActionReject::Unsupported(
            "tool material is absent from the authoritative world manifest",
        ));
    }

    let max_units = spec.max_radius_cells.saturating_mul(BRUSH_UNIT);
    let requested_units = req.claimed_brush.radius_units();
    if requested_units > max_units {
        return Err(ActionReject::RadiusTooLarge {
            requested_cells: requested_units / BRUSH_UNIT,
            max_cells: spec.max_radius_cells,
        });
    }

    // A named body must still be part of the authoritative world.
    if let ClaimedTarget::Body(entity) = req.claimed_target
        && world.body(entity).is_none()
    {
        return Err(ActionReject::StaleTarget);
    }

    let origin = DVec3::from_array(req.aim_origin_m);
    let dir = DVec3::new(
        req.aim_dir[0] as f64,
        req.aim_dir[1] as f64,
        req.aim_dir[2] as f64,
    );
    if !origin.is_finite() || !dir.is_finite() || dir.length_squared() < 1e-24 {
        return Err(ActionReject::BadAim);
    }

    let hit = nearest_hit(world, Ray::new(origin, dir), spec.reach_m)
        .ok_or(ActionReject::NoAuthoritativeHit)?;
    if !target_matches(req.claimed_target, hit.target) {
        return Err(ActionReject::TargetMismatch);
    }

    // Quantize the approved operation into the accepted target volume's local
    // integer cell space, centred on the server's own hit cell (the empty cell
    // against the struck face for a placement). The client's claimed centre is
    // discarded; its radius is kept only after passing the tool cap above.
    let base = match spec.kind {
        EditKind::Cut => hit.cell,
        EditKind::Place(_) => hit.placement_cell,
    };
    let h = BRUSH_UNIT / 2;
    let centre = BrushPoint::from_units(
        base.x.saturating_mul(BRUSH_UNIT).saturating_add(h),
        base.y.saturating_mul(BRUSH_UNIT).saturating_add(h),
        base.z.saturating_mul(BRUSH_UNIT).saturating_add(h),
    );
    let brush = SphereBrush::new(centre, requested_units.max(0)).map_err(|_| {
        ActionReject::RadiusTooLarge {
            requested_cells: requested_units / BRUSH_UNIT,
            max_cells: spec.max_radius_cells,
        }
    })?;

    Ok(EditIntent {
        request_id: req.request_id,
        actor: actor_for(session),
        target: hit.target,
        kind: spec.kind,
        brush,
        explosion: None,
    })
}

/// Per-tick admission counter, shared by the action and repair intake. Returns
/// `true` while `session` is still under `cap` for this tick and bumps its
/// count; `false` once the quota is spent. Each session's count is independent,
/// so one client's flood never consumes another's quota (ENG-48 fair
/// scheduling).
pub(super) fn admit(counts: &mut HashMap<u64, u32>, session: u64, cap: u32) -> bool {
    let n = counts.entry(session).or_insert(0);
    if *n >= cap {
        return false;
    }
    *n += 1;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ENG-47: action-claim validation --------------------------------------

    use spall_protocol::InputSeq;

    /// An `ActionRequest` with a controllable aim, tool, claimed target, and
    /// claimed brush. The brush centre is deliberately somewhere unrelated to
    /// the aim so a passing test proves the server re-derived it.
    fn action_req(
        tool: u16,
        action: ActionKind,
        origin_m: [f64; 3],
        dir: [f32; 3],
        claimed_target: ClaimedTarget,
        claimed_centre_cell: [i64; 3],
        claimed_radius_cells: i64,
    ) -> ActionRequest {
        let h = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                claimed_centre_cell[0] * BRUSH_UNIT + h,
                claimed_centre_cell[1] * BRUSH_UNIT + h,
                claimed_centre_cell[2] * BRUSH_UNIT + h,
            ),
            claimed_radius_cells * BRUSH_UNIT,
        )
        .expect("valid test brush");
        ActionRequest {
            request_id: RequestId(1),
            input_seq: InputSeq(1),
            action,
            tool,
            aim_origin_m: origin_m,
            aim_dir: dir,
            claimed_target,
            claimed_brush: brush,
        }
    }

    /// A `+X` aim from local cell `(0, 4, 1)` — inside the bridge scene's
    /// resident brick, above the floor — that travels to the column at local
    /// cell `(10, 4, 1)`. (An unbounded volume samples cells outside its
    /// resident bricks as `Unknown`, so the ray must *start* inside one.)
    const COLUMN_AIM_ORIGIN: [f64; 3] = [0.0, 1.0, 0.375];
    const PLUS_X: [f32; 3] = [1.0, 0.0, 0.0];

    #[test]
    fn a_valid_aim_resolves_to_a_server_derived_brush_on_the_struck_cell() {
        let sim = Scene::BridgeCut.simulation();
        // Claimed centre `(3, 1, 1)` is nowhere near the aim; the resolved brush
        // must sit on the server's hit cell `(10, 4, 1)` instead.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        let intent = resolve_intent(sim.world(), sess(0, 1), &req).expect("resolves");
        assert_eq!(intent.target, EditTarget::Terrain);
        assert_eq!(intent.kind, EditKind::Cut);
        let h = BRUSH_UNIT / 2;
        assert_eq!(intent.brush.centre.x, 10 * BRUSH_UNIT + h);
        assert_eq!(intent.brush.centre.y, 4 * BRUSH_UNIT + h);
        assert_eq!(intent.brush.centre.z, BRUSH_UNIT + h);
        assert_eq!(intent.brush.radius_units(), 2 * BRUSH_UNIT);
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        let req = action_req(
            77,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::UnknownTool(77))
        );
    }

    #[test]
    fn an_action_the_tool_does_not_grant_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Tool 0 is a cutter; asking it to place is a mismatch.
        let req = action_req(
            0,
            ActionKind::Place,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::ToolActionMismatch)
        );
    }

    #[test]
    fn an_excessive_tool_radius_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Tool 0 caps at 8 cells; the request asks for 40.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            40,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::RadiusTooLarge {
                requested_cells: 40,
                max_cells: 8,
            })
        );
    }

    #[test]
    fn an_aim_that_hits_nothing_within_reach_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        // Aim out of the scene (toward -X off the resident geometry): no solid.
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            [-1.0, 0.0, 0.0],
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::NoAuthoritativeHit)
        );
    }

    #[test]
    fn a_spoofed_target_that_disagrees_with_the_raycast_is_refused() {
        let sim = bridge_after_cut();
        let beam = sim
            .world()
            .bodies()
            .next()
            .and_then(|b| b.entity)
            .expect("the cut detached the beam");
        // Aim straight down onto the terrain floor at x-cell 1 (clear of both
        // the beam body's x-range and the cut column) but claim the real,
        // still-live beam body.
        let req = action_req(
            0,
            ActionKind::Cut,
            [0.375, 2.0, 0.375],
            [0.0, -1.0, 0.0],
            ClaimedTarget::Body(beam),
            [0, 0, 1],
            1,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::TargetMismatch)
        );
    }

    #[test]
    fn a_stale_target_body_is_refused() {
        let sim = Scene::BridgeCut.simulation();
        let req = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Body(EntityId::new(9999).unwrap()),
            [10, 4, 1],
            2,
        );
        assert_eq!(
            resolve_intent(sim.world(), sess(0, 1), &req),
            Err(ActionReject::StaleTarget)
        );
    }

    #[test]
    fn a_body_hit_resolves_in_the_body_local_frame() {
        let sim = bridge_after_cut();
        let beam = sim
            .world()
            .bodies()
            .next()
            .and_then(|b| b.entity)
            .expect("the cut detached the beam");
        // Aim straight down at x-cell 5: clear of the (surviving) column top,
        // so the nearest solid is the detached beam body itself.
        let req = action_req(
            0,
            ActionKind::Cut,
            [1.375, 7.0, 0.375],
            [0.0, -1.0, 0.0],
            ClaimedTarget::Body(beam),
            [999, 999, 999],
            1,
        );
        let intent = resolve_intent(sim.world(), sess(0, 1), &req).expect("resolves");
        assert_eq!(intent.target, EditTarget::Body(beam));
        // Server-derived in the beam's local frame, not the client's
        // `(999, 999, 999)` claim.
        assert!(intent.brush.centre.x.abs() < 100 * BRUSH_UNIT);
    }

    #[test]
    fn the_dev_scenario_path_trusts_the_claim_verbatim() {
        let req = action_req(
            0,
            ActionKind::Cut,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0],
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        let intent = dev_intent_from_request(sess(0, 1), &req).expect("dev passthrough");
        // No raycast, no re-derivation: exactly the claimed brush.
        assert_eq!(intent.brush, req.claimed_brush);
    }

    #[test]
    fn reliable_retry_after_reconnect_replays_before_stale_claim_validation() {
        let original_session = sess(0, 1);
        let replacement_session = sess(0, 2);
        let mut sim = Scene::BridgeCut.simulation();
        let mut request = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [3, 1, 1],
            2,
        );
        request.request_id = RequestId(57);

        let intent = resolve_intent(sim.world(), original_session, &request).unwrap();
        assert!(matches!(
            sim.submit(intent).unwrap().outcome,
            ActionOutcome::Queued
        ));
        sim.run_until_idle(8).unwrap();
        let committed = replay_admitted_status(&sim, request.request_id).unwrap();
        assert!(matches!(committed.outcome, ActionOutcome::Committed { .. }));

        // The original ray now points at removed column cells. A handler that
        // validated first would turn this reliable retry into a fresh rejection.
        assert!(
            resolve_intent(sim.world(), replacement_session, &request).is_err(),
            "the fixture must prove that commit changed the original claim's target"
        );
        assert_eq!(
            replay_admitted_status(&sim, request.request_id),
            Some(committed),
            "the replacement session receives the original committed status"
        );
    }

    #[test]
    fn pre_admission_validation_refusals_do_not_reserve_request_ids() {
        let sim = Scene::BridgeCut.simulation();
        let session = sess(0, 1);
        let rejected = action_req(
            77,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert!(resolve_intent(sim.world(), session, &rejected).is_err());
        assert!(replay_admitted_status(&sim, rejected.request_id).is_none());

        // This is deliberately still eligible for validation/admission with a
        // corrected payload: only accepted pipeline work has a replay ledger.
        let corrected = action_req(
            0,
            ActionKind::Cut,
            COLUMN_AIM_ORIGIN,
            PLUS_X,
            ClaimedTarget::Terrain,
            [10, 4, 1],
            2,
        );
        assert!(resolve_intent(sim.world(), session, &corrected).is_ok());
    }

    #[test]
    fn a_reconnect_supersedes_the_old_session_generation() {
        let mut lj = LateJoin::new(
            DEFAULT_CATCH_UP_CAP,
            DEFAULT_MAX_JOIN_RETRIES,
            default_capture_workers(),
        );
        let old = sess(0, 1);
        let new = sess(0, 2);
        lj.on_joined(old);
        assert!(!lj.session_expired(old));
        lj.on_joined(new);
        assert!(lj.session_expired(old), "the old generation is now expired");
        assert!(!lj.session_expired(new));
        // A straggler after the old link is gone is still rejected.
        lj.on_gone(old);
        assert!(lj.session_expired(old));
        // A slot that never connected is not "expired".
        assert!(!lj.session_expired(sess(3, 1)));
    }

    #[test]
    fn per_tick_admission_caps_each_session_independently() {
        let mut counts: HashMap<u64, u32> = HashMap::new();
        let flooder = sess(0, 1).raw();
        let other = sess(1, 1).raw();

        let mut admitted = 0u32;
        for _ in 0..1_000 {
            if admit(&mut counts, flooder, MAX_ACTIONS_PER_CLIENT_PER_TICK) {
                admitted += 1;
            }
        }
        assert_eq!(
            admitted, MAX_ACTIONS_PER_CLIENT_PER_TICK,
            "one session's flood is capped for the tick"
        );

        // A different session still gets its full quota — fair scheduling, the
        // flood above did not consume it.
        let mut other_ok = 0u32;
        for _ in 0..1_000 {
            if admit(&mut counts, other, MAX_ACTIONS_PER_CLIENT_PER_TICK) {
                other_ok += 1;
            }
        }
        assert_eq!(other_ok, MAX_ACTIONS_PER_CLIENT_PER_TICK);
    }
}
