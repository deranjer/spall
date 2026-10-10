//! A deep lake drained continuously must stay calm. A drain keeps water moving
//! for as long as it runs, so this is the regime the settle-to-rest tests never
//! reach; on real generated terrain the first defaults (safety factor 0.5,
//! damping 1.5/s) let waves grow without bound under it (ENG-168 play check).

use spall_core::{BrickCoord, CellSizeCode, GlobalCell, MaterialId, Revision, VolumeId};
use spall_fluid::columns::{ColumnConfig, ColumnWorld};
use spall_fluid::{DomainSpec, SolidBoundary};
use spall_voxel::{Brick, EditPlan, Volume};

const DT: f64 = 0.05;
const NX: i64 = 96;
const NZ: i64 = 96;
const NY: i64 = 64;

/// A bowl 30 cells deep with a gently shelving floor and uneven steps, so the
/// wetted areas vary the way real lake beds do.
fn bowl() -> SolidBoundary {
    let spec = DomainSpec::new(
        GlobalCell::new(0, 0, 0),
        [NX as u32, NY as u32, NZ as u32],
        (NX * NY * NZ) as usize,
    )
    .unwrap();
    let id = VolumeId::new(0x168).unwrap();
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
                let bed = 2.0 + (dx * dx + dz * dz).sqrt() * 0.12 + ((x / 4 + z / 6) % 3) as f64;
                if wall || (y as f64) < bed {
                    plan.set(GlobalCell::new(x, y, z), MaterialId(1));
                }
            }
        }
    }
    volume.apply_edit(&plan).unwrap();
    SolidBoundary::capture(&volume, spec).unwrap()
}

/// Worst deviation (cells) of a wet column's surface from the median of its
/// eight neighbours: a spike or a trough, regardless of the overall level.
fn roughness(world: &ColumnWorld) -> f64 {
    let top = |x: i64, z: i64| -> Option<f64> {
        world
            .spans_in_column(x, z)
            .iter()
            .filter(|s| s.depth > 0.02)
            .map(|s| s.floor_y as f64 + s.depth)
            .reduce(f64::max)
    };
    let mut worst = 0.0f64;
    for z in 2..NZ - 2 {
        for x in 2..NX - 2 {
            // The water right around the drain really is drawn down; judge the
            // rest of the lake.
            if (22..42).contains(&x) && (12..32).contains(&z) {
                continue;
            }
            let Some(h) = top(x, z) else { continue };
            let mut nb: Vec<f64> = [
                (1, 0),
                (-1, 0),
                (0, 1),
                (0, -1),
                (1, 1),
                (-1, -1),
                (1, -1),
                (-1, 1),
            ]
            .iter()
            .filter_map(|(dx, dz)| top(x + dx, z + dz))
            .collect();
            if nb.len() < 8 {
                continue;
            }
            nb.sort_by(f64::total_cmp);
            worst = worst.max((h - (nb[3] + nb[4]) / 2.0).abs());
        }
    }
    worst
}

fn drained_lake(config: ColumnConfig) -> (f64, f64, f64) {
    let boundary = bowl();
    let mut world = ColumnWorld::new(&boundary, config);
    for z in 1..NZ - 1 {
        for x in 1..NX - 1 {
            world.fill_column_to(x, z, 32.0);
        }
    }
    let start = world.total_volume();
    // A patch of drains low in the bowl: water flows toward it the whole run.
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
    let mut removed = 0.0;
    let mut worst_rough = 0.0f64;
    let mut worst_flow = 0.0f32;
    for step in 0..(150.0 / DT) as u32 {
        world.step(DT);
        for &s in &sinks {
            removed += world.drain_cell(s).unwrap_or(0.0);
        }
        worst_flow = worst_flow.max(world.max_abs_flow());
        if step % 200 == 199 {
            worst_rough = worst_rough.max(roughness(&world));
        }
    }
    let drift = (start - removed - world.total_volume()).abs() / start;
    (worst_rough, f64::from(worst_flow), drift)
}

#[test]
fn a_deep_lake_drained_continuously_stays_calm_and_conserves_volume() {
    let (rough, flow, drift) = drained_lake(ColumnConfig::default());
    println!("worst roughness {rough:.3} cells, worst flow {flow:.1}, drift {drift:.2e}");
    assert!(
        rough < 0.5,
        "waves grew under a sustained drain: worst spike {rough} cells"
    );
    assert!(flow < 400.0, "flow ran away: {flow}");
    assert!(drift < 1.0e-9, "volume drifted by {drift}");
}

/// The first defaults, kept as a negative control: if this ever stops being
/// unstable the test above no longer proves anything about the safety margin.
#[test]
#[ignore = "negative control for the stability fix; run with --ignored to confirm sensitivity"]
fn the_first_defaults_are_unstable_here() {
    let (rough, ..) = drained_lake(ColumnConfig {
        cfl: 0.5,
        damping_per_s: 1.5,
        ..ColumnConfig::default()
    });
    println!("first defaults: worst roughness {rough:.3} cells");
    assert!(
        rough > 1.0,
        "expected the old settings to go unstable: {rough}"
    );
}
