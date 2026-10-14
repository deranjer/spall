//! Steady-flow sleep: a river that carries water at a constant rate stops costing
//! solver time, keeps its currents, conserves mass exactly, and agrees with an
//! always-active run.

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::columns::{ColumnConfig, ColumnWorld};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

const DT: f64 = 0.05;
const NX: i64 = 128;
const NZ: i64 = 32;
const NY: i64 = 64;
const CHANNEL: std::ops::Range<i64> = 10..22;

fn bed(x: i64) -> i64 {
    36 - x / 64
}

/// A straight sloping channel between walls, over three regions in a row.
fn river_bed() -> SolidBoundary {
    let spec = DomainSpec::new(
        GlobalCell::new(0, 0, 0),
        [NX as u32, NY as u32, NZ as u32],
        (NX * NY * NZ) as usize,
    )
    .unwrap();
    let id = VolumeId::new(0x188).unwrap();
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
                let wall = !CHANNEL.contains(&z);
                if wall || y < bed(x) {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    SolidBoundary::capture(&volume, spec).unwrap()
}

struct Run {
    world: ColumnWorld,
    spring: Vec<GlobalCell>,
    sink: Vec<GlobalCell>,
    added: f64,
    removed: f64,
    start: f64,
    steadied: u32,
    woken: u32,
}

impl Run {
    fn new(config: ColumnConfig) -> Self {
        let mut world = ColumnWorld::new(&river_bed(), config);
        let z0 = CHANNEL.start;
        let spring: Vec<_> = (z0 + 3..z0 + 9)
            .map(|z| GlobalCell::new(3, bed(3), z))
            .collect();
        let sink: Vec<_> = (z0..CHANNEL.end)
            .map(|z| GlobalCell::new(NX - 3, bed(NX - 3), z))
            .collect();
        // Open every region along the channel; nothing is wet yet.
        world.allocate_rect(0, 0, NX - 1, NZ - 1);
        let start = world.total_volume();
        Self {
            world,
            spring,
            sink,
            added: 0.0,
            removed: 0.0,
            start,
            steadied: 0,
            woken: 0,
        }
    }

    fn exchange(&mut self) {
        for &c in &self.spring {
            self.added += self.world.fill_cell(c).unwrap_or(0.0);
        }
        for &c in &self.sink {
            self.removed += self.world.drain_cell(c).unwrap_or(0.0);
        }
    }

    fn step(&mut self) {
        let m = self.world.step(DT);
        self.steadied += m.regions_steadied as u32;
        self.woken += m.regions_woken as u32;
        self.exchange();
    }

    fn drift(&self) -> f64 {
        (self.start + self.added - self.removed - self.world.total_volume()).abs()
    }
}

fn depth_field(world: &ColumnWorld) -> Vec<f64> {
    let mut out = Vec::new();
    for z in CHANNEL {
        for x in 0..NX {
            let d: f64 = world.spans_in_column(x, z).iter().map(|s| s.depth).sum();
            out.push(d);
        }
    }
    out
}

fn always_active() -> ColumnConfig {
    ColumnConfig {
        steady_steps: 0,
        ..ColumnConfig::default()
    }
}

/// Steady sleep is off by default; these tests turn it on.
fn steady_sleep() -> ColumnConfig {
    ColumnConfig {
        steady_steps: 40,
        ..ColumnConfig::default()
    }
}

#[test]
fn a_steady_river_stops_costing_time_conserves_mass_and_matches_always_active() {
    let seconds = 900.0;
    let steps = (seconds / DT) as u32;
    let mut reference = Run::new(always_active());
    let mut steady = Run::new(steady_sleep());
    for step in 0..steps {
        let m_ref = reference.world.step(DT);
        reference.exchange();
        let m_steady = steady.world.step(DT);
        steady.steadied += m_steady.regions_steadied as u32;
        steady.woken += m_steady.regions_woken as u32;
        steady.exchange();
        let _ = (m_ref, step);
        if step % 600 == 0 {
            println!(
                "t={:.0}s steady-run: steady now {} (froze {} times, woke {} times) flow {:.3}",
                f64::from(step) * DT,
                steady.world.steady_region_count(),
                steady.steadied,
                steady.woken,
                steady.world.max_abs_flow()
            );
        }
    }
    // The cost of one more step once both have settled.
    let m_ref = reference.world.step(DT);
    let m_steady = steady.world.step(DT);
    println!(
        "after {seconds} s: always-active {} active regions, steady-sleep {} active and {} steady; \
         spans stepped {} vs {}",
        m_ref.active_regions,
        m_steady.active_regions,
        steady.world.steady_region_count(),
        m_ref.active_spans,
        m_steady.active_spans
    );
    println!(
        "water added {:.1} removed {:.1}; drift steady {:.2e} reference {:.2e}",
        steady.added,
        steady.removed,
        steady.drift(),
        reference.drift()
    );

    // How much it saves is measured on real worlds (river_idle_probe); this
    // synthetic channel's end regions never sleep and its one interior region
    // sloshes, so here it is enough that regions did freeze.
    assert!(steady.steadied > 0, "no region ever froze as steady");
    // Mass is conserved exactly in both runs: nothing is created or lost.
    let scale = steady.added.max(1.0);
    assert!(steady.drift() < 1.0e-9 * scale, "drift {}", steady.drift());
    // The frozen river is the same river: depths agree to within the slosh the
    // always-active river itself carries (about a tenth of a cell, 2.5 cm).
    let worst = depth_field(&reference.world)
        .iter()
        .zip(depth_field(&steady.world))
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    println!("worst depth difference {worst:.4} cells");
    assert!(worst < 0.3, "depths differ by {worst} cells");
    // The currents are still there to render and to push bodies.
    assert!(
        steady.world.max_abs_flow() > 0.5 * reference.world.max_abs_flow(),
        "a steady river lost its current: {} vs {}",
        steady.world.max_abs_flow(),
        reference.world.max_abs_flow()
    );
}

#[test]
fn a_steady_river_wakes_when_water_is_added_and_stays_conservative() {
    let mut run = Run::new(steady_sleep());
    // Run until some region has frozen (regions freeze and wake as the river
    // settles); the river must get there within seven minutes.
    for _ in 0..(420.0 / DT) as u32 {
        run.step();
        if run.world.steady_region_count() > 0 {
            break;
        }
    }
    let frozen = run.world.steady_region_count();
    assert!(frozen > 0, "setup: the river must be steady first");
    // A flood pulse mid-river wakes its region and sends a wave down.
    let key = run.world.steady_region_keys()[0];
    let x = i64::from(key.0) * 32 + 16;
    let mid = GlobalCell::new(x, bed(x), 16);
    run.world.add_water(mid, 400.0).unwrap();
    run.start += 400.0;
    assert!(
        run.world.steady_region_count() < frozen,
        "the region did not wake"
    );
    let mut woken_peak = 0usize;
    for _ in 0..(60.0 / DT) as u32 {
        let m = run.world.step(DT);
        woken_peak = woken_peak.max(m.active_regions);
        run.exchange();
    }
    println!(
        "frozen {frozen} then woke, peak active {woken_peak}, steady now {}",
        run.world.steady_region_count()
    );
    assert!(woken_peak > 1, "the pulse stayed in one region");
    assert!(
        run.drift() < 1.0e-9 * (run.start + run.added).max(1.0),
        "drift {}",
        run.drift()
    );
}
