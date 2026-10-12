//! The sandbox's vegetation catalogue. These are gameplay species, not engine
//! materials or a claim to a botanically complete simulation.
//!
//! Every biome of the natural world has its own set: living plants (trees,
//! shrubs, reeds, flowers, ferns) and stateless ground cover. A species lists
//! the biomes it grows in, the soils (surface materials) it roots in and the
//! moisture and temperature it tolerates, so the mix changes from biome to
//! biome and, within a biome, with wetness: cover species mostly set a minimum
//! wetness, so wetter ground admits more kinds, and each is denser where it is
//! wetter. IDs are stable save/wire values: never renumber or reuse one.
use spall_ecology::cover::CoverSpecies;
use spall_ecology::living::Species;
use spall_worldgen::Biome;

/// The bit for a set of biomes.
fn mask(biomes: &[Biome]) -> u16 {
    biomes.iter().fold(0, |m, b| m | 1 << *b as u16)
}

// Palette colours (see `spall_ecology::living::PALETTE`).
const PINE: u8 = 2;
const LIGHT: u8 = 3;
const GREEN: u8 = 1;
const AMBER: u8 = 4;
const RED: u8 = 6;
const BROWN: u8 = 7;
const STRAW: u8 = 8;
const SAGE: u8 = 13;
const TEAL: u8 = 14;
const DRY: u8 = 15;
const YELLOW: u8 = 16;
const WHITE: u8 = 17;
const VIOLET: u8 = 18;
const MAROON: u8 = 19;
const MOSS: u8 = 20;
const LICHEN: u8 = 21;
const GREY: u8 = 22;
const DARK_BROWN: u8 = 23;
const TAN: u8 = 24;
const DEEP: u8 = 25;
const RUST: u8 = 26;
const ROSE: u8 = 27;
const OLIVE: u8 = 28;
const DUSK: u8 = 29;
const WHEAT: u8 = 30;
const DARK_PINE: u8 = 31;

/// Display names by species id (living species and cover share the space).
pub fn name(id: u16) -> &'static str {
    match id {
        1 => "Oak",
        2 => "Maple",
        3 => "Birch",
        4 => "Aspen",
        5 => "Scots pine",
        6 => "Spruce",
        7 => "Larch",
        8 => "Willow",
        9 => "Cypress",
        10 => "Acacia",
        11 => "Meadow grass",
        12 => "Fescue",
        13 => "Alpine tussock",
        14 => "Sedge",
        15 => "Reed",
        16 => "Fern",
        17 => "Clover",
        18 => "Wildflower",
        19 => "Desert bunchgrass",
        20 => "Sage scrub",
        21 => "Beech",
        22 => "Linden",
        23 => "Alder",
        24 => "Poplar",
        25 => "Fir",
        26 => "Palm",
        28 => "Juniper",
        29 => "Krummholz pine",
        30 => "Baobab",
        31 => "Hawthorn",
        32 => "Tamarisk",
        33 => "Heather",
        34 => "Bramble",
        35 => "Cattail",
        36 => "Marsh marigold",
        37 => "Foxglove",
        38 => "Daisy",
        39 => "Bracken",
        40 => "Saxifrage",
        41 => "Rock lichen",
        42 => "Prickly pear",
        43 => "Agave",
        44 => "Yucca",
        46 => "Beach morning glory",
        47 => "Beach rose",
        48 => "Toadstools",
        50 => "Short turf",
        51 => "Prairie grass",
        52 => "Feather grass",
        53 => "Marsh grass",
        54 => "Moss mat",
        55 => "Dune grass",
        56 => "Sea thrift",
        57 => "Thorn tuft",
        58 => "Lichen mat",
        59 => "Woodland sedge",
        60 => "Wild thyme",
        61 => "Horsetail",
        63 => "Frost moss",
        64 => "Lingonberry",
        65 => "Wood sorrel",
        66 => "Pennywort",
        70 => "Kelp",
        71 => "Staghorn coral",
        72 => "Sea fan",
        73 => "Elkhorn coral",
        74 => "Brain coral",
        75 => "Barrel sponge",
        76 => "Hornwort",
        77 => "Eelweed",
        80 => "Seagrass",
        81 => "Eelgrass",
        82 => "Sea anemone",
        83 => "Algae mat",
        84 => "Coral rubble",
        85 => "Sea lettuce",
        86 => "Pondweed",
        87 => "Water milfoil",
        88 => "Pond algae",
        _ => "Unknown",
    }
}

