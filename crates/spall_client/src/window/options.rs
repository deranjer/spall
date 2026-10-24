use super::*;

/// Interactive window options beyond the network and content configuration.
#[derive(Debug, Clone, Default)]
pub struct InteractiveOptions {
    /// Game door palette and dimensions for proximity prompts.
    pub door: Option<spall_voxel::doors::DoorSpec>,
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
    /// Game-owned drawing of player bodies as lit cubes each frame. Without it
    /// other players are not drawn and the third-person toggle does nothing.
    pub player_view: Option<std::sync::Arc<dyn PlayerView>>,
}

/// What the game's items look like laid out on the pack's tarp. The engine
/// owns opening the pack, the tarp, the layout and fetching the inventory;
/// the game owns item names and icons.
pub trait PackView: Send + Sync + std::fmt::Debug {
    /// The game's recipe catalogue version, sent with inventory requests.
    fn catalog_version(&self) -> u32;
    /// Display name for a stable item ID; `None` for an item this build
    /// does not know (it is still laid out, as "Unknown item").
    fn item_name(&self, item_id: u16) -> Option<&str>;
    /// The item's icon as PNG bytes, drawn about 100 logical pixels square
    /// on the tarp: an item on transparency, lit from the top left. `None`
    /// draws a plain marker.
    fn item_icon_png(&self, item_id: u16) -> Option<&'static [u8]>;
    /// The blueprints in the book (B), in page order. Each must match one of
    /// the game's server-side recipes.
    fn blueprints(&self) -> &[Blueprint];
}

/// One page of the blueprint book: a recipe, and where its raw materials
/// must be laid on the crafting cloth. Each slot takes one item; a material
/// dropped near a slot that takes it snaps into place.
#[derive(Debug, Clone, PartialEq)]
pub struct Blueprint {
    /// The game's recipe, sent in the craft request.
    pub recipe_id: u16,
    /// What the blueprint makes, and how many.
    pub output: (u16, u32),
    /// How the cloth is drawn: looking down at it, or from the side with a
    /// ground line (for things that stand or stack).
    pub view: BlueprintView,
    pub slots: Vec<BlueprintSlot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueprintView {
    Overhead,
    /// From the side: the ground runs across the cloth at [`SIDE_GROUND`].
    Side,
}

/// Where the ground line runs in a side view, as a share of the cloth's
/// height from the top.
pub const SIDE_GROUND: f32 = 0.74;

/// One place on the cloth for one item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlueprintSlot {
    pub item: u16,
    /// The slot's centre: across and down the cloth, each 0 to 1.
    pub at: [f32; 2],
    /// The item's drawn size as a share of the cloth's width.
    pub size: f32,
}

impl Blueprint {
    /// Each raw material the blueprint uses and how many, in first-use order.
    pub fn needs(&self) -> Vec<(u16, u32)> {
        let mut needs: Vec<(u16, u32)> = Vec::new();
        for slot in &self.slots {
            match needs.iter_mut().find(|(id, _)| *id == slot.item) {
                Some((_, count)) => *count += 1,
                None => needs.push((slot.item, 1)),
            }
        }
        needs
    }
}

/// One player capsule to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerBody {
    /// The player's entity id, or `0` for the local player.
    pub entity: u64,
    /// The capsule's lowest point, metres.
    pub feet_m: [f64; 3],
    pub velocity_m_s: [f32; 3],
    /// Whether the player swims (the same test the movement kernel uses).
    pub in_water: bool,
    /// Heading of the local player's look direction, as `PlayerInput::yaw`.
    /// `None` for remote players, whose facing follows their movement.
    pub look_yaw: Option<f32>,
}

/// Turns player capsules into cubes to draw this frame.
pub trait PlayerView: Send + Sync + std::fmt::Debug {
    /// Manifest materials re-coloured to an sRGB tint. The window appends one
    /// render-table entry per item, in order, after its debug-overlay entry;
    /// item `i` is material index `tint_base + i` in [`Self::instances`].
    fn tints(&self) -> Vec<(spall_core::MaterialId, [u8; 3])>;

    fn instances(&self, players: &[PlayerBody], tint_base: u32, now: Instant) -> Vec<CubeInstance>;
}

/// What this frame's camera can see, for skipping creatures before posing them.
/// Conservative: a cone that contains the whole view frustum, its apex pulled
/// back so small camera moves within the frame stay inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreatureCull {
    /// Cone apex, world metres.
    pub apex_m: [f64; 3],
    /// Unit view direction.
    pub forward: [f32; 3],
    /// Half-angle of the cone, radians.
    pub half_angle_rad: f32,
    /// The camera's own position, world metres.
    pub eye_m: [f64; 3],
    /// Horizontal distance from `eye_m` past which nothing is drawn, metres.
    pub max_distance_m: f32,
}

/// Turns replicated creatures into cubes to draw this frame.
/// The engine owns replication; the game owns what a species looks like.
pub trait CreatureView: Send + Sync + std::fmt::Debug {
    /// Manifest materials re-coloured to an sRGB tint, as for
    /// [`PlayerView::tints`]: item `i` is material index `tint_base + i` in
    /// [`Self::instances`].
    fn tints(&self) -> Vec<(spall_core::MaterialId, [u8; 3])>;

    /// `tick` is the estimated present server tick, fractional, for animation.
    /// `cull` is `None` when the camera is not known yet: draw everything.
    fn instances(
        &self,
        creatures: &crate::replica_creatures::ReplicaCreatures,
        tick: f64,
        tint_base: u32,
        now: Instant,
        cull: Option<&CreatureCull>,
    ) -> Vec<CubeInstance>;
}
