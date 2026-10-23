//! Versioned, server-owned action policy supplied by the game host.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spall_core::{GlobalCell, MAX_BRUSH_RADIUS_CELLS, MaterialId};
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
    builds: Option<BuildTools>,
}

/// What an approved build action asks the game for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildRequest {
    pub player_id: Option<spall_protocol::PlayerId>,
    /// Session slot, for hosts without player credentials.
    pub slot: u32,
    pub tool: u16,
    /// The empty terrain cell against the struck surface, where a placement
    /// with this tool would land.
    pub cell: GlobalCell,
    /// The player's facing in quarter turns: 0 looks along `-Z`, 1 along
    /// `+X`, 2 along `+Z`, 3 along `-X`.
    pub facing: u8,
}

/// The game's structures: turns an approved build action into the terrain
/// cells to fill and their materials, or refuses with a reason the player
/// sees (for example, a missing cost). The server places the cells a few
/// per tick and skips any that are no longer empty, so a build can end up
/// partial; the game must charge for it when it accepts.
pub type StructureBuilder =
    Arc<dyn Fn(&BuildRequest) -> Result<Vec<(GlobalCell, MaterialId)>, &'static str> + Send + Sync>;

#[derive(Clone)]
struct BuildTools {
    ids: BTreeSet<u16>,
    builder: StructureBuilder,
}

impl std::fmt::Debug for BuildTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildTools")
            .field("ids", &self.ids)
            .finish()
    }
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
    #[error("build tool {0} must be a declared single-cell placement tool")]
    InvalidBuildTool(u16),
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
        Ok(Self {
            version,
            tools,
            builds: None,
        })
    }

    /// Marks `ids` as build tools: each is validated like its single-cell
    /// placement (reach, server raycast), then `builder` decides the cells.
    pub fn with_build_tools(
        mut self,
        ids: impl IntoIterator<Item = u16>,
        builder: StructureBuilder,
    ) -> Result<Self, ToolCatalogError> {
        let ids: BTreeSet<u16> = ids.into_iter().collect();
        for &id in &ids {
            match self.tools.get(&id) {
                Some(rule)
                    if matches!(rule.kind, EditKind::Place(_)) && rule.max_radius_cells == 0 => {}
                _ => return Err(ToolCatalogError::InvalidBuildTool(id)),
            }
        }
        self.builds = Some(BuildTools { ids, builder });
        Ok(self)
    }

    /// The game's builder when `id` is a build tool.
    pub fn builder_for(&self, id: u16) -> Option<&StructureBuilder> {
        self.builds
            .as_ref()
            .filter(|builds| builds.ids.contains(&id))
            .map(|builds| &builds.builder)
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