/// Soil sets by terrain type.
struct Soils {
    lawn: [u16; 4],
    upland: [u16; 4],
    dry_land: [u16; 4],
    wetland: [u16; 4],
    arid: [u16; 4],
    shore: [u16; 4],
    rocky: [u16; 4],
}

fn soils() -> Soils {
    let p = crate::game::terrain_palette();
    let (grass, dirt, moss, mud) = (p.grass.0, p.dirt.0, p.moss.0, p.mud.0);
    let (gravel, clay, sand, sandstone) = (p.gravel.0, p.clay.0, p.sand.0, p.sandstone.0);
    let (slate, stone) = (p.slate.0, p.stone.0);
    Soils {
        lawn: [grass, dirt, moss, mud],
        upland: [grass, dirt, moss, gravel],
        dry_land: [grass, dirt, gravel, clay],
        wetland: [mud, clay, moss, grass],
        arid: [sand, dirt, clay, gravel],
        shore: [sand, gravel, sandstone, dirt],
        rocky: [gravel, slate, stone, dirt],
    }
}

type TreeRow = (
    u16,
    &'static [Biome],
    [u16; 4],
    [u8; 2],
    u8,
    u16,
    u16,
    u16,
    bool,
    [u8; 4],
);
type GroundRow = (
    u16,
    &'static [Biome],
    [u16; 4],
    [u8; 2],
    u8,
    u16,
    u16,
    u16,
    bool,
    [u8; 4],
    u8,
);
type CoverRow = (
    u16,
    &'static [Biome],
    [u16; 4],
    [u8; 2],
    [i8; 2],
    u8,
    u16,
    [u8; 4],
    bool,
    u8,
);

