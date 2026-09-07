//! Offline authoritative-edit scenario harness for T08.
//!
//! Drives [`spall_sim::Simulation`] through the T08 acceptance scenarios
//! (`docs/tasks.md` T08, `docs/validation.md` `conflicting-edits` /
//! `rotating-body-cut` / `collision-commit`) with no GPU and no network, and
//! writes `summary.json`. Exit codes: `0` all pass, `1` a scenario failed, `2`
//! bad arguments.
//!
//! ```text
//! cargo run -p spall_sim --features scenario --bin sim-scenario -- --output .local/runs/t08
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use glam::DVec3;
use serde_json::{Value, json};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush, VolumeId};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{BodyPose, EditIntent, EditTarget, ExplosionImpulse, Simulation, SimulationConfig};
use spall_structure::AnchorPlane;

fn brush(x: i64, y: i64, z: i64, radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(x * BRUSH_UNIT + h, y * BRUSH_UNIT + h, z * BRUSH_UNIT + h),
        radius_cells * BRUSH_UNIT,
    )
    .expect("valid brush")
}

fn actor() -> EntityId {
    EntityId::new(1).expect("nonzero")
}

fn solid(sim: &Simulation, volume: VolumeId) -> u64 {
    sim.world()
        .volume_ref(volume)
        .map(spall_sim::world::solid_cells)
        .unwrap_or(0)
}

struct Scenario {
    name: &'static str,
    passed: bool,
    detail: Value,
}

fn terrain_split() -> Scenario {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).expect("world");
    let before = sim.world().total_solid_cells();
    let req = RequestId(1);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Terrain,
        brush(10, 4, 1, 2),
    ))
    .expect("submit");
    sim.run_until_idle(16).expect("ticks");

    let committed = sim.committed(req).is_some();
    let children = sim.world().body_count();
    let after = sim.world().total_solid_cells();
    let destroyed = before - after;
    Scenario {
        name: "terrain-cut-split",
        passed: committed && children == 1 && destroyed > 0 && destroyed < before,
        detail: json!({
            "committed": committed,
            "children": children,
            "solid_before": before,
            "solid_after": after,
            "destroyed_by_brush": destroyed,
        }),
    }
}

fn rotating_body_cut() -> Scenario {
    let mut setup = fixtures::flat_terrain_setup();
    setup.anchor = AnchorPlane::at(-100_000);
    let mut sim = Simulation::new(SimulationConfig::new(setup)).expect("world");

    let pose = BodyPose::new(fixtures::oblique_spin(), [4.0, 8.0, 4.0]);
    let entity = sim
        .world_mut()
        .spawn_body(
            fixtures::dumbbell(4, 3),
            pose,
            [1.5, 0.0, -0.5],
            [0.0, 0.8, 0.0],
            2600.0,
            0,
        )
        .expect("spawn");
    let body_vid = sim.world().body(entity).expect("body").volume_id;
    let before = solid(&sim, body_vid);

    let req = RequestId(10);
    sim.submit(EditIntent::cut(
        req,
        actor(),
        EditTarget::Body(entity),
        brush(5, 2, 2, 2),
    ))
    .expect("submit");
    sim.run_until_idle(16).expect("ticks");

    let committed = sim.committed(req).is_some();
    let bodies = sim.world().body_count();
    let after: u64 = sim
        .world()
        .bodies()
        .map(|b| {
            b.volume
                .resident_brick_coords()
                .iter()
                .map(|c| {
                    let s = b
                        .volume
                        .snapshot_brick(*c)
                        .ok()
                        .flatten()
                        .expect("resident");
                    (0..spall_core::CELLS_PER_BRICK as u16)
                        .filter(|&i| {
                            !s.get(spall_core::LocalCell::from_linear_index(i).expect("in range"))
                                .is_air()
                        })
                        .count() as u64
                })
                .sum::<u64>()
        })
        .sum();

    Scenario {
        name: "rotating-body-cut",
        passed: committed && bodies == 2 && after < before && after + 8 >= before,
        detail: json!({
            "committed": committed,
            "bodies_after": bodies,
            "solid_before": before,
            "solid_after": after,
        }),
    }
}

