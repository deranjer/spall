//! Invariants of the coarse landscape: erosion, drainage, lakes, rivers, springs.

use spall_worldgen::SEA_LEVEL;
use spall_worldgen::hydrology::{FP, NONE, OUTLET_BASIN, OUTLET_NONE, OUTLET_SEA};
use spall_worldgen::landscape::Landscape;

const SIZE: u32 = 512;
const SEEDS: std::ops::RangeInclusive<u64> = 1..=12;

fn scapes() -> Vec<Landscape> {
    SEEDS
        .map(|s| Landscape::generate(s, SIZE).expect("generates"))
        .collect()
}

#[test]
fn landscape_is_repeatable() {
    let a = Landscape::generate(3, SIZE).unwrap();
    let b = Landscape::generate(3, SIZE).unwrap();
    assert_eq!(a.hydrology.height, b.hydrology.height);
    assert_eq!(a.hydrology.receiver, b.hydrology.receiver);
    assert_eq!(a.hydrology.river, b.hydrology.river);
    assert_eq!(a.hydrology.springs, b.hydrology.springs);
    let c = Landscape::generate(4, SIZE).unwrap();
    assert_ne!(a.hydrology.height, c.hydrology.height);
}

#[test]
fn river_beds_never_rise_downstream() {
    for s in scapes() {
        let h = &s.hydrology;
        for i in 0..h.river.len() {
            if h.river[i] {
                let r = h.receiver[i] as usize;
                assert!(h.bed[i] >= h.bed[r], "seed {}: bed rises at {i}", s.seed);
                assert!(h.bed[i] <= h.height[i]);
            }
        }
    }
}

#[test]
fn every_river_reaches_the_sea_or_a_terminal_basin() {
    for s in scapes() {
        let h = &s.hydrology;
        for i in 0..h.river.len() {
            if h.river[i] {
                let end = h.terminus(i as u32) as usize;
                let kind = h.outlet[end];
                assert!(
                    kind == OUTLET_SEA || kind == OUTLET_BASIN,
                    "seed {}: river {i} ends at a non-outlet",
                    s.seed
                );
                // Terminal basins exist only on landlocked maps, sea only on coasts.
                assert_eq!(
                    kind == OUTLET_SEA,
                    s.landform.is_coastal(),
                    "seed {}",
                    s.seed
                );
            }
        }
    }
}

#[test]
fn lakes_sit_at_their_spill_height() {
    let mut lakes = 0;
    for s in scapes() {
        let h = &s.hydrology;
        for lake in &h.lakes {
            lakes += 1;
            for &c in &lake.cells {
                assert!(h.height[c as usize] < lake.level, "seed {}", s.seed);
                assert_ne!(h.lake_of[c as usize], NONE);
            }
            if lake.spill != NONE {
                assert_eq!(lake.level, h.height[lake.spill as usize], "seed {}", s.seed);
                assert_eq!(h.lake_of[lake.spill as usize], NONE);
            }
        }
    }
    assert!(lakes > 0, "no lake in {} seeds", SEEDS.count());
}

#[test]
fn no_lake_is_larger_than_the_cap() {
    use spall_worldgen::hydrology::HydroParams;
    for s in scapes() {
        let cap = HydroParams::for_grid(s.n, SEA_LEVEL * FP).max_lake_area;
        for lake in &s.hydrology.lakes {
            // Terminal lakes are chosen by the landform, not by the depression cap.
            if lake.spill != NONE {
                assert!(lake.cells.len() as u32 <= cap, "seed {}", s.seed);
            }
        }
    }
}

#[test]
fn springs_are_high_river_heads() {
    let mut springs = 0;
    for s in scapes() {
        let h = &s.hydrology;
        let min = SEA_LEVEL * FP + 40 * FP;
        for &sp in &h.springs {
            springs += 1;
            let sp = sp as usize;
            assert!(h.river[sp] && h.height[sp] >= min, "seed {}", s.seed);
            assert_eq!(h.outlet[sp], OUTLET_NONE);
        }
    }
    assert!(springs > 0, "no spring in {} seeds", SEEDS.count());
}

#[test]
fn both_kinds_of_map_have_rivers_and_water_ends_somewhere() {
    let (mut coastal, mut landlocked) = (0, 0);
    for s in scapes() {
        let rivers = s.hydrology.river.iter().filter(|&&r| r).count();
        if s.landform.is_coastal() {
            coastal += 1;
        } else {
            landlocked += 1;
            assert!(
                !s.hydrology.lakes.iter().all(|l| l.spill != NONE) || !s.landform.basins.is_empty()
            );
        }
        assert!(rivers > 0, "seed {} has no river", s.seed);
    }
    assert!(
        coastal > 0 && landlocked > 0,
        "{coastal} coastal, {landlocked} landlocked"
    );
}

/// Manual: writes height and water maps for eyeballing, and times 1 km.
#[test]
#[ignore = "manual: writes PGM/PPM maps under .local/landscape"]
fn dump_maps() {
    let dir = std::path::Path::new("../../.local/landscape");
    std::fs::create_dir_all(dir).unwrap();
    let size: u32 = std::env::var("LANDSCAPE_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096);
    for seed in 1..=6u64 {
        let t = std::time::Instant::now();
        let s = Landscape::generate(seed, size).unwrap();
        let ms = t.elapsed().as_millis();
        let n = s.n;
        let h = &s.hydrology;
        let mut ppm = format!("P6\n{n} {n}\n255\n").into_bytes();
        let sea = SEA_LEVEL * FP;
        for i in 0..n * n {
            let height = (h.height[i] - sea) as f64 / FP as f64;
            let (r, g, b) = if h.outlet[i] == OUTLET_SEA {
                (20, 50, 140)
            } else if h.lake_of[i] != NONE {
                (40, 90, 200)
            } else if h.river[i] {
                (60, 140, 255)
            } else {
                let at = |dx: i64, dz: i64| {
                    let (x, z) = ((i % n) as i64 + dx, (i / n) as i64 + dz);
                    let (x, z) = (
                        x.clamp(0, n as i64 - 1) as usize,
                        z.clamp(0, n as i64 - 1) as usize,
                    );
                    h.height[x + n * z] as f64 / FP as f64
                };
                let slope_x = at(1, 0) - at(-1, 0);
                let slope_z = at(0, 1) - at(0, -1);
                let shade =
                    (0.75 + 0.35 * (slope_x * -0.7 + slope_z * -0.7) / 2.0).clamp(0.35, 1.25);
                let v = ((height + 10.0) / 330.0).clamp(0.0, 1.0);
                let g = (70.0 + v * 150.0) * shade;
                let c = |f: f64| (g * f).clamp(0.0, 255.0) as u8;
                (c(0.7 + 0.3 * v), c(1.0), c(0.55 + 0.35 * v))
            };
            ppm.extend_from_slice(&[r, g, b]);
        }
        std::fs::write(dir.join(format!("seed{seed}.ppm")), ppm).unwrap();
        eprintln!(
            "landscape seed={seed} n={n} ms={ms} coastal={} basins={} lakes={} rivers={} springs={} falls={}",
            s.landform.is_coastal(),
            s.landform.basins.len(),
            h.lakes.len(),
            h.river.iter().filter(|&&r| r).count(),
            h.springs.len(),
            h.falls.len()
        );
    }
}
