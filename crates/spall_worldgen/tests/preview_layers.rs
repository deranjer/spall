//! The editor's preview layers: every layer is drawn from the same column map
//! the world is built from, and says what it claims to.

use spall_worldgen::debug::{
    PreviewLayer, SINK_COLOR, SPRING_COLOR, layer_range, layer_rgba, layer_swatches, top_down_rgba,
    vegetation_density,
};
use spall_worldgen::{Biome, ColumnMap, Preset, WorldGenSpec, WorldgenPalette};

fn map(preset: Preset, seed: u64, size: u32) -> ColumnMap {
    ColumnMap::compute(&WorldGenSpec::new(
        preset,
        seed,
        size,
        WorldgenPalette::sequential(1),
    ))
    .expect("computes")
}

#[test]
fn every_layer_is_a_full_deterministic_picture_and_they_differ() {
    let columns = map(Preset::Natural, 3, 512);
    let mut pictures = Vec::new();
    for layer in PreviewLayer::ALL {
        let a = layer_rgba(&columns, layer);
        assert_eq!(a.len(), 512 * 512 * 4, "{layer:?}");
        assert!(
            a.chunks_exact(4).all(|p| p[3] == 255),
            "{layer:?} is opaque"
        );
        assert_eq!(a, layer_rgba(&columns, layer), "{layer:?} is deterministic");
        pictures.push(a);
    }
    // The biome layer is the old minimap, unchanged.
    assert_eq!(pictures[0], top_down_rgba(&columns));
    for i in 0..pictures.len() {
        for j in i + 1..pictures.len() {
            assert_ne!(
                pictures[i],
                pictures[j],
                "{:?} vs {:?}",
                PreviewLayer::ALL[i],
                PreviewLayer::ALL[j]
            );
        }
    }
    // Every layer but the biome one has a legend.
    for layer in PreviewLayer::ALL {
        assert_eq!(
            layer_swatches(layer).is_empty(),
            layer == PreviewLayer::Biome
        );
    }
}

#[test]
fn ranges_describe_the_map_and_discrete_layers_have_none() {
    let columns = map(Preset::Natural, 3, 512);
    assert!(layer_range(&columns, PreviewLayer::Biome).is_none());
    assert!(layer_range(&columns, PreviewLayer::Water).is_none());
    let height = layer_range(&columns, PreviewLayer::Height).unwrap();
    let (low, high) = columns.min_max_height();
    assert_eq!(
        (height.min, height.max),
        (f64::from(low) * 0.25, f64::from(high) * 0.25)
    );
    let moisture = layer_range(&columns, PreviewLayer::Moisture).unwrap();
    assert!(0.0 <= moisture.min && moisture.min < moisture.max && moisture.max <= 100.0);
    let temperature = layer_range(&columns, PreviewLayer::Temperature).unwrap();
    assert!(temperature.min < temperature.max && temperature.unit == "degrees C");
    // Mountains are colder than the lowlands.
    assert!(temperature.max - temperature.min > 8.0, "{temperature:?}");
    let plants = layer_range(&columns, PreviewLayer::Vegetation).unwrap();
    assert!(
        plants.min <= 1.0 && plants.max > 40.0 && plants.max <= 100.0,
        "{plants:?}"
    );
}

#[test]
fn the_water_layer_marks_springs_and_sinks_and_tells_sea_from_fresh_water() {
    let columns = map(Preset::Natural, 3, 512);
    assert!(!columns.springs().is_empty(), "a natural world has springs");
    let image = layer_rgba(&columns, PreviewLayer::Water);
    let pixel = |x: i64, z: i64| {
        let i = ((z * 512 + x) * 4) as usize;
        [image[i], image[i + 1], image[i + 2]]
    };
    for (cell, _) in columns.springs() {
        assert_eq!(pixel(cell[0], cell[2]), SPRING_COLOR, "spring at {cell:?}");
    }
    for cell in columns.sinks() {
        assert_eq!(pixel(cell[0], cell[2]), SINK_COLOR, "sink at {cell:?}");
    }
    // Some sea and some fresh water are drawn in different blues.
    let mut sea = None;
    let mut fresh = None;
    for z in (0..512).step_by(3) {
        for x in (0..512).step_by(3) {
            if columns.is_water(x, z) && columns.water_depth(x, z) > 6 {
                if columns.water_surface(x, z) <= 129 {
                    sea.get_or_insert(pixel(x, z));
                } else {
                    fresh.get_or_insert(pixel(x, z));
                }
            }
        }
    }
    let (sea, fresh) = (sea.expect("sea"), fresh.expect("a lake or river"));
    assert_ne!(sea, fresh, "sea and fresh water look different");
}

#[test]
fn vegetation_density_is_zero_where_nothing_grows_and_richer_in_forest_than_desert() {
    let columns = map(Preset::Natural, 1, 1024);
    let size = 1024;
    let mut sums = std::collections::BTreeMap::<u8, (f64, f64)>::new();
    for z in 0..size {
        for x in 0..size {
            let d = vegetation_density(&columns, x, z);
            assert!((0.0..=1.0).contains(&d));
            if columns.is_water(x, z) || matches!(columns.biome(x, z), Biome::Snow | Biome::Ocean) {
                assert_eq!(d, 0.0);
            }
            let e = sums.entry(columns.biome(x, z) as u8).or_default();
            e.0 += d;
            e.1 += 1.0;
        }
    }
    let mean = |b: Biome| sums.get(&(b as u8)).map(|(s, n)| s / n).expect("present");
    assert!(mean(Biome::PineForest) > mean(Biome::Meadow));
    assert!(mean(Biome::Meadow) > mean(Biome::Desert));
    assert!(mean(Biome::TemperateForest) > 5.0 * mean(Biome::Desert));
}

#[test]
fn the_gallery_previews_too() {
    let columns = map(Preset::Gallery, 1, 512);
    for layer in PreviewLayer::ALL {
        assert_eq!(layer_rgba(&columns, layer).len(), 512 * 512 * 4);
    }
}
