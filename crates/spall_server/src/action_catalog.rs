//! Versioned, server-owned action policy supplied by the game host.

use std::collections::BTreeMap;

use spall_core::{MAX_BRUSH_RADIUS_CELLS, MaterialId};
use spall_sim::EditKind;

/// One game-approved edit tool. Clients send only `id`; operation, reach, and
/// radius limits are resolved from this server-owned definition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToolRule {
    pub id: u16,
    pub kind: EditKind,
    pub max_radius_cells: i64,
    pub reach_m: f64,
}

/// Validated game action catalog. `version` is chosen and advanced by the game
/// package when its rules change; the engine never imports game code.
#[derive(Debug, Clone)]
pub struct ToolCatalog {
    version: u32,
    tools: BTreeMap<u16, ToolRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ToolCatalogError {
    #[error("tool catalog version must be nonzero")]
    ZeroVersion,
    #[error("tool id {0} is declared more than once")]
    DuplicateId(u16),
    #[error("tool {0} has a radius outside the supported brush range")]
    InvalidRadius(u16),
    #[error("tool {0} has an invalid ray reach")]
    InvalidReach(u16),
}

impl ToolCatalog {
    pub fn new(
        version: u32,
        rules: impl IntoIterator<Item = ToolRule>,
    ) -> Result<Self, ToolCatalogError> {
        if version == 0 {
            return Err(ToolCatalogError::ZeroVersion);
        }
        let mut tools = BTreeMap::new();
        for rule in rules {
            if rule.max_radius_cells < 0 || rule.max_radius_cells > MAX_BRUSH_RADIUS_CELLS {
                return Err(ToolCatalogError::InvalidRadius(rule.id));
            }
            if !rule.reach_m.is_finite() || rule.reach_m <= 0.0 {
                return Err(ToolCatalogError::InvalidReach(rule.id));
            }
            if tools.insert(rule.id, rule).is_some() {
                return Err(ToolCatalogError::DuplicateId(rule.id));
            }
        }
        Ok(Self { version, tools })
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub fn get(&self, id: u16) -> Option<ToolRule> {
        self.tools.get(&id).copied()
    }
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new(
            1,
            [
                ToolRule {
                    id: 0,
                    kind: EditKind::Cut,
                    max_radius_cells: 8,
                    reach_m: 12.0,
                },
                ToolRule {
                    id: 1,
                    kind: EditKind::Place(MaterialId(1)),
                    max_radius_cells: 4,
                    reach_m: 8.0,
                },
            ],
        )
        .expect("built-in legacy action catalog is valid")
    }
}
