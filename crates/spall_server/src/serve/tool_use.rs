//! Game-ruled tool use (ENG-259..261): who may swing which tool, what a
//! cutting tool does to the material it strikes, and what a committed tool
//! cut leaves behind. The tool catalog says what a tool id does; these rules
//! decide per player and per strike (an admin-only hammer, a shovel only from
//! the pack, digging fast in dirt and slowly in stone, an axe that fells a
//! tree away from the swing).
use super::*;
use spall_core::MaterialId;

/// A tool use about to be resolved, for [`ToolRules::allow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolUse {
    pub session: SessionId,
    /// The session's authenticated account, if any.
    pub player: Option<spall_protocol::PlayerId>,
    pub tool: u16,
    /// Whether this server grants admin commands
    /// ([`HostConfig::admin_commands`]): the server's only notion of admin.
    pub admin: bool,
}

/// What a cutting tool's authoritative ray struck, for [`ToolRules::swing`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolStrike {
    pub tool: u16,
    /// Material of the struck cell.
    pub material: MaterialId,
    /// The cell belongs to a detached body, not the terrain.
    pub on_body: bool,
    /// Thin snow lying on the struck face, metres.
    pub snow_depth_m: f64,
}

/// What a strike does, as the game decides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwingEffect {
    /// An ordinary cut centred on the struck cell. `radius_cells` replaces
    /// the requested radius (`None` keeps it); either way the tool's catalog
    /// cap still applies.
    Cut { radius_cells: Option<i64> },
    /// Shovel the thin snow off the surface within `radius_m` of the struck
    /// point instead of cutting anything.
    ClearSnow { radius_m: f64 },
}

/// A tool cut that has committed, for [`ToolRules::committed`].
#[derive(Debug, Clone, Copy)]
pub struct ToolCut<'a> {
    pub session: SessionId,
    pub player: Option<spall_protocol::PlayerId>,
    pub tool: u16,
    pub request_id: RequestId,
    /// Cells the cut removed, by material.
    pub removed: &'a std::collections::BTreeMap<MaterialId, u64>,
    pub on_body: bool,
}

/// What follows a committed tool cut.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolEffects {
    /// Item kinds the server spawns as world items on the ground at the cut.
    pub world_items: Vec<u16>,
    /// Bodies the cut split off start turning at this rate (rad/s) about the
    /// horizontal axis that tips their top away from the swinger, so a felled
    /// tree topples instead of dropping upright. `0` leaves them alone.
    pub topple_rad_s: f32,
}

/// The game's tool rules. They run on the authoritative thread between
/// ticks, so they must be quick.
pub trait ToolRules: Send {
    /// May this player use this tool now? A refusal's reason goes back to the
    /// client. Asked for every cut and placement, before any hit test.
    fn allow(&mut self, tool_use: &ToolUse) -> Result<(), &'static str>;

    /// What a cutting tool does to what it struck, or why it cannot.
    fn swing(&mut self, _strike: &ToolStrike) -> Result<SwingEffect, &'static str> {
        Ok(SwingEffect::Cut { radius_cells: None })
    }

    /// Called once when a tool cut commits; awards go wherever the game keeps
    /// them, and the returned effects are applied by the server.
    fn committed(&mut self, _cut: &ToolCut<'_>) -> ToolEffects {
        ToolEffects::default()
    }
}

/// A resolved tool cut waiting for its commit.
#[derive(Debug, Clone, Copy)]
struct PendingCut {
    session: SessionId,
    player: Option<spall_protocol::PlayerId>,
    tool: u16,
    on_body: bool,
    /// Where the authoritative ray struck, world metres.
    point_m: [f64; 3],
    /// The swing's horizontal heading (unit, or zero for a vertical swing).
    heading: [f64; 2],
    tick: u64,
}

/// Pending cuts older than this never committed (refused or stale) and are
/// forgotten.
const PENDING_CUT_TICKS: u64 = 600;

struct Hooks {
    rules: Box<dyn ToolRules>,
    pending: HashMap<RequestId, PendingCut>,
}

/// Cloneable handle to the game's [`ToolRules`], so `HostConfig` keeps its
/// derives. Without one, every catalog tool is open to every player.
#[derive(Clone)]
pub struct ToolHooks(Arc<Mutex<Hooks>>);

