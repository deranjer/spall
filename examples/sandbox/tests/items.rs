use std::path::Path;

const NAMES: [&str; 8] = [
    "shovel",
    "axe",
    "compass",
    "compass_needle",
    "paper_note",
    "stick",
    "log",
    "stone",
];

/// (cell count, size in cells) of a decoded asset.
fn measure(name: &str) -> (spall_asset::Asset, u64, [i32; 3]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/items/assets")
        .join(format!("{name}.spvox"));
    let asset = spall_asset::decode(&std::fs::read(&path).unwrap()).unwrap();
    let (mut lo, mut hi, mut cells) = ([i32::MAX; 3], [i32::MIN; 3], 0u64);
    for run in &asset.runs {
        cells += u64::from(run.length_x);
        for axis in 0..3 {
            lo[axis] = lo[axis].min(run.start[axis]);
            hi[axis] = hi[axis].max(run.start[axis]);
        }
        hi[0] = hi[0].max(run.start[0] + run.length_x as i32 - 1);
    }
    let size = [0, 1, 2].map(|i| hi[i] - lo[i] + 1);
    (asset, cells, size)
}

#[test]
fn every_item_asset_loads_non_empty_and_hand_sized() {
    for name in NAMES {
        let (asset, cells, size) = measure(name);
        assert_eq!(asset.cell_size_code, 1, "{name} uses 6.25 cm cells");
        assert!(cells > 0, "{name} is empty");
        assert!(cells <= 400, "{name} has {cells} cells");
        // At most 1.1 m (17 cells) on any axis: hand-held scale.
        assert!(
            size.iter().all(|&s| (1..=17).contains(&s)),
            "{name} {size:?}"
        );
    }
}

#[test]
fn compass_needle_is_a_separate_asset_that_does_not_overlap_the_card() {
    let (compass, _, compass_size) = measure("compass");
    let (needle, needle_cells, needle_size) = measure("compass_needle");
    assert_ne!(compass.uuid, needle.uuid);
    assert_eq!(needle_size, [1, 1, 5]);
    assert_eq!(needle_cells, 5);
    // The needle lives one cell above the 2-cell-thick card (local y 2 in the
    // compass frame), so the card carries no needle geometry.
    assert_eq!(compass_size[1], 2);
    // Pivot at the centre of the middle cell, so it spins in place.
    assert_eq!(needle.pivot_subcells, [128, 128, 2 * 256 + 128]);
}
