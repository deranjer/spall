//! Invariant tests for generated worlds. The full 1024-cell arena is only
//! checked through the cheap 2D column map; voxel checks use a 256-cell arena.

use spall_core::{GlobalCell, MaterialId};
use spall_voxel::fixtures::digest_hex;
use spall_voxel::volume::Sample;
use spall_worldgen::caves::{CAVE_FLOOR, CaveField};
use spall_worldgen::*;
use std::sync::OnceLock;

const SMALL: u32 = 256;

fn palette() -> WorldgenPalette {
    WorldgenPalette::sequential(1)
}

fn spec(seed: u64, size: u32) -> WorldGenSpec {
    WorldGenSpec::new(Preset::Showcase, seed, size, palette())
}

fn small() -> &'static GeneratedWorld {
    static W: OnceLock<GeneratedWorld> = OnceLock::new();
    W.get_or_init(|| generate(&spec(1, SMALL)).expect("generate"))
}

fn at(w: &GeneratedWorld, x: i64, y: i64, z: i64) -> Sample {
    w.terrain
        .sample(GlobalCell::new(x, y, z))
        .expect("in bounds")
}

/// `Some(material)` for rock, `None` for air. Panics on an unresident cell:
/// nothing inside the arena may ever read as `Unknown`.
fn filled(s: Sample) -> Option<MaterialId> {
    match s {
        Sample::Filled(m) => Some(m),
        Sample::Empty { .. } => None,
        Sample::Unknown(r) => panic!("unresident cell inside the arena: {r:?}"),
    }
}

/// Horizontal reach of a cave mouth from its surface start, plus its chamber.
fn mouth_reach(size: u32) -> i64 {
    (f64::from(size) * 0.15).min(160.0) as i64 + 20
}

fn near_mouth(w: &GeneratedWorld, x: i64, z: i64, size: u32) -> bool {
    let reach = mouth_reach(size);
    w.cave_mouths
        .iter()
        .any(|&(mx, mz)| (mx - x).abs().max((mz - z).abs()) <= reach)
}

