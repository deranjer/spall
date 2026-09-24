//! Minimal game-owned content for the sandbox example.
//!
//! This is the `sandbox_game` layer from `docs/architecture.md`: the engine
//! (`spall_sim` and the other `spall_*` crates) never imports it, but the
//! example is free to depend on the engine. Game-owned content turns player
//! tools into engine [`EditIntent`]s and defines material/tool catalogs,
//! recipes, impact profiles, and spawn rules. Asset loading remains future
//! T25 work.

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{
    CellSizeCode, EntityId, GlobalCell, MaterialDef, MaterialFlags, MaterialId, MaterialManifest,
    RenderProps, SimProps, SphereBrush, VolumeId,
};
use spall_sim::{BodyPose, EditIntent, EditKind, EditTarget, RequestId, Simulation};
use spall_voxel::{EditPlan, Volume};

/// Stable ids for the sandbox's handful of materials. Numeric ids are fixed
/// here, never assigned by enumeration order.
pub mod materials {
    use spall_core::MaterialId;
    pub const AIR: MaterialId = MaterialId(0);
    pub const STONE: MaterialId = MaterialId(1);
    pub const DIRT: MaterialId = MaterialId(2);
    pub const WOOD: MaterialId = MaterialId(3);
}

/// Version of the sandbox's server-authoritative content rules.
pub const GAME_RULES_VERSION: u32 = 2;

/// Version of the sandbox-owned collision damage values below.
pub const DAMAGE_RULES_VERSION: u32 = 2;

/// Game-selected tuning for the existing authoritative contact-damage pass.
/// The thresholds start from the T21 analytic fixture values; gameplay tuning
/// can change here without replacing the engine's contact resolution.
pub const IMPACT_DAMAGE_RULES: spall_sim::ContactDamageConfig = spall_sim::ContactDamageConfig {
    impact_ratio: 6.0,
    min_impulse_n_s: 40.0,
    cooldown_ticks: 20,
    max_intents_per_tick: 4,
    brush_radius_cells: 2,
    explosion_ratio: 20.0,
    explosion_scale: 0.2,
    still_speed_m_s: 0.05,
};

/// Returns this game's versioned impact-damage policy for the authoritative
/// server simulation.
pub const fn contact_damage_config() -> spall_sim::ContactDamageConfig {
    IMPACT_DAMAGE_RULES
}

/// Per-material impact response. These are game-owned balance choices; the
/// engine applies the scales to its shared threshold, brush, and detachment
/// rules without knowing the material names.
pub fn contact_damage_profiles() -> Vec<(MaterialId, spall_sim::ContactDamageMaterialProfile)> {
    use materials::*;
    use spall_sim::ContactDamageMaterialProfile as Profile;
    vec![
        (
            STONE,
            Profile {
                impact_ratio_scale: 1.5,
                brush_radius_cells: 1,
                explosion_ratio_scale: 1.5,
                explosion_scale: 0.15,
            },
        ),
        (
            DIRT,
            Profile {
                impact_ratio_scale: 0.7,
                brush_radius_cells: 1,
                explosion_ratio_scale: 0.8,
                explosion_scale: 0.18,
            },
        ),
        (
            WOOD,
            Profile {
                impact_ratio_scale: 0.8,
                brush_radius_cells: 2,
                explosion_ratio_scale: 0.9,
                explosion_scale: 0.2,
            },
        ),
    ]
}

/// Version of the sandbox's stable recipe definitions.
pub const RECIPE_CATALOG_VERSION: u32 = 1;

/// Stable game item identity, independent of display names or source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub u16);

/// Stable recipe identity used by game-owned craft requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecipeId(pub u16);

pub mod items {
    use super::ItemId;
    pub const WOOD_LOG: ItemId = ItemId(1);
    pub const WOOD_PLANK: ItemId = ItemId(2);
    pub const STONE_CHUNK: ItemId = ItemId(3);
    pub const STONE_BLOCK: ItemId = ItemId(4);
}

