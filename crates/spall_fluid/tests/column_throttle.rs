//! Far water steps less often, and catches up when a player comes back. A lake
//! drained far from every player must still drain (more slowly), lose or gain
//! no water on the way, and run its missed time when the focus returns.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::columns::{ColumnConfig, ColumnWorld, region_of};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

const DT: f64 = 0.05;
const NX: i64 = 96;
const NZ: i64 = 96;
const NY: i64 = 64;
/// A point (metres) a world's width and more away from the basin.
const FAR_M: [f64; 2] = [5000.0, 5000.0];
const NEAR_M: [f64; 2] = [12.0, 12.0];

fn bowl() -> SolidBoundary {
    let spec = DomainSpec::new(
        GlobalCell::new(0, 0, 0),
        [NX as u32, NY as u32, NZ as u32],
        (NX * NY * NZ) as usize,
    )
    .unwrap();
    let id = VolumeId::new(0x169).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..(NZ + 31) / 32 {
        for by in 0..(NY + 31) / 32 {
            for bx in 0..(NX + 31) / 32 {
                volume
                    .insert_brick(
                        BrickCoord::new(bx, by, bz),
                        Brick::uniform(MaterialId::AIR, Revision(1)),
                    )
                    .unwrap();
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in 0..NZ {
        for y in 0..NY {
            for x in 0..NX {
                let wall = x == 0 || z == 0 || x == NX - 1 || z == NZ - 1;
                let (dx, dz) = ((x - 48) as f64, (z - 48) as f64);
                let bed = 2.0 + (dx * dx + dz * dz).sqrt() * 0.12;
                if wall || (y as f64) < bed {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    SolidBoundary::capture(&volume, spec).unwrap()
}

fn throttled() -> ColumnConfig {
    ColumnConfig {
        // Within 100 cells (25 m) of the focus every region is near: the whole
        // basin when the focus is on it. Far beyond that, 2x, 4x, 8x slower.
        throttle_from_cells: [100.0, 200.0, 400.0],
        catch_up_substeps: 24,
        ..ColumnConfig::default()
    }
}

struct Lake {
    world: ColumnWorld,
    sinks: Vec<GlobalCell>,
    start: f64,
    removed: f64,
}

fn lake(config: ColumnConfig) -> Lake {
    let boundary = bowl();
    let mut world = ColumnWorld::new(&boundary, config);
    for z in 1..NZ - 1 {
        for x in 1..NX - 1 {
            world.fill_column_to(x, z, 32.0);
        }
    }
    let start = world.total_volume();
    let sinks: Vec<GlobalCell> = (0..4)
        .flat_map(|dz| (0..4).map(move |dx| (30 + dx, 20 + dz)))
        .filter_map(|(x, z)| {
            world
                .spans_in_column(x, z)
                .first()
                .map(|s| GlobalCell::new(x, s.floor_y, z))
        })
        .collect();
    assert!(sinks.len() >= 12);
    Lake {
        world,
        sinks,
        start,
        removed: 0.0,
    }
}

impl Lake {
    fn run(&mut self, seconds: f64) {
        for _ in 0..(seconds / DT) as u32 {
            self.world.step(DT);
            for &s in &self.sinks {
                self.removed += self.world.drain_cell(s).unwrap_or(0.0);
            }
        }
    }

    fn drift(&self) -> f64 {
        (self.start - self.removed - self.world.total_volume()).abs() / self.start
    }

    /// Mean surface height (cells) of three columns well away from the drains:
    /// it falls as the water flows toward them.
    fn level(&self) -> f64 {
        [(60, 40), (70, 60), (50, 80)]
            .iter()
            .map(|&(x, z)| {
                self.world
                    .spans_in_column(x, z)
                    .iter()
                    .map(|s| s.floor_y as f64 + s.depth)
                    .fold(0.0, f64::max)
            })
            .sum::<f64>()
            / 3.0
    }

    fn owed(&self) -> f32 {
        (0..3)
            .flat_map(|x| (0..3).map(move |z| region_of(x * 32, z * 32)))
            .map(|k| self.world.region_owed_s(k))
            .fold(0.0, f32::max)
    }
}

#[test]
fn far_water_drains_more_slowly_but_still_drains_and_conserves_volume() {
    let mut full = lake(ColumnConfig::default());
    full.run(40.0);

    let mut far = lake(throttled());
    far.world.set_focus(&[FAR_M]);
    far.run(40.0);

    let (full_level, far_level) = (full.level(), far.level());
    assert!(far_level < 31.9, "the far lake drained: {far_level}");
    assert!(
        far_level > full_level + 0.2,
        "far water is behind: {far_level} against {full_level}"
    );
    assert!(far.world.resting_region_count() > 0, "regions rested");
    assert!(far.owed() > 1.0, "the debt shows: {} s", far.owed());
    assert!(far.drift() < 1.0e-9, "throttled drift {}", far.drift());
    assert!(full.drift() < 1.0e-9);

    // It still finishes: given long enough the far lake falls as far too.
    far.run(120.0);
    assert!(
        far.level() < full_level,
        "the far lake went on draining: {} after 160 s against {full_level} after 40 s",
        far.level()
    );
    assert!(
        far.drift() < 1.0e-9,
        "drift after a long run {}",
        far.drift()
    );
}

#[test]
fn no_focus_or_an_unthrottled_config_changes_nothing() {
    let mut full = lake(ColumnConfig::default());
    full.run(20.0);
    // No focus point at all: nobody to keep the water up to date for.
    let mut none = lake(throttled());
    none.run(20.0);
    assert_eq!(none.world.resting_region_count(), 0);
    assert_eq!(none.removed.to_bits(), full.removed.to_bits());
    // A focus but the bands off (the default): nothing rests either.
    let mut off = lake(ColumnConfig::default());
    off.world.set_focus(&[FAR_M]);
    off.run(20.0);
    assert_eq!(off.world.resting_region_count(), 0);
    assert_eq!(off.removed.to_bits(), full.removed.to_bits());
    // A focus on the basin: every region is near, so none rests.
    let mut near = lake(throttled());
    near.world.set_focus(&[NEAR_M]);
    near.run(20.0);
    assert_eq!(near.world.resting_region_count(), 0);
    assert_eq!(near.removed.to_bits(), full.removed.to_bits());
}

#[test]
fn a_region_that_is_near_again_runs_the_time_it_missed() {
    let mut full = lake(ColumnConfig::default());
    full.run(80.0);

    let mut late = lake(throttled());
    late.world.set_focus(&[FAR_M]);
    late.run(40.0);
    let behind = late.owed();
    assert!(behind > 1.0, "behind by {behind} s");
    let level_far = late.level();

    // The player comes back to the lake: its regions are near, and catch up.
    late.world.set_focus(&[NEAR_M]);
    late.run(40.0);
    assert!(late.drift() < 1.0e-9, "drift {}", late.drift());
    assert!(late.level() < level_far, "the lake kept draining");
    assert!(
        late.owed() < behind * 0.5,
        "the debt was worked down: {} s from {behind} s",
        late.owed()
    );
    // And it is closer to a lake that was never throttled than one that stayed
    // far for all 80 s.
    let mut still_far = lake(throttled());
    still_far.world.set_focus(&[FAR_M]);
    still_far.run(80.0);
    let gap = |l: &Lake| (full.level() - l.level()).abs();
    println!(
        "levels after 80 s: never throttled {:.2}, caught up {:.2}, stayed far {:.2}; debt {:.1} s from {behind:.1} s",
        full.level(),
        late.level(),
        still_far.level(),
        late.owed()
    );
    assert!(
        gap(&late) < gap(&still_far),
        "caught up: {} short of the control, against {} when it stayed far",
        gap(&late),
        gap(&still_far)
    );
}
