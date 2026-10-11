//! Inputs and errors for world generation.

use spall_core::{MaterialId, VolumeId};
use thiserror::Error;

/// Generation version of [`Preset::Showcase`]. Bumped whenever the same
/// `(spec, seed)` would produce different voxels. A saved world must never be
/// regenerated over with a different version: modified bricks (including
/// all-air tombstones) win over generated ones. Versions are per preset
/// ([`Preset::gen_version`]) so adding a preset never invalidates saves of
/// another.
pub const GEN_VERSION: u32 = 3;
/// Generation version of [`Preset::Natural`].
pub const NATURAL_GEN_VERSION: u32 = 4;

/// Edge of a brick in cells (mirrors `spall_core::BRICK_EDGE`).
pub const BRICK: i64 = 32;
/// Vertical extent of a [`Preset::Showcase`] world, in bricks (96 m at 0.25 m
/// cells). Presets choose their own height: see [`Preset::height_bricks`].
pub const SHOWCASE_HEIGHT_BRICKS: i64 = 12;
/// Tallest supported world, in bricks (128 m): the 1 km presets' height.
pub const MAX_HEIGHT_BRICKS: i64 = 16;
/// Thickness of the solid bedrock wall around the arena, in cells: exactly one
/// brick, so the wall is made of cheap uniform bricks. It keeps players inside
/// the resident arena; generated content lives strictly inside it.
pub const BORDER_CELLS: i64 = BRICK;
/// Water surface: cells with `y <= SEA_LEVEL` above the terrain are water.
pub const SEA_LEVEL: i32 = 128;
/// Smallest and largest supported arena edge in cells.
pub const MIN_SIZE_CELLS: u32 = 128;
pub const MAX_SIZE_CELLS: u32 = 4096;
/// Default showcase arena: 1024 cells = 256 m.
pub const SHOWCASE_SIZE_CELLS: u32 = 1024;

/// Which large-scale layout to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// A bounded arena with every feature (mountains, hills, swamp basin with
    /// lake and river, desert, caves) placed so each is a short walk apart.
    Showcase,
    /// A world whose layout follows cause and effect: ranges, erosion, rivers
    /// and lakes from hydrology, biomes from climate. 128 m tall.
    Natural,
}

impl Preset {
    pub fn name(self) -> &'static str {
        match self {
            Preset::Showcase => "showcase",
            Preset::Natural => "natural",
        }
    }

    /// The generation version stamped on worlds of this preset.
    pub fn gen_version(self) -> u32 {
        match self {
            Preset::Showcase => GEN_VERSION,
            Preset::Natural => NATURAL_GEN_VERSION,
        }
    }

    /// Vertical extent in bricks. Changing a preset's height changes its world:
    /// bump `GEN_VERSION` (or add a new preset) rather than editing a shipped one.
    pub fn height_bricks(self) -> i64 {
        match self {
            Preset::Showcase => SHOWCASE_HEIGHT_BRICKS,
            Preset::Natural => MAX_HEIGHT_BRICKS,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "showcase" => Some(Preset::Showcase),
            _ => None,
        }
    }
}

/// The game's material ids for each terrain role. The engine never names game
/// materials: the game hands these in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldgenPalette {
    pub bedrock: MaterialId,
    pub deep_stone: MaterialId,
    pub stone: MaterialId,
    pub slate: MaterialId,
    pub dirt: MaterialId,
    pub grass: MaterialId,
    pub sand: MaterialId,
    pub sandstone: MaterialId,
    pub mud: MaterialId,
    pub moss: MaterialId,
    pub gravel: MaterialId,
    pub clay: MaterialId,
    pub snow: MaterialId,
}

impl WorldgenPalette {
    fn all(&self) -> [MaterialId; 13] {
        [
            self.bedrock,
            self.deep_stone,
            self.stone,
            self.slate,
            self.dirt,
            self.grass,
            self.sand,
            self.sandstone,
            self.mud,
            self.moss,
            self.gravel,
            self.clay,
            self.snow,
        ]
    }

    /// A palette of consecutive ids starting at `first` (tests and previews).
    pub fn sequential(first: u16) -> Self {
        let m = |i: u16| MaterialId(first + i);
        Self {
            bedrock: m(0),
            deep_stone: m(1),
            stone: m(2),
            slate: m(3),
            dirt: m(4),
            grass: m(5),
            sand: m(6),
            sandstone: m(7),
            mud: m(8),
            moss: m(9),
            gravel: m(10),
            clay: m(11),
            snow: m(12),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldGenSpec {
    pub preset: Preset,
    pub seed: u64,
    /// Arena edge in cells; a multiple of 32 (one brick).
    pub size_cells: u32,
    pub palette: WorldgenPalette,
    pub volume_id: VolumeId,
    /// Vertical extent in bricks; the preset's own unless overridden with
    /// [`Self::with_height_bricks`].
    pub height_bricks: i64,
}

impl WorldGenSpec {
    pub fn new(preset: Preset, seed: u64, size_cells: u32, palette: WorldgenPalette) -> Self {
        Self {
            preset,
            seed,
            size_cells,
            palette,
            volume_id: VolumeId::new(1).expect("nonzero volume id"),
            height_bricks: preset.height_bricks(),
        }
    }

    /// Overrides the world height (measurement probes and tests).
    #[must_use]
    pub fn with_height_bricks(mut self, height_bricks: i64) -> Self {
        self.height_bricks = height_bricks;
        self
    }

    /// Vertical extent in cells. Cell `y = 0` is the bottom of the bedrock brick.
    pub fn height_cells(&self) -> i64 {
        self.height_bricks * BRICK
    }

    /// Highest solid cell of the border wall: every brick below the top brick.
    /// Above every peak, so the wall cannot be walked or jumped over.
    pub fn wall_top(&self) -> i64 {
        (self.height_bricks - 1) * BRICK - 1
    }

    pub(crate) fn validate(&self) -> Result<(), GenError> {
        let size = self.size_cells;
        if !(MIN_SIZE_CELLS..=MAX_SIZE_CELLS).contains(&size) || !size.is_multiple_of(BRICK as u32)
        {
            return Err(GenError::InvalidSize(size));
        }
        if !(1..=MAX_HEIGHT_BRICKS).contains(&self.height_bricks) {
            return Err(GenError::InvalidHeight(self.height_bricks));
        }
        let ids = self.palette.all();
        if ids.contains(&MaterialId::AIR) {
            return Err(GenError::PaletteContainsAir);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum GenError {
    #[error("arena size {0} cells must be a multiple of 32 in {MIN_SIZE_CELLS}..={MAX_SIZE_CELLS}")]
    InvalidSize(u32),
    #[error("world height {0} bricks must be in 1..={MAX_HEIGHT_BRICKS}")]
    InvalidHeight(i64),
    #[error("palette maps a terrain role to the air material")]
    PaletteContainsAir,
    #[error("water covers {measured} {what}, over the {budget} budget; nothing was truncated")]
    WaterBudget {
        /// What was counted: `"regions"` (32x32 column regions) or `"cells"`.
        what: &'static str,
        measured: u64,
        budget: u64,
    },
    #[error("no valid dry spawn with headroom was found")]
    NoSpawn,
    #[error("volume rejected a generated brick: {0}")]
    Volume(#[from] spall_voxel::volume::AccessError),
}
