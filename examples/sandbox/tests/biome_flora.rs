//! Every biome has its own flora: living plants, ground cover that thickens and
//! diversifies with wetness, and enough different surfaces to root in.

use sandbox::game::terrain_palette;
use sandbox::vegetation::{catalogue, cover_catalogue};
use spall_worldgen::{Biome, ColumnMap, Preset, WorldGenSpec};
use std::collections::BTreeSet;

fn world(seed: u64, size: u32) -> ColumnMap {
    ColumnMap::compute(&WorldGenSpec::new(
        Preset::Natural,
        seed,
        size,
        terrain_palette(),
    ))
    .expect("natural world")
}

/// Top materials per biome across a few worlds.
fn surfaces(seeds: &[u64]) -> Vec<BTreeSet<u16>> {
    let palette = terrain_palette();
    let mut tops = vec![BTreeSet::new(); 12];
    for &seed in seeds {
        let cols = world(seed, 512);
        for z in 0..512 {
            for x in 0..512 {
                tops[cols.biome(x, z) as usize].insert(cols.stack(&palette, x, z).top.0);
            }
        }
    }
    tops
}

/// Plants a biome can host: living species and cover whose soils include one
/// of the biome's surfaces (moisture and temperature ignored).
fn flora(biome: Biome, tops: &BTreeSet<u16>) -> (usize, usize, usize) {
    let bit = 1u16 << biome as u16;
    let rooted = |soils: &[u16; 4]| soils.iter().any(|s| tops.contains(s));
    let living = catalogue();
    let trees = living
        .iter()
        .filter(|s| s.tree && s.biomes & bit != 0 && rooted(&s.soils))
        .count();
    let ground = living
        .iter()
        .filter(|s| !s.tree && s.biomes & bit != 0 && rooted(&s.soils))
        .count();
    let cover = cover_catalogue()
        .iter()
        .filter(|c| c.biomes & bit != 0 && rooted(&c.soils))
        .count();
    (trees, ground, cover)
}

#[test]
fn every_fertile_biome_hosts_five_living_plants_and_three_covers() {
    let tops = surfaces(&[1, 2, 3, 4]);
    let mut short = Vec::new();
    for b in Biome::EVERY {
        let (trees, ground, cover) = flora(b, &tops[b as usize]);
        let living = trees + ground;
        // The bare biomes (snow, scree, beach) need 1 to 3 plants; the sea
        // floor has none yet (no underwater plants are drawn).
        let (need_living, need_cover) = match b {
            Biome::Ocean => (0, 0),
            Biome::Snow | Biome::Scree | Biome::Beach => (3, 2),
            Biome::Desert => (5, 3),
            _ => (5, 3),
        };
        if living < need_living || cover < need_cover {
            short.push(format!(
                "{b:?}: {living} living ({trees} trees + {ground} ground), {cover} cover"
            ));
        }
    }
    assert!(short.is_empty(), "{short:#?}");
}

#[test]
fn every_biome_has_three_or_more_surfaces() {
    let tops = surfaces(&[1, 2, 3, 4]);
    for b in Biome::EVERY {
        assert!(tops[b as usize].len() >= 3, "{b:?}: {:?}", tops[b as usize]);
    }
}

/// Wetter ground carries more kinds of cover, in every fertile biome whose
/// wetness varies, and the mix differs from biome to biome.
#[test]
fn wetter_ground_admits_more_kinds_of_cover() {
    let tops = surfaces(&[1, 2, 3, 4]);
    let cover = cover_catalogue();
    let kinds = |biome: Biome, moisture: u8| {
        cover
            .iter()
            .filter(|c| {
                c.biomes & (1 << biome as u16) != 0
                    && c.soils.iter().any(|s| tops[biome as usize].contains(s))
                    && (c.moisture[0]..=c.moisture[1]).contains(&moisture)
                    && (c.temperature[0]..=c.temperature[1]).contains(&12)
            })
            .count()
    };
    for b in [
        Biome::Meadow,
        Biome::Steppe,
        Biome::Riparian,
        Biome::Swamp,
        Biome::TemperateForest,
        Biome::BorealForest,
        Biome::Alpine,
    ] {
        let (dry, mid, wet) = (kinds(b, 40), kinds(b, 130), kinds(b, 230));
        assert!(
            dry <= mid && mid <= wet,
            "{b:?}: {dry} kinds when dry, {mid} mid, {wet} wet"
        );
    }
    // Meadow shows the climb across its whole range.
    assert!(kinds(Biome::Meadow, 40) < kinds(Biome::Meadow, 230));
    // And the sets are not the same everywhere.
    let ids = |biome: Biome| -> BTreeSet<u16> {
        cover
            .iter()
            .filter(|c| c.biomes & (1 << biome as u16) != 0)
            .map(|c| c.id)
            .collect()
    };
    for (a, b) in [
        (Biome::Desert, Biome::Swamp),
        (Biome::Alpine, Biome::Meadow),
        (Biome::Beach, Biome::BorealForest),
        (Biome::Steppe, Biome::TemperateForest),
    ] {
        assert_ne!(ids(a), ids(b), "{a:?} and {b:?} carry the same cover");
    }
}

/// The same counts, but against the climate the worlds really have: a species
/// only counts for a biome if at least 40 of its columns (across four worlds)
/// satisfy its soil, wetness and, for cover, temperature.
#[test]
fn the_flora_really_fits_each_biomes_climate() {
    let palette = terrain_palette();
    let living = catalogue();
    let cover = cover_catalogue();
    let mut living_hits = vec![vec![0u32; living.len()]; 12];
    let mut cover_hits = vec![vec![0u32; cover.len()]; 12];
    for seed in [1u64, 2, 3, 4] {
        let cols = world(seed, 512);
        for z in 0..512 {
            for x in 0..512 {
                let b = cols.biome(x, z);
                let bit = 1u16 << b as u16;
                let top = cols.stack(&palette, x, z).top.0;
                let m = cols.moisture(x, z);
                let t = cols.temperature_c(x, z).round() as i32;
                for (i, s) in living.iter().enumerate() {
                    if s.biomes & bit != 0
                        && s.soils.contains(&top)
                        && (s.moisture[0]..=s.moisture[1]).contains(&m)
                    {
                        living_hits[b as usize][i] += 1;
                    }
                }
                for (i, c) in cover.iter().enumerate() {
                    if c.biomes & bit != 0
                        && c.soils.contains(&top)
                        && (c.moisture[0]..=c.moisture[1]).contains(&m)
                        && (i32::from(c.temperature[0])..=i32::from(c.temperature[1])).contains(&t)
                    {
                        cover_hits[b as usize][i] += 1;
                    }
                }
            }
        }
    }
    let mut short = Vec::new();
    for b in Biome::EVERY {
        let living_kinds = living_hits[b as usize].iter().filter(|&&n| n >= 40).count();
        let cover_kinds = cover_hits[b as usize].iter().filter(|&&n| n >= 40).count();
        let (need_living, need_cover) = match b {
            Biome::Ocean => (0, 0),
            Biome::Snow | Biome::Scree | Biome::Beach => (3, 2),
            _ => (5, 3),
        };
        eprintln!("{b:?}: {living_kinds} living, {cover_kinds} cover");
        if living_kinds < need_living || cover_kinds < need_cover {
            short.push(format!("{b:?}: {living_kinds} living, {cover_kinds} cover"));
        }
    }
    assert!(short.is_empty(), "{short:#?}");
}
