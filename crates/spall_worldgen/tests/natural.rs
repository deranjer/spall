//! Invariants of the natural preset: a hydrology-first world.

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::fixtures::digest_hex;
use spall_voxel::volume::Sample;
use spall_worldgen::debug::{column_stats, top_down_rgba};
use spall_worldgen::*;

const SIZE: u32 = 512;

fn spec(seed: u64, size: u32) -> WorldGenSpec {
    WorldGenSpec::new(Preset::Natural, seed, size, WorldgenPalette::sequential(1))
}

fn columns(seed: u64, size: u32) -> ColumnMap {
    ColumnMap::compute(&spec(seed, size)).expect("computes")
}

fn filled(w: &GeneratedWorld, x: i64, y: i64, z: i64) -> Option<MaterialId> {
    match w
        .terrain
        .sample(GlobalCell::new(x, y, z))
        .expect("in bounds")
    {
        Sample::Filled(m) => Some(m),
        Sample::Empty { .. } => None,
        Sample::Unknown(r) => panic!("unresident cell {r:?}"),
    }
}

#[test]
fn natural_has_its_own_version_and_height() {
    assert_eq!(Preset::Natural.gen_version(), 4);
    assert_eq!(Preset::Showcase.gen_version(), 3);
    assert_eq!(spec(1, SIZE).height_bricks, 16);
    assert_eq!(spec(1, SIZE).height_cells(), 512);
}

#[test]
fn a_natural_world_is_repeatable_and_seed_sensitive() {
    let a = digest_hex(&generate(&spec(3, SIZE)).unwrap().terrain);
    let b = digest_hex(&generate(&spec(3, SIZE)).unwrap().terrain);
    let c = digest_hex(&generate(&spec(4, SIZE)).unwrap().terrain);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn water_sits_above_the_ground_and_never_in_the_border() {
    for seed in 1..=6 {
        let cols = columns(seed, SIZE);
        let size = i64::from(SIZE);
        let mut wet = 0;
        for z in 0..size {
            for x in 0..size {
                let ws = cols.water_surface(x, z);
                if cols.is_water(x, z) {
                    wet += 1;
                    assert!(ws > cols.height(x, z), "seed {seed}");
                    assert!(
                        !cols.in_border(x, z),
                        "seed {seed}: water in border {x},{z}"
                    );
                } else {
                    assert!(
                        ws == 0 || ws <= cols.height(x, z),
                        "seed {seed}: dry column {x},{z} with surface {ws}"
                    );
                }
            }
        }
        assert!(wet > 0, "seed {seed} has no water at all");
    }
}

#[test]
fn heights_stay_under_the_wall() {
    for seed in 1..=6 {
        let cols = columns(seed, SIZE);
        assert!(
            i64::from(cols.min_max_height().1) < spec(seed, SIZE).wall_top(),
            "seed {seed}"
        );
    }
}

#[test]
fn generated_worlds_hold_water_only_in_air_and_keep_a_dry_spawn() {
    for seed in [1, 2, 5] {
        let w = generate(&spec(seed, SIZE)).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(w.version, 4);
        for c in w.water.cells.iter().step_by(37) {
            assert_eq!(
                filled(&w, c.x, c.y, c.z),
                None,
                "seed {seed}: water in rock"
            );
        }
        assert!(!w.spawns.is_empty());
        for s in &w.spawns {
            let (x, z) = ((s[0] / 0.25).floor() as i64, (s[2] / 0.25).floor() as i64);
            assert!(!w.columns.is_water(x, z));
        }
    }
}

#[test]
fn climate_cools_with_height() {
    // Between two dry columns a few metres apart, the much higher one is colder.
    for seed in 1..=4 {
        let cols = columns(seed, SIZE);
        let size = i64::from(SIZE);
        let (mut pairs, mut colder) = (0u32, 0u32);
        for z in (40..size - 40).step_by(7) {
            for x in (40..size - 60).step_by(7) {
                let (a, b) = ((x, z), (x + 16, z));
                if cols.is_water(a.0, a.1) || cols.is_water(b.0, b.1) {
                    continue;
                }
                let dh = cols.height(b.0, b.1) - cols.height(a.0, a.1);
                if dh.abs() >= 40 {
                    pairs += 1;
                    let (hi, lo) = if dh > 0 { (b, a) } else { (a, b) };
                    if cols.temperature_c(hi.0, hi.1) < cols.temperature_c(lo.0, lo.1) {
                        colder += 1;
                    }
                }
            }
        }
        if pairs >= 20 {
            assert!(
                colder * 10 >= pairs * 9,
                "seed {seed}: {colder}/{pairs} colder"
            );
        }
    }
}

#[test]
fn a_seed_sweep_produces_every_natural_biome_and_both_map_kinds() {
    let mut seen = [false; 12];
    let (mut coast, mut landlocked) = (0, 0);
    for seed in 1..=24 {
        let cols = columns(seed, SIZE);
        let stats = column_stats(&cols);
        for (i, share) in stats.biome_share.iter().enumerate() {
            seen[i] |= *share > 0.0;
        }
        if stats.biome_share[Biome::Ocean as usize] > 0.01 {
            coast += 1;
        } else {
            landlocked += 1;
        }
    }
    let missing: Vec<_> = Biome::EVERY
        .iter()
        .zip(seen)
        .filter(|(_, s)| !s)
        .map(|(b, _)| *b)
        .collect();
    assert!(missing.is_empty(), "never generated: {missing:?}");
    assert!(
        coast > 0 && landlocked > 0,
        "{coast} coastal, {landlocked} landlocked"
    );
}

/// Manual: writes top-down maps for eyeballing the world's layout.
#[test]
#[ignore = "manual: writes PPM maps under .local/natural"]
fn dump_natural_maps() {
    let dir = std::path::Path::new("../../.local/natural");
    std::fs::create_dir_all(dir).unwrap();
    let size: u32 = std::env::var("NATURAL_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    for seed in 1..=6u64 {
        let t = std::time::Instant::now();
        let cols = columns(seed, size);
        let ms = t.elapsed().as_millis();
        let stats = column_stats(&cols);
        let mut ppm = format!("P6\n{size} {size}\n255\n").into_bytes();
        for px in top_down_rgba(&cols).chunks(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        std::fs::write(dir.join(format!("seed{seed}.ppm")), ppm).unwrap();
        let mut m = Vec::new();
        let mut t = Vec::new();
        let mut g = Vec::new();
        for z in (0..i64::from(size)).step_by(5) {
            for x in (0..i64::from(size)).step_by(5) {
                if !cols.is_water(x, z) {
                    m.push(f64::from(cols.moisture(x, z)) / 255.0);
                    t.push(cols.temperature_c(x, z));
                    g.push(cols.gradient(x, z));
                }
            }
        }
        let pct = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            [10, 25, 50, 75, 90, 99].map(|p| (v[v.len() * p / 100] * 100.0).round() / 100.0)
        };
        eprintln!(
            "  land pct[10,25,50,75,90,99] moisture={:?} temp={:?} gradient={:?}",
            pct(&mut m),
            pct(&mut t),
            pct(&mut g)
        );
        let shares: Vec<String> = Biome::EVERY
            .iter()
            .map(|b| format!("{b:?}={:.1}%", stats.biome_share[*b as usize] * 100.0))
            .collect();
        eprintln!(
            "natural seed={seed} size={size} ms={ms} water={:.1}% {}",
            stats.water_share * 100.0,
            shares.join(" ")
        );
    }
}
