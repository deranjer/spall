//! Lazy deep-ocean regions: still sea far from any shore is a record, not a
//! solver region, and becomes a real one the moment anything touches it.

use std::sync::Arc;

use spall_core::GlobalCell;
use spall_fluid::columns::{
    ColumnConfig, ColumnWorld, REGION_EDGE, RegionKey, RegionTerrain, TerrainSource,
};

const HEIGHT: i32 = 16;
const DT: f64 = 1.0 / 20.0;
/// The sea block spans regions `-3..=3` on both axes; its interior is `-2..=2`.
const BLOCK: i32 = 3;
const SEA_TOP: i64 = 4;

/// A flat sea floor: solid at y = 0, open above.
struct Floor;

impl TerrainSource for Floor {
    fn y_range(&self) -> (i32, i32) {
        (0, HEIGHT)
    }

    fn region(&self, _key: RegionKey) -> Option<RegionTerrain> {
        let (h, edge) = (HEIGHT as usize, REGION_EDGE as usize);
        let mut solid = vec![false; edge * edge * h];
        for lz in 0..edge {
            for lx in 0..edge {
                solid[lx + edge * h * lz] = true;
            }
        }
        Some(RegionTerrain { solid })
    }
}

fn sea_columns() -> Vec<(i64, i64, i64, i64)> {
    let r = i64::from(BLOCK) * REGION_EDGE;
    let mut columns = Vec::new();
    for z in -r..(r + REGION_EDGE) {
        for x in -r..(r + REGION_EDGE) {
            columns.push((x, z, 0, SEA_TOP));
        }
    }
    columns
}

fn world(lazy: bool) -> ColumnWorld {
    let config = ColumnConfig {
        lazy_ocean: lazy,
        ..ColumnConfig::default()
    };
    let mut world = ColumnWorld::with_terrain(Arc::new(Floor), config);
    let rejected = world.seed_levels(&sea_columns());
    assert!(rejected.is_empty());
    world
}

type WetSpans = Vec<(RegionKey, Vec<(u16, i32, f64)>)>;

fn all_wet_spans(world: &ColumnWorld) -> WetSpans {
    let mut keys = world.region_keys();
    keys.sort_unstable();
    keys.into_iter()
        .map(|k| (k, world.region_wet_spans(k, 0.0)))
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

#[test]
fn a_still_sea_is_held_as_records_and_reports_the_same_water() {
    let eager = world(false);
    let lazy = world(true);
    let interior = (2 * BLOCK as usize - 1).pow(2);
    assert_eq!(lazy.dormant_region_count(), interior);
    assert_eq!(eager.dormant_region_count(), 0);
    assert_eq!(lazy.region_count() + interior, eager.region_count());
    assert!(lazy.memory().bytes * 4 < eager.memory().bytes * 3);
    // Same total, same wet spans per region: nothing is lost by not allocating.
    assert_eq!(lazy.total_volume(), eager.total_volume());
    assert_eq!(all_wet_spans(&lazy), all_wet_spans(&eager));
    assert_eq!(lazy.column_volume(5, 7), eager.column_volume(5, 7));
    let (with_flow_lazy, with_flow_eager) = (
        lazy.region_wet_spans_with_flow((0, 0), 0.0),
        eager.region_wet_spans_with_flow((0, 0), 0.0),
    );
    assert_eq!(with_flow_lazy, with_flow_eager);
}

#[test]
fn a_resting_sea_stays_dormant_while_it_is_stepped() {
    let mut lazy = world(true);
    let before = lazy.dormant_region_count();
    for _ in 0..200 {
        lazy.step(DT);
    }
    assert_eq!(lazy.dormant_region_count(), before);
}

#[test]
fn touching_a_dormant_region_makes_it_real_and_keeps_its_water() {
    let mut lazy = world(true);
    let total = lazy.total_volume();
    let cell = GlobalCell::new(3, 2, 5);
    lazy.add_water(cell, 6.0).unwrap();
    assert!(lazy.is_region_allocated((0, 0)));
    assert_eq!(
        lazy.dormant_region_count(),
        (2 * BLOCK as usize - 1).pow(2) - 1
    );
    assert!((lazy.total_volume() - (total + 6.0)).abs() < 1e-9);
    // The added water spreads, and the disturbance pulls in the neighbours it
    // reaches; the volume stays whole throughout.
    for _ in 0..400 {
        lazy.step(DT);
    }
    assert!(lazy.dormant_region_count() < (2 * BLOCK as usize - 1).pow(2) - 1);
    assert!((lazy.total_volume() - (total + 6.0)).abs() < 1e-6);
}

#[test]
fn a_disturbed_sea_evolves_like_the_eager_one() {
    let (mut lazy, mut eager) = (world(true), world(false));
    let cell = GlobalCell::new(3, 2, 5);
    lazy.add_water(cell, 6.0).unwrap();
    eager.add_water(cell, 6.0).unwrap();
    for _ in 0..300 {
        lazy.step(DT);
        eager.step(DT);
    }
    let r = i64::from(BLOCK) * REGION_EDGE;
    let mut worst: f64 = 0.0;
    for z in (-r..r).step_by(3) {
        for x in (-r..r).step_by(3) {
            worst = worst.max((lazy.column_volume(x, z) - eager.column_volume(x, z)).abs());
        }
    }
    assert!(worst < 1e-6, "columns differ by up to {worst}");
}

#[test]
fn a_sea_beside_a_shore_is_not_dormant() {
    // Regions whose neighbours are not all sea level keep real solver state.
    let mut columns = sea_columns();
    for c in &mut columns {
        if c.0 > 2 * REGION_EDGE + 10 {
            c.3 = SEA_TOP - 1;
        }
    }
    let mut world = ColumnWorld::with_terrain(
        Arc::new(Floor),
        ColumnConfig {
            lazy_ocean: true,
            ..ColumnConfig::default()
        },
    );
    world.seed_levels(&columns);
    assert!(world.dormant_region_count() < (2 * BLOCK as usize - 1).pow(2));
    assert!(!world.region_keys().is_empty());
}