pub mod recipe_ids {
    use super::RecipeId;
    pub const SAW_PLANKS: RecipeId = RecipeId(1);
    pub const SHAPE_STONE: RecipeId = RecipeId(2);
}

/// A positive count of one stable game item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    pub item: ItemId,
    pub count: u32,
}

/// A recipe definition. Ingredient and output item IDs must be unique on each
/// side; quantities are per craft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub id: RecipeId,
    pub ingredients: Vec<ItemStack>,
    pub outputs: Vec<ItemStack>,
}

/// Versioned, immutable game-owned recipe definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeCatalog {
    pub version: u32,
    recipes: std::collections::BTreeMap<RecipeId, Recipe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeCatalogError {
    ZeroVersion,
    DuplicateRecipe(RecipeId),
    InvalidRecipe(RecipeId),
}

impl RecipeCatalog {
    pub fn new(
        version: u32,
        recipes: impl IntoIterator<Item = Recipe>,
    ) -> Result<Self, RecipeCatalogError> {
        if version == 0 {
            return Err(RecipeCatalogError::ZeroVersion);
        }
        let mut by_id = std::collections::BTreeMap::new();
        for recipe in recipes {
            if recipe.id.0 == 0
                || !valid_stacks(&recipe.ingredients)
                || !valid_stacks(&recipe.outputs)
            {
                return Err(RecipeCatalogError::InvalidRecipe(recipe.id));
            }
            let id = recipe.id;
            if by_id.insert(id, recipe).is_some() {
                return Err(RecipeCatalogError::DuplicateRecipe(id));
            }
        }
        Ok(Self {
            version,
            recipes: by_id,
        })
    }

    pub fn get(&self, id: RecipeId) -> Option<&Recipe> {
        self.recipes.get(&id)
    }

    pub fn len(&self) -> usize {
        self.recipes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.recipes.is_empty()
    }
}

fn valid_stacks(stacks: &[ItemStack]) -> bool {
    !stacks.is_empty()
        && stacks
            .iter()
            .all(|stack| stack.item.0 != 0 && stack.count > 0)
        && stacks
            .iter()
            .enumerate()
            .all(|(index, stack)| stacks[..index].iter().all(|prior| prior.item != stack.item))
}

/// Inventory owned by the authoritative game/session layer. It deliberately
/// contains item IDs, not voxel material IDs or engine entity handles.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inventory {
    revision: u64,
    stacks: std::collections::BTreeMap<ItemId, u32>,
}

impl Inventory {
    /// Grants an authoritative game-owned drop, advancing the inventory
    /// revision so any previously staged craft is rejected as stale.
    pub fn grant(&mut self, item: ItemId, count: u32) -> Result<(), CraftError> {
        self.grant_many([ItemStack { item, count }])
    }