/// The catalogue of living species: trees, then ground plants.
pub fn catalogue() -> Vec<Species> {
    use Biome::*;
    let s = soils();
    // (id, biomes, soils, moisture, form, height, spread, spacing, evergreen, colours)
    // Forms: 0 broad crown, 1 cone, 2 column, 3 weeping, 4 umbrella, 5 fan.
    // Height is in quarter-cells.
    let trees: [TreeRow; 21] = [
        (
            1,
            &[Meadow, TemperateForest, Riparian],
            s.lawn,
            [60, 230],
            0,
            28,
            9,
            24,
            false,
            [LIGHT, GREEN, AMBER, BROWN],
        ),
        (
            2,
            &[TemperateForest, Meadow],
            s.lawn,
            [80, 240],
            0,
            24,
            7,
            22,
            false,
            [GREEN, GREEN, RED, BROWN],
        ),
        (
            3,
            &[Meadow, TemperateForest, BorealForest, Alpine],
            s.upland,
            [50, 220],
            2,
            30,
            5,
            18,
            false,
            [LIGHT, GREEN, AMBER, STRAW],
        ),
        (
            4,
            &[BorealForest, TemperateForest, Alpine, Steppe],
            s.upland,
            [40, 220],
            2,
            34,
            4,
            16,
            false,
            [LIGHT, GREEN, YELLOW, STRAW],
        ),
        (
            5,
            &[BorealForest, Alpine, Steppe],
            s.upland,
            [30, 200],
            4,
            36,
            6,
            24,
            true,
            [PINE, PINE, PINE, PINE],
        ),
        (
            6,
            &[BorealForest, Alpine],
            s.upland,
            [60, 255],
            1,
            40,
            6,
            24,
            true,
            [DARK_PINE, DARK_PINE, DARK_PINE, DARK_PINE],
        ),
        (
            7,
            &[BorealForest, Alpine, Scree],
            s.upland,
            [40, 220],
            1,
            34,
            5,
            22,
            false,
            [SAGE, GREEN, YELLOW, STRAW],
        ),
        (
            8,
            &[Swamp, Riparian],
            s.wetland,
            [130, 255],
            3,
            24,
            9,
            26,
            false,
            [SAGE, GREEN, AMBER, BROWN],
        ),
        (
            9,
            &[Swamp],
            s.wetland,
            [150, 255],
            1,
            32,
            5,
            22,
            true,
            [TEAL, PINE, PINE, TEAL],
        ),
        (
            10,
            &[Desert, Steppe, Beach],
            s.arid,
            [0, 90],
            4,
            20,
            9,
            30,
            true,
            [LIGHT, LIGHT, BROWN, LIGHT],
        ),
        (
            21,
            &[TemperateForest],
            s.lawn,
            [100, 255],
            0,
            30,
            8,
            22,
            false,
            [MOSS, DEEP, RUST, BROWN],
        ),
        (
            22,
            &[TemperateForest, Meadow],
            s.lawn,
            [80, 240],
            0,
            26,
            8,
            24,
            false,
            [LIGHT, MOSS, YELLOW, BROWN],
        ),
        (
            23,
            &[Riparian, Swamp, BorealForest],
            s.wetland,
            [120, 255],
            2,
            22,
            5,
            16,
            false,
            [SAGE, DEEP, BROWN, BROWN],
        ),
        (
            24,
            &[Riparian, Meadow],
            s.lawn,
            [70, 240],
            2,
            36,
            4,
            16,
            false,
            [LIGHT, GREEN, YELLOW, STRAW],
        ),
        (
            25,
            &[BorealForest, Alpine],
            s.upland,
            [70, 255],
            1,
            38,
            5,
            22,
            true,
            [DEEP, DEEP, DEEP, DEEP],
        ),
        (
            26,
            &[Beach],
            s.shore,
            [0, 255],
            4,
            24,
            5,
            20,
            true,
            [MOSS, MOSS, MOSS, MOSS],
        ),
        (
            28,
            &[Steppe, Scree, Alpine],
            s.rocky,
            [0, 150],
            1,
            14,
            3,
            14,
            true,
            [LICHEN, LICHEN, LICHEN, LICHEN],
        ),
        (
            29,
            &[Scree, Snow, Alpine],
            s.rocky,
            [0, 255],
            5,
            10,
            6,
            18,
            true,
            [DARK_PINE, DARK_PINE, DARK_PINE, DARK_PINE],
        ),
        (
            30,
            &[Steppe, Desert],
            s.arid,
            [0, 100],
            4,
            22,
            10,
            34,
            false,
            [LIGHT, GREEN, TAN, STRAW],
        ),
        (
            31,
            &[Meadow, Steppe],
            s.upland,
            [40, 200],
            0,
            14,
            5,
            14,
            false,
            [LIGHT, GREEN, RED, BROWN],
        ),
        (
            32,
            &[Desert, Beach, Riparian],
            s.arid,
            [0, 255],
            3,
            14,
            6,
            16,
            true,
            [SAGE, SAGE, SAGE, SAGE],
        ),
    ];
    let mut out: Vec<_> = trees
        .into_iter()
        .map(
            |(id, biomes, soils, moisture, form, height, spread, spacing, evergreen, colours)| {
                Species {
                    id,
                    tree: true,
                    biomes: mask(biomes),
                    soils,
                    moisture,
                    depth: [0, 0],
                    wood: 124 + (id - 1) % 12,
                    trunk_width: if [1, 5, 8, 9, 21, 30].contains(&id) {
                        2
                    } else {
                        1
                    },
                    form,
                    height,
                    spread,
                    spacing,
                    evergreen,
                    colours,
                    flower: 10,
                    growth_ms: 5_000 + u64::from(id) * 400,
                    seed_ms: 90_000 + u64::from(id) * 7000,
                }
            },
        )
        .collect();

    // (id, biomes, soils, moisture, form, height, spread, spacing, evergreen, colours, flower)
    // Forms: 7 reed, 8 fern, 10 flowering stalk, 11 rosette, 12 cushion, 13 scrub.
    let ground: [GroundRow; 19] = [
        (
            15,
            &[Swamp, Riparian],
            s.wetland,
            [150, 255],
            7,
            7,
            3,
            6,
            false,
            [GREEN, GREEN, BROWN, STRAW],
            10,
        ),
        (
            16,
            &[TemperateForest, BorealForest, Swamp, Meadow],
            s.lawn,
            [90, 255],
            8,
            3,
            5,
            6,
            false,
            [GREEN, 0, 5, BROWN],
            10,
        ),
        (
            18,
            &[Meadow, Steppe, Alpine],
            s.upland,
            [40, 230],
            10,
            3,
            3,
            5,
            false,
            [LIGHT, GREEN, BROWN, STRAW],
            11,
        ),
        (
            20,
            &[Desert, Steppe],
            s.arid,
            [0, 110],
            13,
            4,
            6,
            10,
            true,
            [TEAL, TEAL, 8, 8],
            12,
        ),
        (
            33,
            &[Alpine, BorealForest, Scree, Steppe],
            s.rocky,
            [20, 200],
            13,
            2,
            4,
            8,
            true,
            [DUSK, DUSK, DARK_BROWN, DARK_BROWN],
            10,
        ),
        (
            34,
            &[Meadow, TemperateForest, Riparian],
            s.lawn,
            [60, 255],
            13,
            4,
            5,
            7,
            false,
            [GREEN, DEEP, MAROON, BROWN],
            10,
        ),
        (
            35,
            &[Swamp, Riparian],
            s.wetland,
            [160, 255],
            7,
            8,
            3,
            5,
            false,
            [MOSS, GREEN, OLIVE, BROWN],
            10,
        ),
        (
            36,
            &[Swamp, Riparian],
            s.wetland,
            [140, 255],
            10,
            2,
            3,
            5,
            false,
            [MOSS, MOSS, BROWN, BROWN],
            YELLOW,
        ),
        (
            37,
            &[Meadow, Alpine, BorealForest, TemperateForest],
            s.upland,
            [70, 230],
            10,
            5,
            3,
            6,
            false,
            [LIGHT, GREEN, BROWN, STRAW],
            VIOLET,
        ),
        (
            38,
            &[Meadow, Steppe, Riparian],
            s.upland,
            [40, 200],
            10,
            2,
            3,
            4,
            false,
            [LIGHT, GREEN, STRAW, STRAW],
            WHITE,
        ),
        (
            39,
            &[BorealForest, TemperateForest, Alpine],
            s.lawn,
            [100, 255],
            8,
            5,
            6,
            6,
            false,
            [MOSS, DEEP, RUST, BROWN],
            10,
        ),
        (
            40,
            &[Alpine, Scree, Snow],
            s.rocky,
            [20, 255],
            12,
            2,
            3,
            4,
            true,
            [MOSS, ROSE, LICHEN, LICHEN],
            10,
        ),
        (
            41,
            &[Scree, Snow, Alpine],
            s.rocky,
            [0, 255],
            12,
            1,
            3,
            3,
            true,
            [GREY, GREY, GREY, GREY],
            10,
        ),
        (
            42,
            &[Desert, Beach],
            s.arid,
            [0, 70],
            12,
            3,
            3,
            8,
            true,
            [MOSS, MOSS, RUST, RUST],
            10,
        ),
        (
            43,
            &[Desert, Steppe],
            s.arid,
            [0, 90],
            11,
            3,
            4,
            9,
            true,
            [LICHEN, LICHEN, LICHEN, LICHEN],
            10,
        ),
        (
            44,
            &[Desert, Steppe],
            s.arid,
            [0, 90],
            13,
            5,
            4,
            12,
            true,
            [LICHEN, LICHEN, TAN, TAN],
            10,
        ),
        (
            46,
            &[Beach],
            s.shore,
            [20, 255],
            10,
            2,
            3,
            5,
            false,
            [MOSS, MOSS, STRAW, STRAW],
            ROSE,
        ),
        (
            47,
            &[Beach],
            s.shore,
            [20, 255],
            13,
            3,
            4,
            6,
            true,
            [MOSS, DEEP, RUST, RUST],
            10,
        ),
        (
            48,
            &[TemperateForest, BorealForest],
            s.lawn,
            [100, 255],
            12,
            1,
            3,
            4,
            false,
            [BROWN, TAN, BROWN, DARK_BROWN],
            10,
        ),
    ];
    out.extend(ground.into_iter().map(
        |(
            id,
            biomes,
            soils,
            moisture,
            form,
            height,
            spread,
            spacing,
            evergreen,
            colours,
            flower,
        )| {
            Species {
                id,
                tree: false,
                biomes: mask(biomes),
                soils,
                moisture,
                depth: [0, 0],
                wood: 0,
                trunk_width: 0,
                form,
                height,
                spread,
                spacing,
                evergreen,
                colours,
                flower,
                growth_ms: 1000,
                seed_ms: 45_000,
            }
        },
    ));
    out.extend(aquatic_catalogue());
    out
}