#[test]
fn generation_is_deterministic_and_seed_sensitive() {
    let a = digest_hex(&generate(&spec(7, SMALL)).unwrap().terrain);
    let b = digest_hex(&generate(&spec(7, SMALL)).unwrap().terrain);
    let c = digest_hex(&generate(&spec(8, SMALL)).unwrap().terrain);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

/// Pins the exact output. A change here means the same seed now makes a
/// different world: bump `GEN_VERSION` and update this value deliberately.
#[test]
fn golden_digest_for_version() {
    assert_eq!(GEN_VERSION, 1);
    assert_eq!(
        digest_hex(&small().terrain),
        "71810a217d2e3da9a059015b8442c9f41a1265a4ca61b318a4f8089bca5a6377",
        "showcase seed 1 at {SMALL} cells"
    );
}

#[test]
fn invalid_specs_are_rejected() {
    assert!(matches!(
        generate(&spec(1, 100)),
        Err(GenError::InvalidSize(100))
    ));
    assert!(matches!(
        generate(&spec(1, 64)),
        Err(GenError::InvalidSize(64))
    ));
    let mut bad = spec(1, SMALL);
    bad.palette.sand = MaterialId::AIR;
    assert!(matches!(generate(&bad), Err(GenError::PaletteContainsAir)));
}

#[test]
fn every_arena_brick_is_resident_so_nothing_reads_unknown() {
    let w = small();
    let per_side = (SMALL / 32) as usize;
    assert_eq!(w.terrain.resident_brick_count(), per_side * per_side * 12);
    let edge = i64::from(SMALL) - 1;
    for (x, z) in [(0, 0), (edge, 0), (0, edge), (edge, edge)] {
        for y in [0, HEIGHT_CELLS - 1] {
            filled(at(w, x, y, z));
        }
    }
}

#[test]
fn columns_are_bedrock_then_rock_then_air() {
    let w = small();
    let p = palette();
    for z in (0..i64::from(SMALL)).step_by(5) {
        for x in (0..i64::from(SMALL)).step_by(5) {
            let h = i64::from(w.columns.height(x, z));
            for y in (0..HEIGHT_CELLS).step_by(3) {
                let m = filled(at(w, x, y, z));
                if y > h {
                    assert_eq!(m, None, "solid above the surface at {x},{y},{z}");
                } else if y < 32 {
                    assert_eq!(m, Some(p.bedrock), "bedrock floor at {x},{y},{z}");
                } else if y < CAVE_FLOOR {
                    assert!(m.is_some(), "void below the cave floor at {x},{y},{z}");
                    assert_ne!(m, Some(p.bedrock));
                }
            }
            if !near_mouth(w, x, z, SMALL) {
                assert!(
                    filled(at(w, x, h, z)).is_some(),
                    "surface cell solid at {x},{z}"
                );
            }
            assert_eq!(filled(at(w, x, h + 1, z)), None);
        }
    }
}

#[test]
fn caves_exist_but_stay_a_minority_and_keep_their_roof() {
    let w = small();
    let (mut rock, mut voids) = (0u64, 0u64);
    for z in (0..i64::from(SMALL)).step_by(2) {
        for x in (0..i64::from(SMALL)).step_by(2) {
            let h = i64::from(w.columns.height(x, z));
            let exempt = near_mouth(w, x, z, SMALL);
            for y in CAVE_FLOOR..=h {
                let solid = filled(at(w, x, y, z)).is_some();
                if y <= h - 24 || (y <= h - 6 && w.columns.lowland(x, z) == 0) {
                    rock += 1;
                    voids += u64::from(!solid);
                } else if !exempt {
                    assert!(
                        solid,
                        "natural cave breached the roof at {x},{y},{z} (h={h})"
                    );
                }
            }
        }
    }
    let fraction = voids as f64 / (rock + voids) as f64;
    assert!(
        (0.02..0.15).contains(&fraction),
        "cave share of deep rock {fraction:.3}"
    );
}

#[test]
fn cave_mouths_open_onto_the_surface() {
    let w = small();
    assert!(
        !w.cave_mouths.is_empty(),
        "showcase must have a cave entrance"
    );
    for &(x, z) in &w.cave_mouths {
        let h = i64::from(w.columns.height(x, z));
        assert_eq!(
            filled(at(w, x, h - 1, z)),
            None,
            "mouth at {x},{z} is not open below the surface"
        );
    }
}

#[test]
fn full_arena_has_eight_mouths_away_from_the_basin() {
    let s = spec(1, SHOWCASE_SIZE_CELLS);
    let cols = ColumnMap::compute(&s).unwrap();
    let caves = CaveField::new(1, &cols);
    assert_eq!(caves.mouth_count(), 8);
    for (x, z) in caves.mouth_sites() {
        assert_eq!(cols.lowland(x, z), 0);
    }
}

#[test]
fn full_arena_covers_every_biome_and_feature() {
    let cols = ColumnMap::compute(&spec(1, SHOWCASE_SIZE_CELLS)).unwrap();
    let stats = debug::column_stats(&cols);
    for (biome, share) in Biome::ALL.iter().zip(stats.biome_share) {
        assert!(
            share >= 0.05,
            "{biome:?} covers only {share:.3} of the arena"
        );
    }
    assert!(
        stats.water_share >= 0.005,
        "no water: {}",
        stats.water_share
    );
    assert!(
        stats.max_height >= SEA_LEVEL + 120,
        "peaks only {} cells over the sea",
        stats.max_height - SEA_LEVEL
    );
    assert!(
        stats.max_height < HEIGHT_CELLS as i32 - 24,
        "no air above the peaks"
    );
    assert!(stats.min_height <= SEA_LEVEL - 20, "lake is too shallow");

    // Every surface role of the palette shows up somewhere.
    let p = palette();
    let size = i64::from(SHOWCASE_SIZE_CELLS);
    let mut seen = std::collections::BTreeSet::new();
    for z in (0..size).step_by(2) {
        for x in (0..size).step_by(2) {
            seen.insert(cols.stack(&p, x, z).top);
        }
    }
    for (name, id) in [
        ("grass", p.grass),
        ("stone", p.stone),
        ("slate", p.slate),
        ("snow", p.snow),
        ("sand", p.sand),
        ("mud", p.mud),
        ("moss", p.moss),
        ("gravel", p.gravel),
    ] {
        assert!(seen.contains(&id), "no {name} surface in the showcase");
    }
}

#[test]
fn water_is_one_bounded_basin_that_fits_the_fluid_budget() {
    let s = spec(1, SHOWCASE_SIZE_CELLS);
    let cols = ColumnMap::compute(&s).unwrap();
    let size = i64::from(SHOWCASE_SIZE_CELLS);
    let (mut lo, mut hi) = ([i64::MAX; 2], [i64::MIN; 2]);
    for z in 0..size {
        for x in 0..size {
            if cols.is_water(x, z) {
                assert!(
                    (1..size - 1).contains(&x) && (1..size - 1).contains(&z),
                    "water touches the arena border at {x},{z}"
                );
                // A water column's neighbours are water or dry land at/above the surface.
                for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                    assert!(
                        cols.is_water(x + dx, z + dz) || cols.height(x + dx, z + dz) >= SEA_LEVEL
                    );
                }
                lo = [lo[0].min(x), lo[1].min(z)];
                hi = [hi[0].max(x), hi[1].max(z)];
            }
        }
    }
    assert!(lo[0] < hi[0], "no water");
    // The 2D box of the basin, at full water depth, stays inside the budget.
    let deepest = i64::from(SEA_LEVEL - cols.min_max_height().0) + 6 + 4;
    let cells = (hi[0] - lo[0] + 9) * (hi[1] - lo[1] + 9) * deepest;
    assert!(cells as u64 <= WATER_DOMAIN_BUDGET, "domain {cells}");
}

