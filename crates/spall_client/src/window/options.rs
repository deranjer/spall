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
    /// Game-owned drawing of player bodies as lit cubes each frame. Without it
    /// other players are not drawn and the third-person toggle does nothing.
    pub player_view: Option<std::sync::Arc<dyn PlayerView>>,
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

/// Turns replicated creatures into cubes to draw this frame.
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
