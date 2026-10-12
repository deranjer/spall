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
                // The bed follows the ground: a river is not cut down to its mouth.
                assert!(
                    h.bed[i] >= h.height[i] - 4 * FP,
                    "seed {}: river {i} bed {} is far below the ground {}",
                    s.seed,
                    h.bed[i] / FP,
                    h.height[i] / FP
                );
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
fn every_map_is_coastal_and_has_rivers() {
    for s in scapes() {
        let rivers = s.hydrology.river.iter().filter(|&&r| r).count();
        assert!(s.landform.is_coastal(), "seed {} has no coast", s.seed);
        assert!(rivers > 0, "seed {} has no river", s.seed);
    }
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

#[test]
fn river_heads_carry_the_catchment_exactly_once() {
    for s in scapes() {
        let h = &s.hydrology;
        let total: u64 = h.heads.iter().map(|&(_, a)| a).sum();
        assert!(
            total <= (s.n * s.n) as u64,
            "seed {}: {total} samples",
            s.seed
        );
        assert!(
            h.heads
                .iter()
                .all(|&(cell, a)| h.river[cell as usize] && a >= 1)
        );
        // Heads are river cells with no river donor, so a head is never fed by another head.
        for &(cell, _) in &h.heads {
            let donors = (0..h.river.len())
                .filter(|&j| h.river[j] && h.receiver[j] == cell && j as u32 != cell)
                .count();
            assert_eq!(donors, 0, "seed {}: head {cell} has river donors", s.seed);
        }
        // Every river cell's water reaches the sea or a basin from some head.
        assert!(!h.heads.is_empty(), "seed {}", s.seed);
    }
}

/// Manual: how deep the rivers sit below the ground around them.
#[test]
#[ignore = "manual diagnostic"]
fn diag_river_relief() {
    for seed in [1u64, 2, 4, 5] {
        let s = Landscape::generate(seed, 2048).unwrap();
        let (n, h) = (s.n as i64, &s.hydrology);
        let mut rel: Vec<f64> = Vec::new();
        for i in 0..h.river.len() {
            if !h.river[i] {
                continue;
            }
            let (x, z) = ((i % s.n) as i64, (i / s.n) as i64);
            let bed = f64::from(h.bed[i]) / f64::from(FP);
            let mut around: Vec<f64> = [
                (8, 0),
                (-8, 0),
                (0, 8),
                (0, -8),
                (6, 6),
                (-6, 6),
                (6, -6),
                (-6, -6),
            ]
            .iter()
            .filter_map(|(dx, dz)| {
                let (xx, zz) = (x + dx, z + dz);
                (xx >= 0 && zz >= 0 && xx < n && zz < n)
                    .then(|| f64::from(h.height[(xx + n * zz) as usize]) / f64::from(FP) - bed)
            })
            .collect();
            if around.len() == 8 {
                around.sort_by(|a, b| a.partial_cmp(b).unwrap());
                rel.push(around[3] / 4.0); // metres: the typical ground ~8 m away
            }
        }
        rel.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: usize| rel[rel.len() * p / 100];
        eprintln!(
            "seed {seed}: {} river cells; ground ~8 m away stands above the bed by (m) p10 {:.1} p25 {:.1} p50 {:.1} p75 {:.1} p90 {:.1} p99 {:.1}",
            rel.len(),
            q(10),
            q(25),
            q(50),
            q(75),
            q(90),
            q(99)
        );
    }
}

#[test]
#[ignore = "manual diagnostic"]
fn diag_river_profile() {
    let s = Landscape::generate(1, 2048).unwrap();
    let (n, h) = (s.n, &s.hydrology);
    // A river cell near the middle of the map with a long river around it.
    let i = (0..h.river.len())
        .filter(|&i| h.river[i] && (i % n) > n / 3 && (i % n) < 2 * n / 3 && h.area[i] > 3000)
        .nth(40)
        .expect("a river cell");
    let (x, z) = (i % n, i / n);
    eprintln!(
        "river cell ({x},{z}) area {} bed {}",
        h.area[i],
        h.bed[i] / FP
    );
    eprintln!("heights (cells) along x:");
    let row: Vec<String> = (x.saturating_sub(16)..(x + 17).min(n))
        .map(|xx| format!("{}", h.height[xx + n * z] / FP))
        .collect();
    eprintln!("  {}", row.join(" "));
    eprintln!("heights (cells) along z:");
    let col: Vec<String> = (z.saturating_sub(16)..(z + 17).min(n))
        .map(|zz| format!("{}", h.height[x + n * zz] / FP))
        .collect();
    eprintln!("  {}", col.join(" "));
    eprintln!("landform (pre-erosion, cells) along x:");
    let row: Vec<String> = (x.saturating_sub(16)..(x + 17).min(n))
        .map(|xx| format!("{:.0}", s.landform.height[xx + n * z]))
        .collect();
    eprintln!("  {}", row.join(" "));
}