#[test]
fn water_fills_basins_to_the_sea_level_and_never_enters_rock() {
    let w = small();
    assert!(
        !w.water.cells.is_empty(),
        "showcase has water at {SMALL} cells"
    );
    assert_eq!(w.water.surface_y, i64::from(SEA_LEVEL));
    let (lo, hi) = w.water.bounds.expect("bounds");
    for c in &w.water.cells {
        assert_eq!(filled(at(w, c.x, c.y, c.z)), None, "water in rock at {c:?}");
        assert!(c.y <= i64::from(SEA_LEVEL));
        assert!((lo.x..=hi.x).contains(&c.x) && (lo.y..=hi.y).contains(&c.y));
        assert!((lo.z..=hi.z).contains(&c.z));
    }
    // Each wet column is a solid bed, then water all the way to the surface.
    let wet: std::collections::BTreeSet<_> =
        w.water.cells.iter().map(|c| (c.x, c.y, c.z)).collect();
    let sea = i64::from(SEA_LEVEL);
    for z in 0..i64::from(SMALL) {
        for x in 0..i64::from(SMALL) {
            if w.columns.is_water(x, z) {
                let h = i64::from(w.columns.height(x, z));
                assert!(filled(at(w, x, h, z)).is_some());
                assert!(wet.contains(&(x, h + 1, z)) && wet.contains(&(x, sea, z)));
                assert!(!wet.contains(&(x, sea + 1, z)));
            }
        }
    }
    assert!(w.water.domain_cells() <= WATER_DOMAIN_BUDGET);
}

#[test]
fn spawns_are_dry_solid_and_have_headroom() {
    let w = small();
    assert!(!w.spawns.is_empty());
    for s in &w.spawns {
        let (x, z) = ((s[0] / 0.25).floor() as i64, (s[2] / 0.25).floor() as i64);
        let feet = (s[1] / 0.25).round() as i64;
        assert!(
            filled(at(w, x, feet - 1, z)).is_some(),
            "no ground under {s:?}"
        );
        for dy in 0..SPAWN_HEADROOM_CELLS {
            assert_eq!(
                filled(at(w, x, feet + dy, z)),
                None,
                "headroom {dy} at {s:?}"
            );
        }
        assert!(feet > i64::from(SEA_LEVEL), "spawn below the sea surface");
        assert_eq!(w.columns.biome(x, z), Biome::Meadow);
    }
}

#[test]
fn generation_reports_the_arena_region_and_anchor() {
    let w = small();
    assert_eq!(w.anchor_y, 0);
    assert_eq!(w.region.0, GlobalCell::new(0, 0, 0));
    assert_eq!(
        w.region.1,
        GlobalCell::new(i64::from(SMALL) - 1, HEIGHT_CELLS - 1, i64::from(SMALL) - 1)
    );
    assert_eq!(w.version, GEN_VERSION);
}
