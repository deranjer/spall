//! ENG-258: sticks, logs and rocks scattered over a generated world.

use sandbox::game::items;
use sandbox::world_scatter::ScatterSettings;
use sandbox::worldgen_scene::{self, GeneratedScene};
use spall_core::GlobalCell;
use spall_voxel::Sample;

const SIZE: u32 = 512;

fn scene(seed: u64) -> GeneratedScene {
    worldgen_scene::generate("natural", seed, SIZE).expect("generate")
}

fn cell(p: [f64; 3], dy: f64) -> GlobalCell {
    let c = [p[0], p[1] + dy, p[2]].map(|v| (v * 4.0).floor() as i64);
    GlobalCell::new(c[0], c[1], c[2])
}

#[test]
fn placement_is_deterministic_for_a_seed() {
    let settings = ScatterSettings::default();
    let a = scene(3).scatter_items(3, &settings);
    let b = scene(3).scatter_items(3, &settings);
    assert!(!a.is_empty());
    assert_eq!(a, b);
    assert_ne!(a, scene(3).scatter_items(4, &settings));
}

#[test]
fn items_rest_on_solid_ground_with_air_above() {
    let s = scene(3);
    let all = s.scatter_items(3, &ScatterSettings::default());
    for item in &all {
        let t = &s.world().terrain;
        assert!(
            matches!(
                t.sample(cell(item.translation_m, 0.05)),
                Ok(Sample::Empty { .. })
            ),
            "{item:?} is buried"
        );
        assert!(
            matches!(
                t.sample(cell(item.translation_m, -0.05)),
                Ok(Sample::Filled(_))
            ),
            "{item:?} floats"
        );
    }
}

#[test]
fn rocks_favour_banks_and_the_count_is_bounded() {
    let s = scene(3);
    let settings = ScatterSettings::default();
    let all = s.scatter_items(3, &settings);
    let columns = &s.world().columns;
    let near = |p: &[f64; 3]| {
        columns.river_dist((p[0] * 4.0) as i64, (p[2] * 4.0) as i64) <= settings.bank_cells
    };
    let rocks: Vec<_> = all
        .iter()
        .filter(|i| i.kind == items::STONE_CHUNK.0)
        .collect();
    let bank_rocks = rocks.iter().filter(|i| near(&i.translation_m)).count();
    let n = i64::from(columns.size());
    let bank_cols = (0..n)
        .flat_map(|z| (0..n).map(move |x| (x, z)))
        .filter(|&(x, z)| columns.river_dist(x, z) <= settings.bank_cells)
        .count();
    let bank_share_of_rocks = bank_rocks as f64 / rocks.len().max(1) as f64;
    let bank_share_of_ground = bank_cols as f64 / (n * n) as f64;
    eprintln!(
        "rocks {} (bank {bank_rocks}), bank columns {bank_share_of_ground:.3}, items {}",
        rocks.len(),
        all.len()
    );
    assert!(!rocks.is_empty());
    assert!(
        bank_share_of_rocks > 3.0 * bank_share_of_ground,
        "{bank_share_of_rocks} vs {bank_share_of_ground}"
    );
    assert!(all.iter().any(|i| i.kind == items::STICK.0));
    // 512 cells is 1/64 of a 1 km world; the budget there is a few thousand.
    assert!(all.len() < 200, "{}", all.len());
}

/// Measurement for the full 1 km world (slow, needs several GiB):
/// `cargo test -p sandbox --release --test world_scatter -- --ignored --nocapture`.
#[test]
#[ignore = "generates a 1 km world"]
fn full_world_item_count_and_late_join_bytes() {
    let s = worldgen_scene::generate("natural", 1, worldgen_scene::DEFAULT_SIZE_CELLS).unwrap();
    let all = s.scatter_items(1, &ScatterSettings::default());
    let world_items: Vec<_> = all
        .iter()
        .enumerate()
        .map(|(i, item)| spall_protocol::WorldItem {
            item_id: i as u64 + 1,
            kind: item.kind,
            translation_m: item.translation_m,
            yaw_rad: item.yaw_rad,
        })
        .collect();
    let bytes: usize = spall_sim::world_items::updates(world_items, Vec::new())
        .iter()
        .map(|u| spall_protocol::codec::encode_control(u).unwrap().len())
        .sum();
    let count = |k: u16| all.iter().filter(|i| i.kind == k).count();
    eprintln!(
        "items {} sticks {} logs {} rocks {} late-join bytes {bytes}",
        all.len(),
        count(items::STICK.0),
        count(items::WOOD_LOG.0),
        count(items::STONE_CHUNK.0)
    );
    assert!(all.len() < 6000, "{}", all.len());
}
