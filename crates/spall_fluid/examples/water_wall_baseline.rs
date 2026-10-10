//! ENG-164 (W0) failing baseline: does `grid_mac` level out a released pool, or
//! leave water standing as a wall beside empty space?
//!
//! Hand-built basin, 40 x 20 x 12 quarter-metre cells, closed top. Floor is 4
//! cells thick with a side pocket dug into it. A pool of water 8 cells deep sits
//! against the left wall with no divider, so it is released on the first step.
//! Reports, per simulated second, the column surface profile along x and the
//! largest surface-height step between adjacent wetted columns.
//!
//! `cargo run --release -p spall_fluid --example water_wall_baseline -- SECONDS`
use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::grid_mac::{MacConfig, MacGridWorld};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

const NX: i64 = 40;
const NY: i64 = 20;
const NZ: i64 = 12;
const FLOOR: i64 = 4;
const POOL_X: i64 = 12;
const POOL_DEPTH: i64 = 8;
const SOLID: MaterialId = MaterialId(1);

fn is_solid(x: i64, y: i64, z: i64) -> bool {
    if x == 0 || x == NX - 1 || z == 0 || z == NZ - 1 || y < FLOOR {
        // Dug side pocket: a 4x2x6 hole in the floor at the far right.
        let pocket =
            (30..34).contains(&x) && (FLOOR - 2..FLOOR).contains(&y) && (3..9).contains(&z);
        return !pocket;
    }
    false
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: f64 = std::env::args().nth(1).map_or(Ok(30.0), |s| s.parse())?;
    let spec = DomainSpec::new(
        GlobalCell::new(0, 0, 0),
        [NX as u32, NY as u32, NZ as u32],
        (NX * NY * NZ) as usize,
    )?;
    let id = VolumeId::new(0x164).unwrap();
    let mut volume = Volume::new(id, CellSizeCode::Quarter);
    for bz in 0..1 {
        for by in 0..1 {
            for bx in 0..2 {
                volume.insert_brick(
                    BrickCoord::new(bx, by, bz),
                    Brick::uniform(MaterialId::AIR, Revision(1)),
                )?;
            }
        }
    }
    let mut plan = EditPlan::new(id);
    for z in 0..NZ {
        for y in 0..NY {
            for x in 0..NX {
                if is_solid(x, y, z) {
                    plan.set(GlobalCell::new(x, y, z), SOLID);
                }
            }
        }
    }
    volume.apply_edit(&plan)?;
    let boundary = SolidBoundary::capture(&volume, spec)?;
    let config = MacConfig {
        open_top: false,
        ..MacConfig::default()
    };
    let mut grid = MacGridWorld::new(&boundary, config)?;
    for z in 1..NZ - 1 {
        for y in FLOOR..FLOOR + POOL_DEPTH {
            for x in 1..=POOL_X {
                grid.set_fraction(GlobalCell::new(x, y, z), 1.0)?;
            }
        }
    }
    let start_volume = grid.water_volume_m3();
    let dt = 1.0 / 60.0;
    let steps = (seconds / dt).round() as u32;
    let mid_z = NZ / 2;
    for step in 0..=steps {
        if step % 60 == 0 {
            // Surface height (cells above floor) per column, mid z row.
            let surface = |x: i64, z: i64| -> f64 {
                (FLOOR..NY)
                    .map(|y| grid.fraction_at(GlobalCell::new(x, y, z)).unwrap_or(0.0))
                    .sum()
            };
            let row: Vec<f64> = (1..NX - 1).map(|x| surface(x, mid_z)).collect();
            let mut max_step = 0.0f64;
            for z in 1..NZ - 1 {
                for x in 1..NX - 2 {
                    let (a, b) = (surface(x, z), surface(x + 1, z));
                    if a > 0.05 && b > 0.05 {
                        max_step = max_step.max((a - b).abs());
                    }
                }
            }
            let wet_edge = (1..NX - 1)
                .filter(|&x| surface(x, mid_z) > 0.05)
                .max()
                .unwrap_or(0);
            println!(
                "t={:6.2}s vol={:.4}/{:.4} m3 wet_edge_x={} max_adjacent_step={:.2} cells maxv={:.3} m/s",
                f64::from(step) * dt,
                grid.water_volume_m3(),
                start_volume,
                wet_edge,
                max_step,
                grid.max_liquid_speed_m_s()
            );
            if step % 600 == 0 || step == steps {
                let s: Vec<String> = row.iter().map(|v| format!("{v:.1}")).collect();
                println!("  mid-z surface (cells): {}", s.join(" "));
            }
        }
        if step < steps {
            grid.step(dt)?;
        }
    }
    Ok(())
}