fn conflicting_cuts() -> Scenario {
    let build =
        || Simulation::new(SimulationConfig::new(fixtures::flat_terrain_setup())).expect("world");
    let mut sim = build();
    let terrain = sim.world().terrain_volume_id();
    let a = RequestId(100);
    let b = RequestId(101);
    sim.submit(EditIntent::cut(
        a,
        actor(),
        EditTarget::Terrain,
        brush(6, 1, 6, 2),
    ))
    .expect("a");
    sim.submit(EditIntent::cut(
        b,
        actor(),
        EditTarget::Terrain,
        brush(7, 1, 6, 2),
    ))
    .expect("b");
    let reports = sim.run_until_idle(24).expect("ticks");

    let both = sim.committed(a).is_some() && sim.committed(b).is_some();
    let retried = reports.iter().any(|r| r.retried.contains(&b));
    let ordered = match (sim.committed(a), sim.committed(b)) {
        (Some(ca), Some(cb)) => ca.transaction.get() < cb.transaction.get(),
        _ => false,
    };

    let mut reference = build();
    reference
        .submit(EditIntent::cut(
            a,
            actor(),
            EditTarget::Terrain,
            brush(6, 1, 6, 2),
        ))
        .expect("a");
    reference.run_until_idle(8).expect("ticks");
    reference
        .submit(EditIntent::cut(
            b,
            actor(),
            EditTarget::Terrain,
            brush(7, 1, 6, 2),
        ))
        .expect("b");
    reference.run_until_idle(8).expect("ticks");
    let converged = sim.world().volume_hash(terrain)
        == reference
            .world()
            .volume_hash(reference.world().terrain_volume_id());

    Scenario {
        name: "conflicting-cuts-converge",
        passed: both && retried && ordered && converged,
        detail: json!({
            "both_committed": both,
            "second_retried": retried,
            "commit_order_ascending": ordered,
            "hash_matches_sequential": converged,
        }),
    }
}

fn no_second_impulse() -> Scenario {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup())).expect("world");
    let decoy = RequestId(200);
    let split = RequestId(201);
    sim.submit(EditIntent::cut(
        decoy,
        actor(),
        EditTarget::Terrain,
        brush(11, 6, 2, 1),
    ))
    .expect("decoy");
    sim.submit(
        EditIntent::cut(split, actor(), EditTarget::Terrain, brush(10, 4, 1, 2)).with_explosion(
            ExplosionImpulse {
                magnitude_ns: 400.0,
                direction: [1.0, 0.0, 0.0],
            },
        ),
    )
    .expect("split");
    let reports = sim.run_until_idle(24).expect("ticks");

    let retries = reports
        .iter()
        .filter(|r| r.retried.contains(&split))
        .count();
    let committed = sim.committed(split).is_some();
    let child_entity = sim
        .committed(split)
        .and_then(|c| c.children.first().copied());
    let (bodies, speed, single_dv) = match child_entity {
        Some(e) => {
            let child = sim.world().body(e).expect("child");
            let cell_m = child.volume.cell_size().metres();
            let mass = solid(&sim, child.volume_id) as f64 * cell_m.powi(3) * 2600.0;
            let snap = sim
                .journal()
                .last()
                .and_then(|j| j.participants.iter().find(|p| p.body == e).copied());
            let speed = snap
                .map(|s| {
                    DVec3::new(
                        s.linear_velocity[0] as f64,
                        s.linear_velocity[1] as f64,
                        s.linear_velocity[2] as f64,
                    )
                    .length()
                })
                .unwrap_or(0.0);
            (sim.world().body_count(), speed, 400.0 / mass.max(1e-6))
        }
        None => (sim.world().body_count(), 0.0, 1.0),
    };

    Scenario {
        name: "no-second-impulse-on-retry",
        passed: committed
            && retries >= 1
            && bodies == 1
            && speed > 0.3 * single_dv
            && speed < 1.8 * single_dv + 0.5,
        detail: json!({
            "committed": committed,
            "retry_count": retries,
            "bodies": bodies,
            "child_speed_m_s": speed,
            "single_impulse_m_s": single_dv,
        }),
    }
}

fn main() -> ExitCode {
    let mut output: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!("sim-scenario --output <DIR>");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("sim-scenario: unexpected argument {other:?}");
                return ExitCode::from(2);
            }
        }
    }
    let output =
        output.unwrap_or_else(|| PathBuf::from(format!(".local/runs/sim-{}", std::process::id())));
    if let Err(e) = std::fs::create_dir_all(&output) {
        eprintln!("sim-scenario: cannot create {}: {e}", output.display());
        return ExitCode::from(2);
    }

    let scenarios = [
        terrain_split(),
        rotating_body_cut(),
        conflicting_cuts(),
        no_second_impulse(),
    ];
    let all_pass = scenarios.iter().all(|s| s.passed);
    let summary = json!({
        "version": 1,
        "result": if all_pass { "passed" } else { "failed" },
        "platform": std::env::consts::OS,
        "scenarios": scenarios
            .iter()
            .map(|s| json!({ "name": s.name, "passed": s.passed, "detail": s.detail }))
            .collect::<Vec<_>>(),
    });
    let path = output.join("summary.json");
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&summary).expect("serialisable"),
    ) {
        eprintln!("sim-scenario: cannot write {}: {e}", path.display());
        return ExitCode::from(2);
    }
    for s in &scenarios {
        println!("{}: {}", s.name, if s.passed { "pass" } else { "FAIL" });
    }
    println!("summary: {}", path.display());
    if all_pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
