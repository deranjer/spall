//! The gallery world: every biome's pad holds every plant that grows there.

use sandbox::vegetation::{catalogue, name};
use spall_worldgen::gallery_patches;
use std::collections::BTreeMap;

#[test]
fn every_pad_shows_its_biomes_living_plants_and_the_cover_is_there_to_fit() {
    let scene = sandbox::worldgen_scene::generate("gallery", 1, 1024).expect("gallery");
    let state = scene.vegetation();
    state.validate().unwrap();
    assert_eq!(state.preset, 2);
    let species = catalogue();
    let mut report = Vec::new();
    for pad in gallery_patches(1024) {
        let bit = 1u16 << pad.biome as u16;
        let wanted: Vec<u16> = species
            .iter()
            .filter(|s| s.biomes & bit != 0)
            .map(|s| s.id)
            .collect();
        let mut got: BTreeMap<u16, usize> = BTreeMap::new();
        for p in &state.organisms {
            if (pad.x0..pad.x1).contains(&p.root[0]) && (pad.z0..pad.z1).contains(&p.root[2]) {
                assert!(
                    wanted.contains(&p.species),
                    "{} is not a {:?} plant",
                    name(p.species),
                    pad.biome
                );
                *got.entry(p.species).or_default() += 1;
            }
        }
        let missing: Vec<_> = wanted
            .iter()
            .filter(|id| !got.contains_key(id))
            .map(|id| name(*id))
            .collect();
        report.push(format!(
            "{:?}: {} of {} kinds placed, {} plants, missing {missing:?}",
            pad.biome,
            got.len(),
            wanted.len(),
            got.values().sum::<usize>()
        ));
        // Every kind that can live on the pad is on it; trees twice, the rest four times.
        assert!(missing.is_empty(), "{:?} lacks {missing:?}", pad.biome);
        for (id, n) in &got {
            let tree = species.iter().find(|s| s.id == *id).unwrap().tree;
            assert!(
                *n >= 1 && *n <= if tree { 2 } else { 4 },
                "{} x{n}",
                name(*id)
            );
        }
    }
    eprintln!("{}", report.join("\n"));
}
