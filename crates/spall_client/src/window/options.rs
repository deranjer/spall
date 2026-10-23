use super::*;

/// Interactive window options beyond the network and content configuration.
#[derive(Debug, Clone, Default)]
pub struct InteractiveOptions {
    /// Present without vsync (rendering-throughput measurement only).
    pub uncapped: bool,
    /// A scripted tour: each step runs, then the frame is saved to
    /// `shots_dir`; the window closes after the last step unless it is `play`.
    /// Empty = normal play.
    pub shots: Vec<ShotStep>,
    pub shots_dir: PathBuf,
    /// Whether to advance the world clock from authoritative server ticks.
    /// `None` uses the normal cycle-on default.
    pub day_night_cycle: Option<bool>,
    /// How far (metres) terrain and water are drawn; `None` keeps the
    /// default. Clamped to [`MIN_VIEW_DISTANCE_M`]..=[`MAX_VIEW_DISTANCE_M`].
    pub view_distance_m: Option<f32>,
    /// Game-owned ambience catalogue indexed by stable biome code.
    pub biome_audio: BiomeAudioConfig,
    /// Game-owned drawing of replicated creatures as lit cubes each frame.
    pub creature_view: Option<std::sync::Arc<dyn CreatureView>>,
    /// Game-owned look of the inventory laid out on the pack's tarp (Tab).
    /// `None` leaves Tab unbound.
    pub pack_view: Option<std::sync::Arc<dyn PackView>>,
}

/// What the game's items look like laid out on the pack's tarp. The engine
/// owns opening the pack, the kneel, the layout and fetching the inventory;
/// the game owns item names and models.
pub trait PackView: Send + Sync + std::fmt::Debug {
    /// The game's recipe catalogue version, sent with inventory requests.
    fn catalog_version(&self) -> u32;
    /// Display name for a stable item ID; `None` for an item this build
    /// does not know (it is still laid out, as a plain crate).
    fn item_name(&self, item_id: u16) -> Option<&str>;
    /// The item's cubes in item-local metres: `+X` to the player's right,
    /// `-Z` away from them, resting on `y = 0` and centred on X and Z.
    /// Empty for an unknown item.
    fn item_model(&self, item_id: u16) -> &[CubeInstance];
    /// Material of the tarp the items lie on.
    fn tarp_material(&self) -> u32;
    /// Material of the twine and the stones pinning the tarp's corners.
    fn trim_materials(&self) -> [u32; 2];
    /// How far a camp reaches from a site of this kind, for the site kinds
    /// that found a camp.
    fn camp_radius_m(&self, site_kind: u16) -> Option<f32>;
    /// The build tool that founds a camp, offered on the tarp when the
    /// player has none.
    fn camp_tool(&self) -> Option<CampTool>;
}

/// The game's camp-founding build tool, as the pack offers it.
#[derive(Debug, Clone, Copy)]
pub struct CampTool {
    pub tool: u16,
    /// Item IDs and counts the build costs.
    pub cost: &'static [(u16, u32)],
    /// The cost as the player reads it ("4 stones").
    pub cost_text: &'static str,
}

/// Turns replicated creature ownership records into cubes to draw this frame.
/// The engine owns replication; the game owns what a species looks like.
pub trait CreatureView: Send + Sync + std::fmt::Debug {
    /// `tick` is the estimated present server tick, fractional, for animation.
    fn instances(
        &self,
        creatures: &crate::replica_creatures::ReplicaCreatures,
        tick: f64,
        now: Instant,
    ) -> Vec<CubeInstance>;
}
