//! The sandbox's tool rules (ENG-259): which player may swing which tool.
//!
//! Players start empty-handed. The hammer (the catalog's `DIG` tool) is an
//! admin tool: the server accepts its swings only when it grants admin
//! commands.

use spall_server::{ToolRules, ToolUse};

use crate::game::tool_ids;

/// Refusal for a hammer swing from a non-admin.
pub const HAMMER_IS_ADMIN_ONLY: &str = "the hammer is an admin tool";

/// The server-side rules for the sandbox's tools.
#[derive(Debug, Default)]
pub struct SandboxToolRules;

impl ToolRules for SandboxToolRules {
    fn allow(&mut self, tool_use: &ToolUse) -> Result<(), &'static str> {
        match tool_use.tool {
            tool_ids::DIG if !tool_use.admin => Err(HAMMER_IS_ADMIN_ONLY),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::{SessionId, SlotId};

    fn tool_use(tool: u16, admin: bool) -> ToolUse {
        ToolUse {
            session: SessionId::from_parts(SlotId(0), 1),
            player: None,
            tool,
            admin,
        }
    }

    #[test]
    fn only_an_admin_may_swing_the_hammer() {
        let mut rules = SandboxToolRules;
        assert_eq!(
            rules.allow(&tool_use(tool_ids::DIG, false)),
            Err(HAMMER_IS_ADMIN_ONLY)
        );
        assert_eq!(rules.allow(&tool_use(tool_ids::DIG, true)), Ok(()));
        // The torch is not a gated tool.
        assert_eq!(rules.allow(&tool_use(tool_ids::PLACE_LAMP, false)), Ok(()));
    }
}