/// Plants that live under water: kelp, corals, sponges and freshwater weeds.
/// Interactive like the other living plants (they grow, seed, can be
/// harvested and die with their seabed); the underwater carpet is in
/// [`cover_catalogue`]. Depths are in cells (four to the metre).
fn aquatic_catalogue() -> Vec<Species> {
    use Biome::*;
    let p = crate::game::terrain_palette();
    let seabed = [p.sand.0, p.gravel.0, p.clay.0, p.sandstone.0];
    let lakebed = [p.gravel.0, p.mud.0, p.clay.0, p.dirt.0];
    // (id, biomes, soils, depth, form, height, spread, spacing, colours, tip)
    // Forms: 12 cushion, 14 kelp, 15 branching coral, 16 sea fan, 17 barrel sponge.
    type Row = (
        u16,
        &'static [Biome],
        [u16; 4],
        [u8; 2],
        u8,
        u16,
        u16,
        u16,
        [u8; 4],
        u8,
    );
    let rows: [Row; 8] = [
        (
            70,
            &[Ocean],
            seabed,
            [6, 80],
            14,
            16,
            3,
            7,
            [OLIVE, MOSS, OLIVE, MOSS],
            BROWN,
        ),
        (
            71,
            &[Ocean],
            seabed,
            [3, 40],
            15,
            6,
            4,
            6,
            [AMBER, AMBER, AMBER, AMBER],
            YELLOW,
        ),
        (
            72,
            &[Ocean],
            seabed,
            [4, 60],
            16,
            5,
            4,
            8,
            [ROSE, ROSE, ROSE, ROSE],
            MAROON,
        ),
        (
            73,
            &[Ocean],
            seabed,
            [2, 24],
            15,
            5,
            4,
            7,
            [VIOLET, VIOLET, VIOLET, VIOLET],
            ROSE,
        ),
        (
            74,
            &[Ocean],
            seabed,
            [2, 30],
            12,
            2,
            3,
            5,
            [LICHEN, TAN, LICHEN, TAN],
            ROSE,
        ),
        (
            75,
            &[Ocean],
            seabed,
            [12, 120],
            17,
            4,
            3,
            10,
            [MAROON, MAROON, MAROON, MAROON],
            DARK_BROWN,
        ),
        (
            76,
            &[Riparian],
            lakebed,
            [2, 24],
            14,
            5,
            3,
            5,
            [MOSS, MOSS, MOSS, MOSS],
            OLIVE,
        ),
        (
            77,
            &[Riparian, Swamp],
            lakebed,
            [3, 30],
            14,
            8,
            3,
            6,
            [GREEN, GREEN, OLIVE, OLIVE],
            MOSS,
        ),
    ];
    rows.into_iter()
        .map(
            |(id, biomes, soils, depth, form, height, spread, spacing, colours, tip)| Species {
                id,
                tree: false,
                biomes: mask(biomes),
                soils,
                moisture: [0, 255],
                depth,
                wood: 0,
                trunk_width: 0,
                form,
                height,
                spread,
                spacing,
                evergreen: true,
                colours,
                flower: tip,
                growth_ms: 1000,
                seed_ms: 60_000,
            },
        )
        .collect()
}

