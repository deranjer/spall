//! The sandbox's initial vegetation catalogue. These are gameplay species,
//! not engine materials or a claim to a botanically complete simulation.
use spall_ecology::living::Species;

pub const NAMES: [&str; 20] = [
    "Oak",
    "Maple",
    "Birch",
    "Aspen",
    "Scots pine",
    "Spruce",
    "Larch",
    "Willow",
    "Cypress",
    "Acacia",
    "Meadow grass",
    "Fescue",
    "Alpine tussock",
    "Sedge",
    "Reed",
    "Fern",
    "Clover",
    "Wildflower",
    "Desert bunchgrass",
    "Sage scrub",
];

pub fn catalogue() -> Vec<Species> {
    let p = crate::game::terrain_palette();
    let soil = [p.grass.0, p.dirt.0, p.moss.0, p.mud.0];
    let alpine = [p.grass.0, p.dirt.0, p.moss.0, p.gravel.0];
    let desert = [p.sand.0, p.dirt.0, p.clay.0, p.gravel.0];
    // Meadow=1, Alpine=2, Swamp=4, Desert=8. IDs are stable save/wire values.
    let trees = [
        (1, 1, soil, 0, 28, 9, 24, false, [1, 0, 4, 7]),
        (2, 1, soil, 0, 24, 7, 22, false, [1, 1, 6, 7]),
        (3, 3, alpine, 2, 30, 5, 18, false, [3, 1, 4, 8]),
        (4, 3, alpine, 2, 34, 4, 16, false, [3, 1, 4, 8]),
        (5, 3, alpine, 4, 36, 6, 24, true, [2, 2, 2, 2]),
        (6, 2, alpine, 1, 40, 6, 24, true, [2, 2, 2, 2]),
        (7, 2, alpine, 1, 34, 5, 22, false, [13, 1, 4, 8]),
        (8, 4, soil, 3, 24, 9, 26, false, [13, 1, 4, 7]),
        (9, 4, soil, 1, 32, 5, 22, true, [14, 2, 2, 14]),
        (10, 8, desert, 4, 20, 9, 30, true, [3, 3, 7, 3]),
    ];
    let mut out: Vec<_> = trees
        .into_iter()
        .map(
            |(id, biomes, soils, form, height, spread, spacing, evergreen, colours)| Species {
                id,
                tree: true,
                biomes,
                soils,
                moisture: if biomes == 8 {
                    [0, 65]
                } else if biomes == 4 {
                    [150, 255]
                } else {
                    [50, 180]
                },
                wood: 124 + (id - 1) % 12,
                trunk_width: if [1, 5, 8, 9].contains(&id) { 2 } else { 1 },
                form,
                height,
                spread,
                spacing,
                evergreen,
                colours,
                flower: 10,
                growth_ms: 5_000 + u64::from(id) * 400,
                seed_ms: 90_000 + u64::from(id) * 7000,
            },
        )
        .collect();
    let ground = [
        (11, 1, soil, 6, 2, 4, 4, true, [3, 1, 7, 8], 10),
        (12, 3, alpine, 6, 3, 3, 4, true, [13, 1, 8, 8], 10),
        (13, 2, alpine, 12, 2, 5, 5, true, [13, 13, 7, 8], 10),
        (14, 4, soil, 9, 4, 4, 5, true, [13, 1, 7, 8], 10),
        (15, 4, soil, 7, 7, 3, 6, false, [1, 1, 7, 8], 10),
        (16, 5, soil, 8, 3, 5, 6, false, [1, 0, 5, 7], 10),
        (17, 1, soil, 11, 1, 4, 4, false, [3, 1, 7, 8], 10),
        (18, 1, soil, 10, 3, 3, 5, false, [3, 1, 7, 8], 11),
        (19, 8, desert, 6, 3, 4, 7, true, [15, 15, 7, 8], 10),
        (20, 8, desert, 13, 4, 6, 10, true, [14, 14, 8, 8], 12),
    ];
    out.extend(ground.into_iter().map(
        |(id, biomes, soils, form, height, spread, spacing, evergreen, colours, flower)| Species {
            id,
            tree: false,
            biomes,
            soils,
            moisture: if biomes == 8 {
                [0, 65]
            } else if biomes == 4 {
                [150, 255]
            } else {
                [50, 235]
            },
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
        },
    ));
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalogue_has_stable_twenty_species_and_every_biome() {
        let s = super::catalogue();
        assert_eq!(s.len(), 20);
        assert_eq!(s.iter().filter(|s| s.tree).count(), 10);
        for biome in [1, 2, 4, 8] {
            assert!(s.iter().any(|s| s.tree && s.biomes & biome != 0));
            assert!(s.iter().any(|s| !s.tree && s.biomes & biome != 0));
        }
        for (i, s) in s.iter().enumerate() {
            assert_eq!(usize::from(s.id), i + 1);
        }
    }
}
