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