/// The sandbox's stateless ground cover: the carpet. Forms: 6 blades, 9 sedge,
/// 11 rosette, 12 cushion. Most species set only a minimum wetness, so the
/// wetter the ground the more kinds it carries, and every kind is denser where
/// it is wetter.
pub fn cover_catalogue() -> Vec<CoverSpecies> {
    use Biome::*;
    let s = soils();
    let (grass, dirt, moss, mud) = (s.lawn[0], s.lawn[1], s.lawn[2], s.lawn[3]);
    let (gravel, slate) = (s.upland[3], s.rocky[1]);
    // (id, biomes, soils, moisture, temperature, form, height, colours, evergreen, density)
    let rows: [CoverRow; 22] = [
        (
            11,
            &[Meadow, TemperateForest, Riparian, Steppe],
            s.lawn,
            [50, 255],
            [-12, 42],
            6,
            2,
            [LIGHT, GREEN, BROWN, STRAW],
            true,
            120,
        ),
        (
            12,
            &[Alpine, Meadow, BorealForest, Steppe, Scree],
            s.upland,
            [30, 255],
            [-35, 28],
            6,
            3,
            [SAGE, GREEN, STRAW, STRAW],
            true,
            110,
        ),
        (
            13,
            &[Alpine, Scree, BorealForest],
            [grass, dirt, gravel, slate],
            [40, 255],
            [-40, 22],
            12,
            2,
            [SAGE, SAGE, BROWN, STRAW],
            true,
            90,
        ),
        (
            14,
            &[Swamp, Riparian, TemperateForest, BorealForest, Meadow],
            s.wetland,
            [140, 255],
            [-8, 40],
            9,
            4,
            [SAGE, GREEN, BROWN, STRAW],
            true,
            130,
        ),
        (
            17,
            &[Meadow, TemperateForest, Riparian, Steppe],
            s.lawn,
            [70, 255],
            [-5, 40],
            11,
            1,
            [LIGHT, GREEN, BROWN, STRAW],
            false,
            80,
        ),
        (
            19,
            &[Desert, Steppe, Beach],
            s.arid,
            [0, 80],
            [0, 62],
            6,
            3,
            [DRY, DRY, BROWN, STRAW],
            true,
            70,
        ),
        (
            50,
            &[Meadow, Steppe, Alpine, Beach],
            s.dry_land,
            [10, 255],
            [-20, 45],
            6,
            1,
            [DRY, LIGHT, STRAW, STRAW],
            true,
            110,
        ),
        (
            51,
            &[Steppe, Meadow],
            s.dry_land,
            [30, 255],
            [0, 42],
            6,
            4,
            [DRY, LIGHT, WHEAT, WHEAT],
            true,
            90,
        ),
        (
            52,
            &[Steppe, Alpine, Meadow],
            s.dry_land,
            [20, 255],
            [-10, 35],
            9,
            3,
            [DRY, WHEAT, WHEAT, STRAW],
            true,
            80,
        ),
        (
            53,
            &[Swamp, Riparian, Meadow],
            s.wetland,
            [170, 255],
            [0, 40],
            6,
            5,
            [MOSS, GREEN, OLIVE, BROWN],
            true,
            140,
        ),
        (
            54,
            &[
                TemperateForest,
                BorealForest,
                Swamp,
                Alpine,
                Riparian,
                Scree,
            ],
            [moss, dirt, mud, grass],
            [120, 255],
            [-20, 30],
            12,
            1,
            [MOSS, MOSS, MOSS, MOSS],
            true,
            150,
        ),
        (
            55,
            &[Beach, Desert],
            s.shore,
            [0, 255],
            [-5, 50],
            6,
            3,
            [DRY, DRY, WHEAT, STRAW],
            true,
            100,
        ),
        (
            56,
            &[Beach, Riparian],
            s.shore,
            [30, 255],
            [-5, 40],
            12,
            2,
            [MOSS, ROSE, ROSE, MOSS],
            true,
            70,
        ),
        (
            57,
            &[Desert, Steppe],
            s.arid,
            [0, 60],
            [5, 62],
            12,
            2,
            [OLIVE, OLIVE, RUST, RUST],
            true,
            60,
        ),
        (
            58,
            &[Scree, Snow, Alpine],
            s.rocky,
            [0, 255],
            [-60, 25],
            12,
            1,
            [GREY, GREY, GREY, GREY],
            true,
            110,
        ),
        (
            59,
            &[TemperateForest, BorealForest],
            s.lawn,
            [100, 255],
            [-15, 32],
            9,
            3,
            [MOSS, DEEP, RUST, BROWN],
            true,
            90,
        ),
        (
            60,
            &[Meadow, Steppe, Alpine],
            s.dry_land,
            [30, 255],
            [-15, 35],
            12,
            1,
            [DUSK, DUSK, DARK_BROWN, DARK_BROWN],
            true,
            60,
        ),
        (
            61,
            &[Swamp, Riparian],
            s.wetland,
            [180, 255],
            [-5, 38],
            9,
            4,
            [MOSS, MOSS, OLIVE, BROWN],
            false,
            100,
        ),
        (
            63,
            &[Snow, Scree],
            s.rocky,
            [0, 255],
            [-62, 10],
            12,
            1,
            [LICHEN, LICHEN, GREY, GREY],
            true,
            80,
        ),
        (
            64,
            &[BorealForest, Alpine],
            s.upland,
            [80, 255],
            [-30, 25],
            12,
            1,
            [MOSS, DEEP, RED, BROWN],
            true,
            90,
        ),
        (
            65,
            &[TemperateForest, Meadow],
            s.lawn,
            [110, 255],
            [-5, 38],
            11,
            2,
            [MOSS, DEEP, RUST, BROWN],
            false,
            70,
        ),
        (
            66,
            &[Riparian, Swamp, Meadow],
            s.wetland,
            [150, 255],
            [-5, 40],
            11,
            1,
            [MOSS, MOSS, OLIVE, BROWN],
            true,
            90,
        ),
    ];
    rows.into_iter()
        .map(
            |(
                id,
                biomes,
                soils,
                moisture,
                temperature,
                form,
                height,
                colours,
                evergreen,
                density,
            )| {
                CoverSpecies {
                    id,
                    biomes: mask(biomes),
                    soils,
                    moisture,
                    depth: [0, 0],
                    temperature,
                    form,
                    height,
                    colours,
                    evergreen,
                    density,
                }
            },
        )
        .collect::<Vec<_>>()
        .into_iter()
        .chain(aquatic_cover())
        .collect()
}