impl ToolHooks {
    pub fn new(rules: impl ToolRules + 'static) -> Self {
        Self(Arc::new(Mutex::new(Hooks {
            rules: Box::new(rules),
            pending: HashMap::new(),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Hooks> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for ToolHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ToolHooks")
    }
}

/// Asks the game whether `session` may use the request's tool. Interactions
/// (pickup, doors) are not tool swings and always pass, as does everything
/// when the game installed no rules.
pub(super) fn permit_tool(
    hooks: &Option<ToolHooks>,
    session: SessionId,
    player: Option<spall_protocol::PlayerId>,
    req: &ActionRequest,
    admin: bool,
) -> Result<(), ActionReject> {
    let Some(hooks) = hooks else {
        return Ok(());
    };
    if req.action == ActionKind::Interact {
        return Ok(());
    }
    hooks
        .lock()
        .rules
        .allow(&ToolUse {
            session,
            player,
            tool: req.tool,
            admin,
        })
        .map_err(ActionReject::Unsupported)
}

/// A resolved swing: an edit to stage, or an answer already given.
#[derive(Debug)]
pub(super) enum Swing {
    Edit(EditIntent),
    /// Done on the spot (snow shovelled off); the status is recorded so a
    /// retry gets the same answer.
    Done(ActionStatus),
}

/// [`resolve_intent_with_catalog`] with the game's say over a cut: the
/// server's own hit decides the struck material, the game's
/// [`ToolRules::swing`] decides the radius (or clears snow instead), and the
/// cut is remembered so its commit can award drops.
pub(super) fn resolve_swing(
    sim: &mut Simulation,
    session: SessionId,
    player: Option<spall_protocol::PlayerId>,
    req: &ActionRequest,
    catalog: &ToolCatalog,
    hooks: &Option<ToolHooks>,
) -> Result<Swing, ActionReject> {
    let (Some(hooks), Some(spec)) = (hooks, catalog.get(req.tool)) else {
        return resolve_intent_with_catalog(sim.world(), session, req, catalog).map(Swing::Edit);
    };
    if req.action != ActionKind::Cut || spec.kind != EditKind::Cut {
        return resolve_intent_with_catalog(sim.world(), session, req, catalog).map(Swing::Edit);
    }
    let origin = DVec3::from_array(req.aim_origin_m);
    let dir = DVec3::from_array(req.aim_dir.map(f64::from));
    if !origin.is_finite() || !dir.is_finite() || dir.length_squared() < 1e-24 {
        return Err(ActionReject::BadAim);
    }
    let dir = dir.normalize();
    let hit = nearest_hit(sim.world(), Ray::new(origin, dir), spec.reach_m)
        .ok_or(ActionReject::NoAuthoritativeHit)?;
    let (volume, on_body) = match hit.target {
        EditTarget::Terrain => (&sim.world().terrain().volume, false),
        EditTarget::Body(entity) => (
            &sim.world()
                .body(entity)
                .ok_or(ActionReject::StaleTarget)?
                .volume,
            true,
        ),
    };
    let Ok(spall_voxel::Sample::Filled(material)) = volume.sample(hit.cell) else {
        return Err(ActionReject::NoAuthoritativeHit);
    };
    let point_m = (origin + dir * hit.t_m).to_array();
    let strike = ToolStrike {
        tool: req.tool,
        material,
        on_body,
        snow_depth_m: if on_body {
            0.0
        } else {
            sim.snow_depth_m_at(point_m)
        },
    };
    let mut hooks = hooks.lock();
    let radius_cells = match hooks
        .rules
        .swing(&strike)
        .map_err(ActionReject::Unsupported)?
    {
        SwingEffect::ClearSnow { radius_m } => {
            sim.clear_snow(point_m, radius_m);
            let status = ActionStatus {
                request_id: req.request_id,
                outcome: ActionOutcome::Queued,
            };
            sim.record_action_status(status.clone());
            return Ok(Swing::Done(status));
        }
        SwingEffect::Cut { radius_cells } => radius_cells,
    };
    let mut req = req.clone();
    if let Some(radius) = radius_cells {
        let radius = radius.clamp(0, spec.max_radius_cells);
        req.claimed_brush = SphereBrush::new(req.claimed_brush.centre, radius * BRUSH_UNIT)
            .map_err(|_| ActionReject::Unsupported("tool radius is outside brush limits"))?;
    }
    let intent = resolve_intent_with_catalog(sim.world(), session, &req, catalog)?;
    let tick = sim.current_tick().get();
    hooks
        .pending
        .retain(|_, cut| tick.saturating_sub(cut.tick) <= PENDING_CUT_TICKS);
    hooks.pending.insert(
        req.request_id,
        PendingCut {
            session,
            player,
            tool: req.tool,
            on_body,
            point_m,
            heading: DVec3::new(dir.x, 0.0, dir.z)
                .try_normalize()
                .map_or([0.0; 2], |h| [h.x, h.z]),
            tick,
        },
    );
    Ok(Swing::Edit(intent))
}

/// After a tool cut commits: lets the game award it, spawns the world items
/// it asks for on the ground at the cut, and tips any bodies the cut split
/// off away from the swinger.
pub(super) fn apply_tool_effects(
    sim: &mut Simulation,
    hooks: &Option<ToolHooks>,
    request_id: RequestId,
    committed: &spall_sim::commit::Committed,
) {
    let Some(hooks) = hooks else {
        return;
    };
    let mut hooks = hooks.lock();
    let Some(cut) = hooks.pending.remove(&request_id) else {
        return;
    };
    let effects = hooks.rules.committed(&ToolCut {
        session: cut.session,
        player: cut.player,
        tool: cut.tool,
        request_id,
        removed: &committed.removed_materials,
        on_body: cut.on_body,
    });
    drop(hooks);
    let [hx, hz] = cut.heading;
    if effects.topple_rad_s > 0.0 && hx.hypot(hz) > 0.0 {
        // Spin about up x heading: the top moves along the heading.
        let w = f64::from(effects.topple_rad_s);
        let spin = [hz * w, 0.0, -hx * w].map(|v| v as f32);
        let push = [hx * w * 0.3, 0.0, hz * w * 0.3].map(|v| v as f32);
        for entity in &committed.children {
            if let Some(phys) = sim.world().body(*entity).map(|body| body.phys) {
                sim.world_mut()
                    .physics_mut()
                    .set_body_velocity(phys, push, spin);
            }
        }
    }
    let ground = ground_below(&sim.world().terrain().volume, cut.point_m);
    for (i, kind) in effects.world_items.into_iter().enumerate() {
        // A few centimetres apart, so a pile reads as several items.
        let spread = 0.12 * i as f64;
        let at = [ground[0] + spread, ground[1], ground[2] + spread * 0.5];
        if let Err(error) = sim.spawn_world_item(kind, at, i as f32 * 1.3) {
            tracing::error!(kind, ?error, "could not spawn a tool drop");
        }
    }
}

/// The top of the first solid terrain cell at or below `point_m` (within
/// 8 m), where a dropped item rests; `point_m` itself when there is none.
fn ground_below(volume: &spall_voxel::Volume, point_m: [f64; 3]) -> [f64; 3] {
    const CELL_M: f64 = 0.25;
    let x = (point_m[0] / CELL_M).floor() as i64;
    let z = (point_m[2] / CELL_M).floor() as i64;
    let top = (point_m[1] / CELL_M).floor() as i64;
    (top - 32..=top)
        .rev()
        .find(|&y| {
            matches!(
                volume.sample(GlobalCell::new(x, y, z)),
                Ok(spall_voxel::Sample::Filled(_))
            )
        })
        .map_or(point_m, |y| {
            [point_m[0], (y + 1) as f64 * CELL_M, point_m[2]]
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tool 0 is admin-only; everything else is open.
    struct AdminHammer;

    impl ToolRules for AdminHammer {
        fn allow(&mut self, tool_use: &ToolUse) -> Result<(), &'static str> {
            if tool_use.tool == 0 && !tool_use.admin {
                return Err("the hammer is an admin tool");
            }
            Ok(())
        }
    }

    fn request(tool: u16, action: ActionKind) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(1),
            input_seq: spall_protocol::InputSeq(1),
            action,
            tool,
            aim_origin_m: [0.0, 1.0, 0.375],
            aim_dir: [1.0, 0.0, 0.0],
            claimed_target: ClaimedTarget::Terrain,
            claimed_brush: SphereBrush::new(BrushPoint::from_units(0, 0, 0), 2 * BRUSH_UNIT)
                .unwrap(),
        }
    }

    #[test]
    fn a_non_admin_hammer_swing_is_refused_and_an_admin_swing_is_not() {
        let hooks = Some(ToolHooks::new(AdminHammer));
        let session = sess(0, 1);
        let hammer = request(0, ActionKind::Cut);
        assert_eq!(
            permit_tool(&hooks, session, None, &hammer, false),
            Err(ActionReject::Unsupported("the hammer is an admin tool"))
        );
        assert_eq!(permit_tool(&hooks, session, None, &hammer, true), Ok(()));
        // The admitted swing then resolves to an ordinary server-derived cut.
        let sim = Scene::BridgeCut.simulation();
        assert!(resolve_intent(sim.world(), session, &hammer).is_ok());
        // Other tools and interactions are untouched by the rule.
        assert_eq!(
            permit_tool(&hooks, session, None, &request(4, ActionKind::Place), false),
            Ok(())
        );
        assert_eq!(
            permit_tool(
                &hooks,
                session,
                None,
                &request(0, ActionKind::Interact),
                false
            ),
            Ok(())
        );
        // No rules installed: the catalog alone decides.
        assert_eq!(permit_tool(&None, session, None, &hammer, false), Ok(()));
    }

    const SHOVEL: u16 = 6;
    const ROCK: u16 = 3;

    /// A shovel that digs radius 0 in the fixtures' stone (2 elsewhere),
    /// clears thin snow deeper than 2 cm, and drops one rock per committed cut.
    struct Shovel;

    impl ToolRules for Shovel {
        fn allow(&mut self, _: &ToolUse) -> Result<(), &'static str> {
            Ok(())
        }

        fn swing(&mut self, strike: &ToolStrike) -> Result<SwingEffect, &'static str> {
            if strike.snow_depth_m > 0.02 {
                return Ok(SwingEffect::ClearSnow { radius_m: 0.75 });
            }
            Ok(SwingEffect::Cut {
                radius_cells: Some(if strike.material == spall_voxel::fixtures::STONE {
                    0
                } else {
                    2
                }),
            })
        }

        fn committed(&mut self, cut: &ToolCut<'_>) -> ToolEffects {
            ToolEffects {
                world_items: if cut.removed.is_empty() {
                    Vec::new()
                } else {
                    vec![ROCK]
                },
                ..ToolEffects::default()
            }
        }
    }

    /// Cuts every material at one radius.
    struct Fixed(i64);

    impl ToolRules for Fixed {
        fn allow(&mut self, _: &ToolUse) -> Result<(), &'static str> {
            Ok(())
        }

        fn swing(&mut self, _: &ToolStrike) -> Result<SwingEffect, &'static str> {
            Ok(SwingEffect::Cut {
                radius_cells: Some(self.0),
            })
        }
    }

    fn shovel_catalog() -> ToolCatalog {
        ToolCatalog::new(
            1,
            [crate::ToolRule {
                id: SHOVEL,
                kind: EditKind::Cut,
                max_radius_cells: 2,
                reach_m: 3.0,
            }],
        )
        .unwrap()
    }

    fn walk_arena() -> Simulation {
        Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::walk_arena_setup(),
        ))
        .unwrap()
    }

    /// Top of the walk arena's floor, metres.
    fn floor_m(sim: &Simulation) -> f64 {
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

    /// A shovel swing from 1.6 m above `(3, floor, 4)` down onto `(3, floor, 3)`.
    fn shovel_swing(id: u64, floor: f64) -> ActionRequest {
        ActionRequest {
            request_id: RequestId(id),
            aim_origin_m: [3.0, floor + 1.6, 4.0],
            aim_dir: [0.0, -0.848, -0.53],
            ..request(SHOVEL, ActionKind::Cut)
        }
    }

    #[test]
    fn the_games_radius_for_the_struck_material_replaces_the_requested_one() {
        let hooks = Some(ToolHooks::new(Shovel));
        let catalog = shovel_catalog();
        let mut sim = Scene::BridgeCut.simulation();
        // The bridge column is stone: the game says radius 0, whatever was asked.
        let swing = resolve_swing(
            &mut sim,
            sess(0, 1),
            None,
            &request(SHOVEL, ActionKind::Cut),
            &catalog,
            &hooks,
        );
        let Ok(Swing::Edit(intent)) = swing else {
            panic!("a cut: {swing:?}");
        };
        assert_eq!(intent.brush.radius_units(), 0);
        // A radius past the tool's cap is capped, not refused.
        let hooks = Some(ToolHooks::new(Fixed(99)));
        let mut sim = walk_arena();
        let floor = floor_m(&sim);
        let Ok(Swing::Edit(intent)) = resolve_swing(
            &mut sim,
            sess(0, 1),
            None,
            &shovel_swing(2, floor),
            &catalog,
            &hooks,
        ) else {
            panic!("a cut");
        };
        assert_eq!(intent.brush.radius_units(), 2 * BRUSH_UNIT);
    }

    #[test]
    fn a_committed_tool_cut_spawns_its_drops_on_the_ground_at_the_cut() {
        let hooks = Some(ToolHooks::new(Shovel));
        let mut sim = walk_arena();
        let floor = floor_m(&sim);
        let swing = shovel_swing(7, floor);
        let Ok(Swing::Edit(intent)) = resolve_swing(
            &mut sim,
            sess(0, 1),
            None,
            &swing,
            &shovel_catalog(),
            &hooks,
        ) else {
            panic!("a cut");
        };
        sim.submit(intent).unwrap();
        let committed = (0..120)
            .find_map(|_| {
                let report = sim.tick().unwrap();
                report
                    .committed
                    .into_iter()
                    .find(|(rid, _)| *rid == RequestId(7))
            })
            .expect("the cut commits")
            .1;
        sim.take_world_item_changes();
        apply_tool_effects(&mut sim, &hooks, RequestId(7), &committed);
        let items: Vec<_> = sim.world_items().copied().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ROCK);
        // On the bottom of the fresh dip, not floating at the old surface.
        let [x, y, z] = items[0].translation_m;
        assert!((x - 3.0).abs() < 0.3 && (z - 3.0).abs() < 0.3, "{x} {z}");
        assert!(
            y < floor && y >= floor - 1.0,
            "rests in the dip: {y} vs {floor}"
        );
        let cell = GlobalCell::new(
            (x / 0.25).floor() as i64,
            (y / 0.25).round() as i64 - 1,
            (z / 0.25).floor() as i64,
        );
        assert!(matches!(
            sim.world().terrain().volume.sample(cell),
            Ok(spall_voxel::Sample::Filled(_))
        ));
        // Announced to clients like any item the server spawns.
        let changes = sim.take_world_item_changes();
        assert_eq!(changes[0].spawned, items);
        // Applied once: a second call for the same commit does nothing.
        apply_tool_effects(&mut sim, &hooks, RequestId(7), &committed);
        assert_eq!(sim.world_items().count(), 1);
    }

    /// Cold heavy snow on every face of the arena tile under (0..4, 0..4) m.
    fn snow_over_tile(sim: &mut Simulation, floor: f64) {
        let mut snow = spall_sim::snow::SnowState::new();
        let y = (floor / 0.25).round() as i32 - 1;
        snow.reconcile(0, 0, y, 1, [u64::MAX; 4], 0);
        let mut weather = spall_protocol::WeatherState::initial(
            1,
            1,
            spall_protocol::WeatherClock {
                time_ms: 0,
                source: spall_protocol::WeatherClockSource::ServerFallback,
                start_season: 3,
                season_ms: 900_000,
            },
        );
        weather.force_kind(Some(spall_protocol::WeatherKind::Snow), None);
        weather.precipitation_q16 = u16::MAX;
        weather.temperature_centi_c = -800;
        for tick in 1..=8_000 {
            snow.advance(&weather, tick).unwrap();
        }
        sim.restore_snow_state(&snow.to_canonical_bytes().unwrap())
            .unwrap();
    }

    #[test]
    fn a_buried_item_is_refused_until_the_shovel_clears_the_snow_off_it() {
        let hooks = Some(ToolHooks::new(Shovel));
        let mut sim = walk_arena();
        let floor = floor_m(&sim);
        let item = sim.spawn_world_item(ROCK, [3.0, floor, 3.0], 0.0).unwrap();
        snow_over_tile(&mut sim, floor);
        assert!(
            sim.snow_depth_m_at(item.translation_m) > spall_sim::world_items::ITEM_BURIED_SNOW_M
        );
        let session = sess(0, 1);
        sim.add_player(session_player_entity(session), [3.0, floor, 4.0]);
        let pickup = ItemPickup::new(|_, _, _| Ok(()));
        let interact = |id| ActionRequest {
            request_id: RequestId(id),
            ..request(5, ActionKind::Interact)
        };
        let refused = pick_up_item(&mut sim, session, &interact(1), &pickup).expect("in reach");
        assert_eq!(
            refused.outcome,
            ActionOutcome::Rejected {
                reason: "buried under snow".into()
            }
        );
        assert!(sim.world_items().any(|i| i.item_id == item.item_id));

        // Shovelling the snow cuts nothing: it clears the layer, answered at once.
        let hash = sim.world().world_hash();
        sim.take_snow_frames();
        let swing = resolve_swing(
            &mut sim,
            session,
            None,
            &shovel_swing(2, floor),
            &shovel_catalog(),
            &hooks,
        );
        let Ok(Swing::Done(status)) = swing else {
            panic!("snow cleared: {swing:?}");
        };
        assert_eq!(status.outcome, ActionOutcome::Queued);
        assert_eq!(replay_admitted_status(&sim, RequestId(2)), Some(status));
        assert_eq!(sim.world().world_hash(), hash, "no terrain was cut");
        assert_eq!(sim.snow_depth_m_at(item.translation_m), 0.0);
        assert!(
            !sim.take_snow_frames().is_empty(),
            "the cleared layer replicates"
        );
        // Snow a metre and a half away was left alone.
        assert!(sim.snow_depth_m_at([1.5, floor, 3.0]) > 0.15);

        let taken = pick_up_item(&mut sim, session, &interact(3), &pickup).expect("in reach");
        assert_eq!(taken.outcome, ActionOutcome::Queued);
        assert!(!sim.world_items().any(|i| i.item_id == item.item_id));
    }

    /// Cuts terrain at radius 2 and bodies at radius 1; a committed terrain
    /// cut topples what it split off at `0` rad/s, and a cut into a body drops
    /// two sticks (kind 5).
    struct Felling(f32);

    impl ToolRules for Felling {
        fn allow(&mut self, _: &ToolUse) -> Result<(), &'static str> {
            Ok(())
        }

        fn swing(&mut self, strike: &ToolStrike) -> Result<SwingEffect, &'static str> {
            Ok(SwingEffect::Cut {
                radius_cells: Some(if strike.on_body { 1 } else { 2 }),
            })
        }

        fn committed(&mut self, cut: &ToolCut<'_>) -> ToolEffects {
            ToolEffects {
                world_items: if cut.on_body { vec![5, 5] } else { Vec::new() },
                topple_rad_s: if cut.on_body { 0.0 } else { self.0 },
            }
        }
    }

    /// A flat floor (`y 0..2` cells) with a 0.5 x 0.5 m trunk standing 4 m
    /// tall at `x, z 10..12` (2.5..3 m), resident air above.
    fn tree_world() -> Simulation {
        use spall_voxel::EditPlan;
        let mut setup = spall_sim::fixtures::flat_terrain_setup();
        let id = setup.terrain.id();
        for (a, b, material) in [
            (
                GlobalCell::new(0, 10, 0),
                GlobalCell::new(23, 31, 23),
                spall_core::MaterialId::AIR,
            ),
            (
                GlobalCell::new(10, 2, 10),
                GlobalCell::new(11, 17, 11),
                spall_voxel::fixtures::STONE,
            ),
        ] {
            setup
                .terrain
                .apply_edit(&EditPlan::filled_box(id, a, b, material))
                .unwrap();
        }
        setup.terrain_collider_region = (GlobalCell::new(0, 0, 0), GlobalCell::new(23, 31, 23));
        Simulation::new(spall_sim::SimulationConfig::new(setup)).unwrap()
    }

    /// Fells the tree with one swing from the west (+X) under `rules`, runs
    /// `ticks` ticks, and returns the felled body's tilt from upright, degrees.
    fn fell(rules: Felling, ticks: u32) -> f64 {
        let hooks = Some(ToolHooks::new(rules));
        let mut sim = tree_world();
        let catalog = ToolCatalog::new(
            1,
            [crate::ToolRule {
                id: 7,
                kind: EditKind::Cut,
                max_radius_cells: 2,
                reach_m: 3.0,
            }],
        )
        .unwrap();
        let swing = ActionRequest {
            request_id: RequestId(9),
            aim_origin_m: [1.0, 1.0, 2.75],
            aim_dir: [1.0, 0.0, 0.0],
            ..request(7, ActionKind::Cut)
        };
        let Ok(Swing::Edit(intent)) =
            resolve_swing(&mut sim, sess(0, 1), None, &swing, &catalog, &hooks)
        else {
            panic!("a cut");
        };
        sim.submit(intent).unwrap();
        let mut felled = None;
        for _ in 0..ticks {
            let report = sim.tick().unwrap();
            for (rid, committed) in &report.committed {
                apply_tool_effects(&mut sim, &hooks, *rid, committed);
                felled = felled.or(committed.children.first().copied());
            }
        }
        let body = sim
            .world()
            .body(felled.expect("the trunk split off"))
            .unwrap();
        let up = body.pose.rotation * DVec3::Y;
        up.angle_between(DVec3::Y).to_degrees()
    }

    #[test]
    fn an_axe_felled_trunk_topples_where_a_hammer_felled_one_drops_upright() {
        const TICKS: u32 = 180;
        let dropped = fell(Felling(0.0), TICKS);
        let toppled = fell(Felling(0.8), TICKS);
        assert!(dropped < 10.0, "hammer-felled tilt {dropped:.1} degrees");
        assert!(toppled > 45.0, "axe-felled tilt {toppled:.1} degrees");
        // Same binary, same swing: the same fall.
        assert_eq!(fell(Felling(0.8), TICKS), toppled);
    }

    #[test]
    fn chopping_a_loose_body_leaves_server_owned_sticks_on_the_ground() {
        let hooks = Some(ToolHooks::new(Felling(0.0)));
        let mut sim = bridge_after_cut();
        let beam = sim
            .world()
            .bodies()
            .find_map(|body| body.entity)
            .expect("the bridge cut split the beam off");
        // Let the beam come down, then chop it from above.
        for _ in 0..240 {
            sim.tick().unwrap();
        }
        // A beam cell (the bridge's beam spans local x 4..20 at y 8, z 1),
        // wherever the fall left it; the body's origin is not its geometry.
        let body = sim.world().body(beam).unwrap();
        assert!(matches!(
            body.volume.sample(GlobalCell::new(12, 8, 1)),
            Ok(spall_voxel::Sample::Filled(_))
        ));
        let [x, y, z] = body
            .pose
            .xform(body.cell_size())
            .local_cell_to_world_m(DVec3::new(12.5, 8.5, 1.5))
            .to_array();
        let chop = ActionRequest {
            request_id: RequestId(11),
            aim_origin_m: [x, y + 1.5, z],
            aim_dir: [0.0, -1.0, 0.0],
            claimed_target: ClaimedTarget::Body(beam),
            ..request(7, ActionKind::Cut)
        };
        let catalog = ToolCatalog::new(
            1,
            [crate::ToolRule {
                id: 7,
                kind: EditKind::Cut,
                max_radius_cells: 2,
                reach_m: 3.0,
            }],
        )
        .unwrap();
        let swing = resolve_swing(&mut sim, sess(0, 1), None, &chop, &catalog, &hooks);
        let Ok(Swing::Edit(intent)) = swing else {
            panic!("a chop: {swing:?}");
        };
        assert_eq!(intent.target, EditTarget::Body(beam));
        sim.submit(intent).unwrap();
        sim.take_world_item_changes();
        let committed = (0..120)
            .find_map(|_| {
                let report = sim.tick().unwrap();
                assert!(report.rejected.is_empty(), "{:?}", report.rejected);
                report
                    .committed
                    .into_iter()
                    .find(|(rid, _)| *rid == RequestId(11))
            })
            .unwrap_or_else(|| panic!("the chop commits: {:?}", sim.action_status(RequestId(11))))
            .1;
        apply_tool_effects(&mut sim, &hooks, RequestId(11), &committed);
        let sticks: Vec<_> = sim.world_items().copied().collect();
        assert_eq!(sticks.len(), 2);
        assert!(sticks.iter().all(|s| s.kind == 5));
        // The server allocated them and announces them like any other item.
        let announced = sim.take_world_item_changes();
        assert_eq!(announced[0].spawned.len(), 2);
        for stick in &sticks {
            assert!((stick.translation_m[0] - x).abs() < 1.0);
            assert!(stick.translation_m[1] < y, "on the ground below the log");
        }
    }
}
