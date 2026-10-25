//! Game-ruled tool use (ENG-259): who may swing which tool. The tool catalog
//! says what a tool id does; these rules say whether this player may use it
//! right now (an admin-only hammer, a shovel only from the pack).
use super::*;

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

/// The game's tool rules. They run on the authoritative thread between
/// ticks, so they must be quick.
pub trait ToolRules: Send {
    /// May this player use this tool now? A refusal's reason goes back to the
    /// client. Asked for every cut and placement, before any hit test.
    fn allow(&mut self, tool_use: &ToolUse) -> Result<(), &'static str>;
}

/// Cloneable handle to the game's [`ToolRules`], so `HostConfig` keeps its
/// derives. Without one, every catalog tool is open to every player.
#[derive(Clone)]
pub struct ToolHooks(Arc<Mutex<Box<dyn ToolRules>>>);

impl ToolHooks {
    pub fn new(rules: impl ToolRules + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(rules))))
    }

    fn rules(&self) -> std::sync::MutexGuard<'_, Box<dyn ToolRules>> {
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
        .rules()
        .allow(&ToolUse {
            session,
            player,
            tool: req.tool,
            admin,
        })
        .map_err(ActionReject::Unsupported)
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
}
