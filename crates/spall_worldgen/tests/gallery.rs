//! The gallery preset: one pad per biome.

use spall_worldgen::{
    Biome, ColumnMap, Preset, SEA_LEVEL, WorldGenSpec, WorldgenPalette, gallery_patches, generate,
};
use std::collections::BTreeSet;

fn spec(size: u32) -> WorldGenSpec {
    WorldGenSpec::new(Preset::Gallery, 1, size, WorldgenPalette::sequential(1))
}

#[test]
fn the_gallery_has_one_separate_pad_per_biome() {
    for size in [512, 1024, 2048] {
        let pads = gallery_patches(size);
        assert_eq!(pads.len(), 13);
        let biomes: BTreeSet<_> = pads.iter().map(|p| p.biome as u8).collect();
        assert_eq!(biomes.len(), 13, "every biome once");
        for (i, a) in pads.iter().enumerate() {
            assert!(a.x1 - a.x0 >= 64, "size {size}: pad too small");
            assert!(a.x0 > 32 && a.z0 > 32 && a.x1 < i64::from(size) - 32);
            for b in &pads[i + 1..] {
                let apart = a.x1 <= b.x0 || b.x1 <= a.x0 || a.z1 <= b.z0 || b.z1 <= a.z0;
                assert!(apart, "{:?} overlaps {:?}", a.biome, b.biome);
            }
        }
    }
}

#[test]
fn each_pad_is_its_biome_with_its_own_wetness_water_and_surfaces() {
    let size = 1024;
    let cols = ColumnMap::compute(&spec(size)).unwrap();
    assert!(cols.is_gallery() && cols.is_natural());
    let palette = WorldgenPalette::sequential(1);
    for p in gallery_patches(size) {
        let mut tops = BTreeSet::new();
        let mut deepest = 0;
        for z in p.z0..p.z1 {
            for x in p.x0..p.x1 {
                assert_eq!(cols.biome(x, z), p.biome, "{:?} at {x},{z}", p.biome);
                tops.insert(cols.stack(&palette, x, z).top.0);
                deepest = deepest.max(cols.water_depth(x, z));
            }
        }
        assert!(tops.len() >= 3, "{:?}: {tops:?}", p.biome);
        // Wetter in the east than the west of each pad.
        let (z, w) = ((p.z0 + p.z1) / 2, p.x1 - p.x0);
        let (west, east) = (
            cols.moisture(p.x0 + w / 10, z),
            cols.moisture(p.x1 - w / 10, z),
        );
        assert!(east > west, "{:?}: {west} then {east}", p.biome);
        match p.biome {
            Biome::Ocean => assert!(deepest >= 40, "the sea is deep ({deepest})"),
            Biome::Riparian => assert!((36..48).contains(&deepest), "pond depth {deepest}"),
            Biome::Swamp => assert!((1..=8).contains(&deepest), "swamp pools {deepest}"),
            _ => assert_eq!(deepest, 0, "{:?} is dry", p.biome),
        }
    }
}

#[test]
fn the_riparian_stream_is_too_shallow_for_weed_and_the_pond_is_not() {
    let size = 1024;
    let cols = ColumnMap::compute(&spec(size)).unwrap();
    let p = gallery_patches(size)
        .into_iter()
        .find(|p| p.biome == Biome::Riparian)
        .unwrap();
    let w = p.x1 - p.x0;
    // The stream's centreline, well away from the pond.
    let (sx, sz) = (p.x0 + w / 12, p.z0 + (w as f64 * 0.55) as i64);
    let stream = cols.water_depth(sx, sz);
    assert!((6..28).contains(&stream), "stream {stream}");
    let pond = cols.water_depth(p.x0 + w / 2, sz);
    assert!(pond >= 28, "pond {pond}");
}

#[test]
fn the_gallery_generates_with_a_spawn_on_every_dry_pad() {
    let world = generate(&spec(512)).expect("gallery world");
    assert_eq!(world.version, 1);
    assert_eq!(world.spawns.len(), 12, "every pad but the sea");
    for s in &world.spawns {
        assert!(s[1] / 0.25 > f64::from(SEA_LEVEL), "{s:?} above the sea");
    }
    assert!(
        !world.water.columns.is_empty(),
        "the sea and ponds hold water"
    );
    // Deterministic.
    let again = generate(&spec(512)).unwrap();
    assert_eq!(world.spawns, again.spawns);
}
