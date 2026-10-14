//! Ecological placement of the living plants of a natural world: canopy by
//! biome, stands, spacing, the shaded understory, and determinism.

use std::collections::{BTreeMap, HashMap};

use spall_ecology::living::{MAX_GROUND, MAX_TREES};
use spall_worldgen::Biome;

fn world() -> sandbox::worldgen_scene::GeneratedScene {
    sandbox::worldgen_scene::generate("natural", 4, 1024).unwrap()
}

/// Land columns per biome, sampled every eight cells (each sample stands for 64).
fn area(scene: &sandbox::worldgen_scene::GeneratedScene) -> HashMap<Biome, f64> {
    let cols = &scene.world().columns;
    let size = i64::from(cols.size());
    let mut out = HashMap::new();
    for z in (0..size).step_by(8) {
        for x in (0..size).step_by(8) {
            if !cols.is_water(x, z) {
                *out.entry(cols.biome(x, z)).or_insert(0.0) += 64.0;
            }
        }
    }
    out
}

fn trees_per_area(scene: &sandbox::worldgen_scene::GeneratedScene) -> HashMap<Biome, f64> {
    let cols = &scene.world().columns;
    let area = area(scene);
    let mut count: HashMap<Biome, f64> = HashMap::new();
    for p in scene
        .vegetation()
        .organisms
        .iter()
        .filter(|p| !p.wood.is_empty())
    {
        *count.entry(cols.biome(p.root[0], p.root[2])).or_default() += 1.0;
    }
    area.into_iter()
        .filter(|(_, a)| *a >= 4000.0)
        .map(|(b, a)| (b, count.get(&b).copied().unwrap_or(0.0) / a * 10_000.0))
        .collect()
}

#[test]
fn canopy_follows_the_biome_dense_in_forests_open_on_grassland_absent_in_deserts() {
    let scene = world();
    let density = trees_per_area(&scene);
    let get = |b: Biome| density.get(&b).copied();
    let forest = [
        Biome::PineForest,
        Biome::TemperateForest,
        Biome::BorealForest,
    ]
    .into_iter()
    .filter_map(get)
    .fold(0.0f64, f64::max);
    assert!(forest > 0.0, "{density:?}");
    for open in [Biome::Meadow, Biome::Steppe] {
        if let Some(d) = get(open) {
            assert!(
                d < forest * 0.6,
                "{open:?} {d:.1} trees per 10k cells against forest {forest:.1}: {density:?}"
            );
        }
    }
    for bare in [Biome::Desert, Biome::Snow, Biome::Scree] {
        if let Some(d) = get(bare) {
            assert!(d < forest * 0.1, "{bare:?} {d:.1} against {forest:.1}");
        }
    }
}

#[test]
fn trees_gather_in_stands_with_a_dominant_species() {
    let scene = world();
    let state = scene.vegetation();
    // Stands of about 40 m: the commonest species of a stand takes most of it.
    let mut stands: BTreeMap<(i64, i64), BTreeMap<u16, usize>> = BTreeMap::new();
    for p in state.organisms.iter().filter(|p| !p.wood.is_empty()) {
        *stands
            .entry((p.root[0] / 160, p.root[2] / 160))
            .or_default()
            .entry(p.species)
            .or_default() += 1;
    }
    let (mut share, mut n) = (0.0, 0.0);
    for kinds in stands.values() {
        let total: usize = kinds.values().sum();
        if total >= 12 {
            share += *kinds.values().max().unwrap() as f64 / total as f64;
            n += 1.0;
        }
    }
    assert!(n >= 8.0, "only {n} stands with a dozen trees");
    assert!(share / n > 0.45, "mean dominant share {:.2}", share / n);
}

#[test]
fn spacing_is_respected_and_caps_hold() {
    let scene = world();
    let state = scene.vegetation();
    let trees: Vec<_> = state
        .organisms
        .iter()
        .filter(|p| !p.wood.is_empty())
        .collect();
    assert!(trees.len() <= MAX_TREES);
    assert!(state.organisms.len() - trees.len() <= MAX_GROUND);
    // No two trees closer than three cells (placement never goes below that).
    let mut seen = std::collections::HashMap::new();
    for t in &trees {
        let key = (t.root[0].div_euclid(3), t.root[2].div_euclid(3));
        seen.entry(key).or_insert_with(Vec::new).push(t.root);
    }
    for t in &trees {
        let (kx, kz) = (t.root[0].div_euclid(3), t.root[2].div_euclid(3));
        for dz in -1..=1 {
            for dx in -1..=1 {
                for other in seen.get(&(kx + dx, kz + dz)).into_iter().flatten() {
                    if *other != t.root {
                        let d2 = (other[0] - t.root[0]).pow(2) + (other[2] - t.root[2]).pow(2);
                        assert!(d2 >= 9, "trees at {:?} and {:?}", t.root, other);
                    }
                }
            }
        }
    }
}

#[test]
fn the_understory_gathers_in_the_shade_of_trees() {
    let scene = world();
    let state = scene.vegetation();
    let cols = &scene.world().columns;
    let forest = |x: i64, z: i64| {
        matches!(
            cols.biome(x, z),
            Biome::TemperateForest | Biome::BorealForest | Biome::PineForest
        )
    };
    let trees: Vec<[i64; 3]> = state
        .organisms
        .iter()
        .filter(|p| !p.wood.is_empty() && forest(p.root[0], p.root[2]))
        .map(|p| p.root)
        .collect();
    let near = |x: i64, z: i64| {
        trees
            .iter()
            .any(|t| (t[0] - x).pow(2) + (t[2] - z).pow(2) <= 49)
    };
    // Compare the share of forest ground plants under trees with the share of
    // forest ground that is under trees at all.
    let ground: Vec<_> = state
        .organisms
        .iter()
        .filter(|p| p.wood.is_empty() && forest(p.root[0], p.root[2]))
        .collect();
    if ground.len() < 40 || trees.len() < 40 {
        return;
    }
    let under =
        ground.iter().filter(|p| near(p.root[0], p.root[2])).count() as f64 / ground.len() as f64;
    let size = i64::from(cols.size());
    let (mut covered, mut total) = (0.0, 0.0);
    for z in (33..size - 33).step_by(6) {
        for x in (33..size - 33).step_by(6) {
            if forest(x, z) && !cols.is_water(x, z) {
                total += 1.0;
                if near(x, z) {
                    covered += 1.0;
                }
            }
        }
    }
    let base = covered / total;
    assert!(
        under > base * 1.2,
        "{under:.2} of forest ground plants are under trees; {base:.2} of the ground is"
    );
}

#[test]
fn placement_is_deterministic_for_a_seed() {
    let a = world();
    let b = world();
    assert_eq!(a.vegetation().organisms, b.vegetation().organisms);
    assert_eq!(a.vegetation().next_id, b.vegetation().next_id);
}