    /// Atomically grants a batch of drops with one inventory revision change.
    pub fn grant_many(
        &mut self,
        stacks: impl IntoIterator<Item = ItemStack>,
    ) -> Result<(), CraftError> {
        let mut next = self.stacks.clone();
        let mut changed = false;
        for stack in stacks {
            if stack.item.0 == 0 || stack.count == 0 {
                return Err(CraftError::InvalidStack);
            }
            let count = next
                .get(&stack.item)
                .copied()
                .unwrap_or(0)
                .checked_add(stack.count)
                .ok_or(CraftError::Overflow)?;
            next.insert(stack.item, count);
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(CraftError::RevisionExhausted)?;
        self.stacks = next;
        self.revision = revision;
        Ok(())
    }

    pub fn from_stacks(stacks: impl IntoIterator<Item = ItemStack>) -> Result<Self, CraftError> {
        let mut result = Self::default();
        for stack in stacks {
            if stack.item.0 == 0 || stack.count == 0 {
                return Err(CraftError::InvalidStack);
            }
            let current = result.stacks.get(&stack.item).copied().unwrap_or(0);
            result.stacks.insert(
                stack.item,
                current
                    .checked_add(stack.count)
                    .ok_or(CraftError::Overflow)?,
            );
        }
        Ok(result)
    }

    /// Reconstructs an inventory from its explicit persisted revision and
    /// canonical stack list. Duplicate item IDs are invalid in persisted
    /// snapshots rather than being silently merged.
    pub fn restore(
        revision: u64,
        stacks: impl IntoIterator<Item = ItemStack>,
    ) -> Result<Self, CraftError> {
        let stacks: Vec<_> = stacks.into_iter().collect();
        let restored = Self::from_stacks(stacks.iter().copied())?;
        if restored.stacks.len() != stacks.len() {
            return Err(CraftError::InvalidStack);
        }
        Ok(Self {
            revision,
            ..restored
        })
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn count(&self, item: ItemId) -> u32 {
        self.stacks.get(&item).copied().unwrap_or(0)
    }

    pub fn stacks(&self) -> impl Iterator<Item = ItemStack> + '_ {
        self.stacks
            .iter()
            .map(|(&item, &count)| ItemStack { item, count })
    }

    /// Commits a staged crafting transaction atomically if its base revision is
    /// still current. Rejected/stale commits leave the inventory unchanged.
    pub fn commit(&mut self, transaction: CraftTransaction) -> Result<CraftReceipt, CraftError> {
        if self.revision != transaction.base_revision {
            return Err(CraftError::StaleInventory {
                expected: transaction.base_revision,
                actual: self.revision,
            });
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(CraftError::RevisionExhausted)?;
        self.stacks = transaction.next_stacks;
        Ok(transaction.receipt)
    }
}

/// Maps a material drop emitted by a server-validated harvest to the game's
/// stable inventory item identity. Call only after the world edit commits.
pub fn record_gathered_material_drop(
    inventory: &mut Inventory,
    material: MaterialId,
    count: u32,
) -> Result<(), CraftError> {
    let item = if material == materials::WOOD {
        items::WOOD_LOG
    } else if material == materials::STONE {
        items::STONE_CHUNK
    } else {
        return Err(CraftError::InvalidStack);
    };
    inventory.grant(item, count)
}

/// One server-run player's authoritative progression state, keyed by the
/// protocol connection slot. Slots survive reconnect generations, but are not
/// account IDs and this in-memory store is intentionally not durable yet.
#[derive(Debug, Clone, Default)]
pub struct PlayerInventories {
    players: std::collections::BTreeMap<InventoryOwner, Inventory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InventoryOwner {
    LegacySlot(u32),
    Player(spall_protocol::PlayerId),
}

impl PlayerInventories {
    pub fn ensure(&mut self, player_slot: u32) -> &mut Inventory {
        self.players
            .entry(InventoryOwner::LegacySlot(player_slot))
            .or_default()
    }

    /// Returns inventory for a server-authenticated stable player principal.
    pub fn ensure_player(&mut self, player_id: spall_protocol::PlayerId) -> &mut Inventory {
        self.players
            .entry(InventoryOwner::Player(player_id))
            .or_default()
    }

    pub fn get(&self, player_slot: u32) -> Option<&Inventory> {
        self.players.get(&InventoryOwner::LegacySlot(player_slot))
    }

    pub fn get_player(&self, player_id: spall_protocol::PlayerId) -> Option<&Inventory> {
        self.players.get(&InventoryOwner::Player(player_id))
    }

    /// Awards bounded game drops from the exact material delta of a committed
    /// cut. Sixteen removed wood/stone cells yield one corresponding item;
    /// other materials currently have no drop rule.
    pub fn record_committed_cut(
        &mut self,
        player_slot: u32,
        removed: &std::collections::BTreeMap<MaterialId, u64>,
    ) -> Result<Vec<ItemStack>, CraftError> {
        let mut drops = Vec::new();
        for (material, cells_per_item, item) in [
            (materials::WOOD, 16_u64, items::WOOD_LOG),
            (materials::STONE, 16_u64, items::STONE_CHUNK),
        ] {
            let count = removed.get(&material).copied().unwrap_or(0) / cells_per_item;
            if count > 0 {
                drops.push(ItemStack {
                    item,
                    count: u32::try_from(count).map_err(|_| CraftError::Overflow)?,
                });
            }
        }
        if drops.is_empty() {
            return Ok(drops);
        }
        self.players
            .entry(InventoryOwner::LegacySlot(player_slot))
            .or_default()
            .grant_many(drops.iter().copied())?;
        Ok(drops)
    }

    /// Same committed-cut award path for a server-authenticated player.
    pub fn record_committed_cut_for_player(
        &mut self,
        player_id: spall_protocol::PlayerId,
        removed: &std::collections::BTreeMap<MaterialId, u64>,
    ) -> Result<Vec<ItemStack>, CraftError> {
        let mut drops = Vec::new();
        for (material, cells_per_item, item) in [
            (materials::WOOD, 16_u64, items::WOOD_LOG),
            (materials::STONE, 16_u64, items::STONE_CHUNK),
        ] {
            let count = removed.get(&material).copied().unwrap_or(0) / cells_per_item;
            if count > 0 {
                drops.push(ItemStack {
                    item,
                    count: u32::try_from(count).map_err(|_| CraftError::Overflow)?,
                });
            }
        }
        if drops.is_empty() {
            return Ok(drops);
        }
        self.players
            .entry(InventoryOwner::Player(player_id))
            .or_default()
            .grant_many(drops.iter().copied())?;
        Ok(drops)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CraftRequest {
    pub recipe: RecipeId,
    pub batch_count: u32,
    pub expected_catalog_version: u32,
    pub expected_inventory_revision: u64,
}

/// Fully validated inventory replacement plus the revision it was based on.
/// Callers stage on the owning server thread and apply with `Inventory::commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftTransaction {
    base_revision: u64,
    next_stacks: std::collections::BTreeMap<ItemId, u32>,
    receipt: CraftReceipt,
}

/// Stages and commits one authorized craft request against the current game
/// inventory. A server/session owner should call this only after authenticating
/// the player and selecting that player's authoritative inventory.
pub fn craft(
    inventory: &mut Inventory,
    catalog: &RecipeCatalog,
    request: CraftRequest,
) -> Result<CraftReceipt, CraftError> {
    let transaction = catalog.stage(inventory, request)?;
    inventory.commit(transaction)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CraftReceipt {
    pub recipe: RecipeId,
    pub catalog_version: u32,
    pub batch_count: u32,
    pub consumed: Vec<ItemStack>,
    pub produced: Vec<ItemStack>,
    pub inventory_revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraftError {
    InvalidStack,
    CatalogVersionMismatch { expected: u32, actual: u32 },
    InventoryRevisionMismatch { expected: u64, actual: u64 },
    UnknownRecipe(RecipeId),
    ZeroBatch,
    InsufficientItems(ItemId),
    Overflow,
    StaleInventory { expected: u64, actual: u64 },
    RevisionExhausted,
}

impl RecipeCatalog {
    /// Validates a request against an immutable catalog and inventory snapshot.
    /// This does not mutate inventory; the returned transaction is committed
    /// separately with its optimistic revision check.
    pub fn stage(
        &self,
        inventory: &Inventory,
        request: CraftRequest,
    ) -> Result<CraftTransaction, CraftError> {
        if request.expected_catalog_version != self.version {
            return Err(CraftError::CatalogVersionMismatch {
                expected: request.expected_catalog_version,
                actual: self.version,
            });
        }
        if request.expected_inventory_revision != inventory.revision {
            return Err(CraftError::InventoryRevisionMismatch {
                expected: request.expected_inventory_revision,
                actual: inventory.revision,
            });
        }
        if request.batch_count == 0 {
            return Err(CraftError::ZeroBatch);
        }
        let recipe = self
            .get(request.recipe)
            .ok_or(CraftError::UnknownRecipe(request.recipe))?;
        let scale = |stacks: &[ItemStack]| -> Result<Vec<ItemStack>, CraftError> {
            stacks
                .iter()
                .map(|stack| {
                    Ok(ItemStack {
                        item: stack.item,
                        count: stack
                            .count
                            .checked_mul(request.batch_count)
                            .ok_or(CraftError::Overflow)?,
                    })
                })
                .collect()
        };
        let consumed = scale(&recipe.ingredients)?;
        let produced = scale(&recipe.outputs)?;
        let mut next = inventory.stacks.clone();
        for stack in &consumed {
            let available = next.get(&stack.item).copied().unwrap_or(0);
            if available < stack.count {
                return Err(CraftError::InsufficientItems(stack.item));
            }
            if available == stack.count {
                next.remove(&stack.item);
            } else {
                next.insert(stack.item, available - stack.count);
            }
        }
        for stack in &produced {
            let available = next.get(&stack.item).copied().unwrap_or(0);
            next.insert(
                stack.item,
                available
                    .checked_add(stack.count)
                    .ok_or(CraftError::Overflow)?,
            );
        }
        let inventory_revision = inventory
            .revision
            .checked_add(1)
            .ok_or(CraftError::RevisionExhausted)?;
        Ok(CraftTransaction {
            base_revision: inventory.revision,
            next_stacks: next,
            receipt: CraftReceipt {
                recipe: recipe.id,
                catalog_version: self.version,
                batch_count: request.batch_count,
                consumed,
                produced,
                inventory_revision,
            },
        })
    }
}

/// Versioned starter recipes used by the sandbox game layer.
pub fn recipe_catalog() -> RecipeCatalog {
    RecipeCatalog::new(
        RECIPE_CATALOG_VERSION,
        [
            Recipe {
                id: recipe_ids::SAW_PLANKS,
                ingredients: vec![ItemStack {
                    item: items::WOOD_LOG,
                    count: 1,
                }],
                outputs: vec![ItemStack {
                    item: items::WOOD_PLANK,
                    count: 4,
                }],
            },
            Recipe {
                id: recipe_ids::SHAPE_STONE,
                ingredients: vec![ItemStack {
                    item: items::STONE_CHUNK,
                    count: 4,
                }],
                outputs: vec![ItemStack {
                    item: items::STONE_BLOCK,
                    count: 1,
                }],
            },
        ],
    )
    .expect("sandbox recipes have unique IDs and positive stacks")
}

/// Stable IDs used by `ActionRequest::tool`; IDs are not inferred from order.
pub mod tool_ids {
    pub const DIG: u16 = 0;
    pub const PLACE_STONE: u16 = 1;
    pub const PLACE_WOOD: u16 = 2;
    pub const PLACE_DIRT: u16 = 3;
}

fn def(id: MaterialId, name: &str, density: f32, albedo: [f32; 3]) -> MaterialDef {
    MaterialDef {
        id,
        name: name.into(),
        render: RenderProps {
            albedo,
            roughness: 0.85,
            metalness: 0.0,
            emissive: [0.0; 3],
        },
        sim: SimProps {
            density_kg_m3: density,
            friction: 0.8,
            restitution: 0.05,
            hardness: 3.0,
            bond_strength: 10.0,
            flags: MaterialFlags(
                MaterialFlags::OPAQUE.0 | MaterialFlags::COLLIDES.0 | MaterialFlags::STRUCTURAL.0,
            ),
        },
    }
}

/// The sandbox world's validated material manifest.
pub fn manifest() -> MaterialManifest {
    let mut entries = vec![
        MaterialDef {
            id: materials::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        },
        def(materials::STONE, "stone", 2600.0, [0.50, 0.50, 0.52]),
        def(materials::DIRT, "dirt", 1500.0, [0.35, 0.25, 0.15]),
        def(materials::WOOD, "wood", 700.0, [0.45, 0.30, 0.15]),
    ];
    // Preserve the established playground scene's authored palette in the
    // game manifest so selecting that scene does not lose its material IDs.
    entries.extend(
        spall_sim::fixtures::playground_manifest()
            .entries()
            .iter()
            .filter(|entry| entry.id.0 >= 10)
            .cloned(),
    );
    MaterialManifest::validated(entries).expect("sandbox manifest is valid")
}

/// Server policy for the example's network-visible tools. The server owns the
/// hit test and applies these rules; a client-selected material is never trusted.
pub fn tool_catalog() -> spall_server::ToolCatalog {
    use spall_server::{ToolCatalog, ToolRule};
    ToolCatalog::new(
        GAME_RULES_VERSION,
        [
            ToolRule {
                id: tool_ids::DIG,
                kind: EditKind::Cut,
                max_radius_cells: 8,
                reach_m: 12.0,
            },
            ToolRule {
                id: tool_ids::PLACE_STONE,
                kind: EditKind::Place(materials::STONE),
                max_radius_cells: 4,
                reach_m: 8.0,
            },
            ToolRule {
                id: tool_ids::PLACE_WOOD,
                kind: EditKind::Place(materials::WOOD),
                max_radius_cells: 2,
                reach_m: 8.0,
            },
            ToolRule {
                id: tool_ids::PLACE_DIRT,
                kind: EditKind::Place(materials::DIRT),
                max_radius_cells: 2,
                reach_m: 8.0,
            },
        ],
    )
    .expect("sandbox action catalog is valid")
}

/// A player tool: a fixed-radius sphere brush that either removes material or
/// places one material.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub id: u16,
    pub radius_cells: i64,
    pub kind: EditKind,
}

impl Tool {
    /// The one-metre digging tool: a 2-cell-radius (`0.5 m`) cut sphere, matching
    /// the `4 x 4 x 4` cell footprint of a one-metre placement in
    /// `docs/architecture.md`.
    pub const DIG: Tool = Tool {
        id: tool_ids::DIG,
        radius_cells: 2,
        kind: EditKind::Cut,
    };

    /// The one-metre stone placement tool.
    pub const PLACE_STONE: Tool = Tool {
        id: tool_ids::PLACE_STONE,
        radius_cells: 2,
        kind: EditKind::Place(materials::STONE),
    };

    /// The one-metre wood placement tool. This stays in game-owned content:
    /// the engine receives only a normal authoritative placement intent.
    pub const PLACE_WOOD: Tool = Tool {
        id: tool_ids::PLACE_WOOD,
        radius_cells: 2,
        kind: EditKind::Place(materials::WOOD),
    };

    /// One-metre dirt placement. Dirt is present in the engine's built-in
    /// bounded-world material manifest, so this tool can be used immediately.
    pub const PLACE_DIRT: Tool = Tool {
        id: tool_ids::PLACE_DIRT,
        radius_cells: 2,
        kind: EditKind::Place(materials::DIRT),
    };

    /// Turns a tool use — aimed at an already-validated `(cell_x, cell_y,
    /// cell_z)` in the target volume's local cell space — into an engine
    /// [`EditIntent`]. The server still owns whether it commits.
    pub fn intent(
        &self,
        request_id: RequestId,
        actor: EntityId,
        target: EditTarget,
        cell_x: i64,
        cell_y: i64,
        cell_z: i64,
    ) -> EditIntent {
        let half = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(
                cell_x * BRUSH_UNIT + half,
                cell_y * BRUSH_UNIT + half,
                cell_z * BRUSH_UNIT + half,
            ),
            self.radius_cells * BRUSH_UNIT,
        )
        .expect("tool radius is within brush limits");
        EditIntent {
            request_id,
            actor,
            target,
            kind: self.kind,
            brush,
            explosion: None,
        }
    }
}

/// Spawns the sandbox's 0.5 m wood crate into a fresh authoritative world.
/// The server supplies the simulation; IDs, density, material, and shape come
/// from game content, and the normal simulation body-spawn API allocates stable
/// runtime IDs and physics state.
pub fn spawn_demo_wood_crate(
    simulation: &mut Simulation,
    position_m: [f64; 3],
) -> Result<EntityId, String> {
    let density = simulation
        .world()
        .materials()
        .get(materials::WOOD)
        .ok_or_else(|| "wood is absent from the authoritative world manifest".to_string())?
        .sim
        .density_kg_m3;
    let mut pose = BodyPose::identity();
    pose.translation_m = position_m;
    simulation
        .world_mut()
        .spawn_body(
            |volume_id: VolumeId| {
                let mut volume = Volume::new(volume_id, CellSizeCode::Quarter);
                let shape = EditPlan::filled_box(
                    volume_id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(1, 1, 1),
                    materials::WOOD,
                );
                volume
                    .apply_edit(&shape)
                    .expect("sandbox crate is a valid solid voxel box");
                volume
            },
            pose,
            [0.0; 3],
            [0.0; 3],
            density,
            0,
        )
        .map_err(|error| format!("could not spawn sandbox wood crate: {error}"))
}

/// Explicit portable SPVX key mapping for the sandbox. Import never guesses a
/// material from a display tint or file ordering.
pub fn asset_material_mapping() -> std::collections::BTreeMap<String, MaterialId> {
    [
        ("stone".to_owned(), materials::STONE),
        ("dirt".to_owned(), materials::DIRT),
        ("wood".to_owned(), materials::WOOD),
    ]
    .into_iter()
    .collect()
}

/// Converts a decoded static asset into one authoritative destructible body.
/// Its authored pivot is placed at `pivot_position_m`; mass properties use the
/// per-material densities from the world's active manifest.
pub fn spawn_loaded_voxel_asset(
    simulation: &mut Simulation,
    asset: crate::content::LoadedVoxelAsset,
    pivot_position_m: [f64; 3],
) -> Result<EntityId, String> {
    if asset.cells.is_empty() {
        return Err("cannot spawn an empty voxel asset".into());
    }
    let mut densities = std::collections::BTreeMap::new();
    for cell in &asset.cells {
        let definition = simulation
            .world()
            .materials()
            .get(cell.material)
            .ok_or_else(|| format!("asset references unknown material {}", cell.material.0))?;
        densities.insert(cell.material, definition.sim.density_kg_m3);
    }
    let pivot_offset_m = asset
        .pivot_subcells
        .map(|subcells| f64::from(subcells) / 256.0 * asset.cell_size.metres());
    let mut pose = BodyPose::identity();
    pose.translation_m = [
        pivot_position_m[0] - pivot_offset_m[0],
        pivot_position_m[1] - pivot_offset_m[1],
        pivot_position_m[2] - pivot_offset_m[2],
    ];
    let cells = asset.cells;
    simulation
        .world_mut()
        .spawn_body_with_material_densities(
            |volume_id| {
                let mut volume = Volume::new(volume_id, asset.cell_size);
                let mut edit = EditPlan::new(volume_id);
                for cell in &cells {
                    edit.set(cell.cell, cell.material);
                }
                volume
                    .apply_edit(&edit)
                    .expect("decoded bounded asset forms a valid voxel edit");
                volume
            },
            pose,
            [0.0; 3],
            [0.0; 3],
            |material| densities.get(&material).copied().unwrap_or(1.0),
            0,
        )
        .map_err(|error| format!("could not spawn imported voxel asset: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_valid_and_ids_are_fixed() {
        let m = manifest();
        assert_eq!(m.get(materials::WOOD).unwrap().name, "wood");
        assert!(m.get(MaterialId(99)).is_none());
    }

    #[test]
    fn a_tool_use_becomes_a_matching_edit_intent() {
        let intent = Tool::DIG.intent(
            RequestId(7),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            10,
            4,
            2,
        );
        assert!(matches!(intent.kind, EditKind::Cut));
        assert_eq!(intent.brush.radius_units(), 2 * BRUSH_UNIT);
        assert!(matches!(intent.target, EditTarget::Terrain));
    }
}
