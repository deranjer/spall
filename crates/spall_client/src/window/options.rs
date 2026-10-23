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

/// One page of the blueprint book: a recipe and the pattern its raw
/// materials must be laid out in on the crafting cloth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    /// The game's recipe, sent in the craft request.
    pub recipe_id: u16,
    /// What the blueprint makes, and how many.
    pub output: (u16, u32),
    /// Rows from the top, at most 3 by 3: the item for each square, or
    /// `None` for a square left bare. It may sit anywhere on the cloth.
    pub pattern: Vec<Vec<Option<u16>>>,
}

impl Blueprint {
    /// The pattern's width and height in squares.
    pub fn size(&self) -> (usize, usize) {
        let width = self.pattern.iter().map(Vec::len).max().unwrap_or(0);
        (width, self.pattern.len())
    }

    /// The pattern's item at column `x`, row `y`.
    pub fn at(&self, x: usize, y: usize) -> Option<u16> {
        self.pattern.get(y)?.get(x).copied().flatten()
    }

    /// Each raw material the pattern uses and how many, in first-use order.
    pub fn needs(&self) -> Vec<(u16, u32)> {
        let mut needs: Vec<(u16, u32)> = Vec::new();
        for item in self.pattern.iter().flatten().flatten() {
            match needs.iter_mut().find(|(id, _)| id == item) {
                Some((_, count)) => *count += 1,
                None => needs.push((*item, 1)),
            }
        }
        needs
    }
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