/// The underwater carpet: seagrass, anemones, algae. Like all cover it is
/// stateless; the destroyed-cover record and regrowth apply to it too.
fn aquatic_cover() -> Vec<CoverSpecies> {
    use Biome::*;
    let p = crate::game::terrain_palette();
    let seabed = [p.sand.0, p.gravel.0, p.clay.0, p.sandstone.0];
    let lakebed = [p.gravel.0, p.mud.0, p.clay.0, p.dirt.0];
    // (id, biomes, soils, depth, form, height, colours, density)
    type Row = (
        u16,
        &'static [Biome],
        [u16; 4],
        [u8; 2],
        u8,
        u16,
        [u8; 4],
        u8,
    );
    let rows: [Row; 9] = [
        (
            80,
            &[Ocean],
            seabed,
            [1, 24],
            6,
            2,
            [MOSS, GREEN, MOSS, OLIVE],
            150,
        ),
        (
            81,
            &[Ocean],
            seabed,
            [2, 40],
            9,
            4,
            [GREEN, DEEP, GREEN, OLIVE],
            110,
        ),
        (
            82,
            &[Ocean],
            seabed,
            [3, 50],
            11,
            1,
            [ROSE, VIOLET, ROSE, VIOLET],
            60,
        ),
        (
            83,
            &[Ocean],
            seabed,
            [1, 120],
            12,
            1,
            [OLIVE, OLIVE, BROWN, BROWN],
            100,
        ),
        (
            84,
            &[Ocean],
            seabed,
            [6, 150],
            12,
            1,
            [GREY, LICHEN, GREY, LICHEN],
            70,
        ),
        (
            85,
            &[Ocean],
            seabed,
            [1, 16],
            11,
            1,
            [LIGHT, LIGHT, MOSS, MOSS],
            70,
        ),
        (
            86,
            &[Riparian],
            lakebed,
            [2, 40],
            6,
            3,
            [MOSS, GREEN, OLIVE, BROWN],
            130,
        ),
        (
            87,
            &[Riparian],
            lakebed,
            [2, 30],
            11,
            2,
            [MOSS, MOSS, OLIVE, OLIVE],
            90,
        ),
        (
            88,
            &[Riparian],
            lakebed,
            [1, 60],
            12,
            1,
            [OLIVE, OLIVE, BROWN, BROWN],
            110,
        ),
    ];
    rows.into_iter()
        .map(
            |(id, biomes, soils, depth, form, height, colours, density)| CoverSpecies {
                id,
                biomes: mask(biomes),
                soils,
                moisture: [0, 255],
                temperature: [-20, 62],
                form,
                height,
                colours,
                evergreen: true,
                density,
                depth,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn species_ids_are_unique_stable_and_named() {
        let living = catalogue();
        let cover = cover_catalogue();
        let mut ids: Vec<u16> = living
            .iter()
            .map(|s| s.id)
            .chain(cover.iter().map(|c| c.id))
            .collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "an id is used twice");
        assert!(
            ids.iter().all(|id| name(*id) != "Unknown"),
            "an id has no name"
        );
        // The first twenty keep the numbers they always had.
        for (id, expected) in [
            (1, "Oak"),
            (10, "Acacia"),
            (11, "Meadow grass"),
            (20, "Sage scrub"),
        ] {
            assert_eq!(name(id), expected);
        }
        for s in &living {
            assert!(s.biomes != 0 && s.biomes <= spall_ecology::living::ALL_BIOMES);
        }
        for c in &cover {
            c.validate().unwrap();
        }
        assert!(cover.len() <= spall_ecology::cover::MAX_COVER_SPECIES);
        assert!(living.len() <= spall_ecology::living::MAX_SPECIES);
    }
}
