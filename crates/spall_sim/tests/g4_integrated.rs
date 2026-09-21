//! T23 / G4 integrated workload ("demolition yard"): population, geometry, and
//! edit-stream checks on the real `Simulation`, plus ignored CPU measurements
//! (`--ignored --nocapture`).

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures::{self, G4_ACTIVE_BODY_COUNT, G4_SLEEPING_BODY_COUNT};
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::fixtures::{self as vfix, G4BodyEdit};

fn brush_cell(c: [i64; 3], radius_cells: i64) -> SphereBrush {
    let h = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            c[0] * BRUSH_UNIT + h,
            c[1] * BRUSH_UNIT + h,
            c[2] * BRUSH_UNIT + h,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .unwrap()
}

fn actor() -> EntityId {
    EntityId::new(1).unwrap()
}

fn body_cut(rid: u64, e: G4BodyEdit) -> EditIntent {
    EditIntent::cut(
        RequestId(rid),
        actor(),
        EditTarget::Body(EntityId::new(e.entity).unwrap()),
        brush_cell(e.cell, e.radius),
    )
}

fn scene() -> (Simulation, fixtures::G4IntegratedBodies) {
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    if std::env::var("TERRAIN_BRICKS").is_ok() {
        sim.world_mut().enable_terrain_brick_colliders().unwrap();
    }
    let bodies = fixtures::spawn_g4_integrated_bodies(
        sim.world_mut(),
        fixtures::G4_INTEGRATED_SEPARATED_SPAWNS[0],
    );
    (sim, bodies)
}

fn run(sim: &mut Simulation, ticks: u64) -> (usize, Vec<(RequestId, String)>) {
    let mut committed = 0;
    let mut rejected = Vec::new();
    for _ in 0..ticks {
        let r = sim.tick().unwrap();
        committed += r.committed.len();
        rejected.extend(r.rejected.clone());
    }
    (committed, rejected)
}

/// The population is what the gate asks for, sits over the ground slab, and is
/// inside the 256 m world envelope (the earlier fixture parked debris at
/// 400 m / 700 m, outside it and over nothing). The entity-id map the harness
/// generator relies on matches the real registry.
#[test]
fn integrated_population_is_complete_grounded_and_in_bounds() {
    let (sim, bodies) = scene();
    assert_eq!(bodies.active.len(), G4_ACTIVE_BODY_COUNT);
    assert_eq!(bodies.sleeping_total, G4_SLEEPING_BODY_COUNT);
    assert!(
        bodies.active_near_observer >= fixtures::G4_NEAR_OBSERVER_BODY_COUNT,
        "near-observer bodies: {}",
        bodies.active_near_observer
    );
    // "Nontrivial": the active mix is 64-, 56- and 28-cell shapes.
    assert!(bodies.active_cells / bodies.active.len() as u64 >= 40);
    assert!(bodies.giant_cells >= 2_097_152, "64 bricks of solid stone");

    // Entity ids the generator hard-codes.
    assert_eq!(bodies.giant.unwrap().get(), vfix::G4_ENTITY_FIRST);
    for (c, e) in bodies.combs.iter().enumerate() {
        assert_eq!(e.get(), vfix::g4_comb_entity(c as u64), "comb {c}");
    }
    for (t, e) in bodies.towers.iter().enumerate() {
        assert_eq!(e.get(), vfix::g4_tower_entity(t as u64), "tower {t}");
    }

    let world_m = 256.0;
    let mut n = 0usize;
    for b in sim.world().bodies() {
        let t = b.pose.translation_m;
        assert!(
            (0.0..world_m).contains(&t[0]) && (0.0..world_m).contains(&t[2]) && t[1] >= 0.0,
            "body at {t:?} is outside the world bounds"
        );
        // The origin corner is over the 96 m x 56 m slab.
        assert!(t[0] < 96.0 && t[2] < 56.0, "body at {t:?} is off the slab");
        n += 1;
    }
    assert_eq!(
        n,
        1 + vfix::G4_COMB_COUNT
            + vfix::G4_TOWER_COUNT
            + G4_ACTIVE_BODY_COUNT
            + G4_SLEEPING_BODY_COUNT
    );
    // Persistent destructibles start dormant; the 256 debris start awake.
    assert_eq!(
        sim.world().dormant_body_count(),
        1 + vfix::G4_COMB_COUNT + vfix::G4_TOWER_COUNT + G4_SLEEPING_BODY_COUNT
    );
}

/// The edit generators supply at least the 30-minute lane's 18,000 distinct
/// ordinary edits and 180 distinct blasts.
#[test]
fn edit_geometry_capacity_covers_the_thirty_minute_lane() {
    assert!(vfix::g4_comb_cut_capacity() >= 18_000);
    let mut seen = std::collections::HashSet::new();
    for i in 0..vfix::g4_comb_cut_capacity() as u64 {
        let e = vfix::g4_ordinary_edit(i).expect("within capacity");
        assert!(seen.insert((e.entity, e.cell)), "edit {i} repeats {e:?}");
        assert!((2..=vfix::G4_COMB_COUNT as u64 + 1).contains(&e.entity));
    }
    assert!(vfix::g4_ordinary_edit(vfix::g4_comb_cut_capacity() as u64).is_none());
    let mut blasts = std::collections::HashSet::new();
    for i in 0..(vfix::G4_TOWER_COUNT as i64 * vfix::G4_TOWER_BLASTS) as u64 {
        let e = vfix::g4_blast(i).expect("tower available");
        assert_eq!(e.radius, vfix::G4_BLAST_RADIUS_CELLS);
        assert!(blasts.insert((e.entity, e.cell)), "blast {i} repeats");
    }
    assert_eq!(blasts.len(), 180, "180 blasts in the 30-minute lane");
    assert!(vfix::g4_blast(180).is_none());
    let towers: std::collections::HashSet<_> = blasts.iter().map(|(e, _)| *e).collect();
    assert_eq!(towers.len(), vfix::G4_TOWER_COUNT, "three blasts per tower");
}

/// A comb cut wakes the dormant comb, commits, and detaches exactly the tooth
/// tip; a tower blast detaches the upper shaft; the giant cut detaches the
/// whole 64-brick block as one body.
#[test]
fn a_comb_cut_a_tower_blast_and_the_giant_cut_each_commit_and_detach() {
    let (mut sim, _) = scene();
    let bodies_before = sim.world().body_count();

    let e = vfix::g4_ordinary_edit(0).unwrap();
    sim.submit(body_cut(1, e)).unwrap();
    let (committed, rejected) = run(&mut sim, 8);
    assert_eq!(
        (committed, rejected.len()),
        (1, 0),
        "comb cut: {rejected:?}"
    );
    assert_eq!(sim.world().body_count(), bodies_before + 1, "one tooth tip");
    let comb = sim.world().body(EntityId::new(e.entity).unwrap()).unwrap();
    assert!(!comb.dormant, "the edit woke the comb");

    let b = vfix::g4_blast(0).unwrap();
    sim.submit(body_cut(2, b)).unwrap();
    let (committed, rejected) = run(&mut sim, 8);
    assert_eq!((committed, rejected.len()), (1, 0), "blast: {rejected:?}");
    assert_eq!(
        sim.world().body_count(),
        bodies_before + 2,
        "the upper shaft"
    );
    let shed = sim
        .world()
        .bodies()
        .max_by_key(|b| b.entity.map(|e| e.get()))
        .unwrap();
    assert!(
        spall_sim::world::solid_cells(&shed.volume) >= 400,
        "the blast shed a large segment"
    );

    let g = vfix::g4_giant_cut();
    let t = std::time::Instant::now();
    sim.submit(body_cut(3, g)).unwrap();
    let (committed, rejected) = run(&mut sim, 30);
    assert_eq!((committed, rejected.len()), (1, 0), "giant: {rejected:?}");
    println!("giant collapse (this build profile): {:?}", t.elapsed());
    // The block (largest component) keeps the giant's entity; the plate and
    // stub detach as a small child. Either way exactly one body is added and
    // the block's 2,097,152 cells are intact and awake.
    assert_eq!(sim.world().body_count(), bodies_before + 3);
    let block = sim
        .world()
        .body(EntityId::new(vfix::G4_ENTITY_FIRST).unwrap())
        .unwrap();
    assert!(!block.dormant);
    assert!(spall_sim::world::solid_cells(&block.volume) >= 2_097_152);
}

/// The fixture agitator keeps designated debris in the solver.
#[test]
fn the_agitator_keeps_active_bodies_awake_and_near_home() {
    let (mut sim, bodies) = scene();
    let probe = bodies.active[0];
    let mut awake_samples = 0;
    for tick in 0..900u64 {
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, tick);
        sim.tick().unwrap();
        if tick >= 300 && !sim.world().body(probe.entity).unwrap().sleeping {
            awake_samples += 1;
        }
    }
    assert!(awake_samples > 500, "awake in {awake_samples}/600 samples");
    let t = sim.world().body(probe.entity).unwrap().pose.translation_m;
    let d = ((t[0] - probe.home[0]).powi(2) + (t[2] - probe.home[2]).powi(2)).sqrt();
    assert!(d < 8.0, "body drifted {d:.1} m from home");
}

/// CPU cost of the networked workload's edit mix, one minute at 10 ordinary
/// edits/s plus a blast every 10 s and the giant collapse at second 1:
///
/// ```sh
/// cargo test -p spall_sim --release --test g4_integrated -- --ignored --nocapture measure_workload
/// ```
#[test]
#[ignore = "measurement: per-tick CPU cost of the integrated workload"]
fn measure_workload_cost() {
    let t0 = std::time::Instant::now();
    let (mut sim, bodies) = scene();
    println!("scene build + populate: {:?}", t0.elapsed());
    println!(
        "giant {} cells; comb {} cells; tower {} cells; active {} bodies ({} cells); sleeping {} ({} cells)",
        bodies.giant_cells,
        bodies.comb_cells,
        bodies.tower_cells,
        bodies.active.len(),
        bodies.active_cells,
        bodies.sleeping_total,
        bodies.sleeping_cells
    );
    let mut req = 1u64;
    let mut ordinary = 0u64;
    let mut blast = 0u64;
    let mut ticks = Vec::new();
    let mut commit_ticks = Vec::new();
    for t in 0..3600u64 {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            let e = vfix::g4_blast(blast).unwrap();
            blast += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let s = std::time::Instant::now();
        let r = sim.tick().unwrap();
        let d = s.elapsed();
        ticks.push(d);
        if !r.committed.is_empty() {
            commit_ticks.push(d);
        }
    }
    let pct = |v: &[std::time::Duration], q: usize| {
        let mut v = v.to_vec();
        v.sort();
        v[(v.len() * q / 100).min(v.len() - 1)]
    };
    println!(
        "60 s: {} ordinary + {} blasts + giant; ticks p50 {:?} p95 {:?} p99 {:?} max {:?}; commit ticks n={} p50 {:?} p95 {:?}; bodies {} (solver-active {})",
        ordinary,
        blast,
        pct(&ticks, 50),
        pct(&ticks, 95),
        pct(&ticks, 99),
        ticks.iter().max().unwrap(),
        commit_ticks.len(),
        pct(&commit_ticks, 50),
        pct(&commit_ticks, 95),
        sim.world().body_count(),
        sim.world().physics().active_body_count(),
    );
}

/// What a *terrain* commit costs at yard scale (`384 x 224 x 96` cells, `3.2 M`
/// solid): the reason the networked workload's edits target bodies.
///
/// ```sh
/// cargo test -p spall_sim --release --test g4_integrated -- --ignored --nocapture full_terrain_edit
/// ```
#[test]
#[ignore = "measurement: terrain-commit cost at yard scale"]
fn full_terrain_edit_cost_scales_with_terrain() {
    let mut sim =
        Simulation::new(SimulationConfig::new(fixtures::g4_full_yard_terrain_setup())).unwrap();
    println!("terrain solid cells: {}", sim.world().total_solid_cells());
    let mut times = Vec::new();
    for i in 0..10u64 {
        let x = 4 + 4 * (i as i64 * 7 % 50);
        sim.submit(EditIntent::cut(
            RequestId(i + 1),
            actor(),
            EditTarget::Terrain,
            brush_cell([x, 57, 4], 2),
        ))
        .unwrap();
        let s = std::time::Instant::now();
        let mut committed = 0;
        while committed == 0 {
            committed += sim.tick().unwrap().committed.len();
        }
        times.push(s.elapsed());
    }
    times.sort();
    println!(
        "terrain commit tick: p50 {:?} max {:?}",
        times[times.len() / 2],
        times.last().unwrap()
    );
}

/// The agitator list reconstructed from the fixed spawn order (as used on a
/// restored world) is exactly the list the spawner produced.
#[test]
fn reconstructed_active_list_matches_the_spawner() {
    let (_, bodies) = scene();
    let rebuilt = fixtures::g4_integrated_active_bodies();
    assert_eq!(rebuilt.len(), bodies.active.len());
    for (a, b) in rebuilt.iter().zip(&bodies.active) {
        assert_eq!(a.entity, b.entity);
        assert_eq!(a.home, b.home);
    }
}

/// Why does rubble never fall asleep? Runs 90 s of the edit stream and reports
/// what the awake, non-agitated bodies are doing.
///
/// ```sh
/// cargo test -p spall_sim --release --test g4_integrated -- --ignored --nocapture rubble_census
/// ```
#[test]
#[ignore = "diagnostic: what are the awake rubble bodies doing"]
fn rubble_census() {
    let (mut sim, bodies) = scene();
    let agit: std::collections::HashSet<_> = bodies.active.iter().map(|b| b.entity).collect();
    let mut req = 1u64;
    let mut ordinary = 0u64;
    for t in 0..(90 * 60u64) {
        if t % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        sim.tick().unwrap();
        if t % 1800 == 1799 {
            let (mut awake, mut asleep, mut dormant) = (0, 0, 0);
            for b in sim.world().bodies() {
                if agit.contains(&b.entity.unwrap()) {
                    continue;
                }
                if b.dormant {
                    dormant += 1;
                } else if b.sleeping {
                    asleep += 1;
                } else {
                    awake += 1;
                }
            }
            println!("t={t}: non-agitated awake={awake} asleep={asleep} dormant={dormant}");
        }
    }
    // Speed / height distribution of the awake rubble.
    let mut rows: Vec<(f64, f64, f64, u64)> = Vec::new();
    for b in sim.world().bodies() {
        let Some(e) = b.entity else { continue };
        if agit.contains(&e) || b.dormant || b.sleeping {
            continue;
        }
        let v = b.linvel_m_s;
        let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        let w = b.angvel_rad_s;
        let spin = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
        rows.push((speed, spin, b.pose.translation_m[1], e.get()));
    }
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let n = rows.len();
    println!("awake rubble: {n}");
    for q in [0, n / 4, n / 2, 3 * n / 4, n - 1] {
        let r = rows[q.min(n - 1)];
        println!(
            "  q{q}: speed {:.3} m/s spin {:.3} rad/s y {:.2} m (entity {})",
            r.0, r.1, r.2, r.3
        );
    }
    let low_y = rows.iter().filter(|r| r.2 < -1.0).count();
    let first_debris =
        vfix::G4_ENTITY_FIRST + 1 + vfix::G4_COMB_COUNT as u64 + vfix::G4_TOWER_COUNT as u64;
    let (mut comb, mut tower, mut tip) = (0, 0, 0);
    for r in rows.iter().filter(|r| r.2 < -1.0) {
        if r.3 < vfix::g4_tower_entity(0) {
            comb += 1
        } else if r.3 < first_debris {
            tower += 1
        } else {
            tip += 1
        }
    }
    println!("  fallen: comb parents {comb}, tower parents {tower}, detached rubble {tip}");
    println!("  below the ground (y < -1 m): {low_y}");
}

/// Does a body dropped from height H stay on the 1 m ground slab?
#[test]
#[ignore = "diagnostic: tunnelling versus drop height"]
fn tunnelling_vs_drop_height() {
    for (label, comb) in [("comb plate", true), ("4-cell cube", false)] {
        for h in [3.0f64, 6.0, 10.0, 15.0, 20.0, 30.0] {
            let mut sim =
                Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
            let pose = spall_sim::BodyPose::new(glam::DQuat::IDENTITY, [60.0, 1.0 + h, 40.0]);
            let e = if comb {
                sim.world_mut()
                    .spawn_body(vfix::g4_comb_body, pose, [0.0; 3], [0.0; 3], 2600.0, 0)
            } else {
                sim.world_mut().spawn_body(
                    fixtures::solid_block(4),
                    pose,
                    [0.0; 3],
                    [0.0; 3],
                    2600.0,
                    0,
                )
            }
            .unwrap();
            for _ in 0..600 {
                sim.tick().unwrap();
            }
            let b = sim.world().body(e).unwrap();
            println!(
                "{label} from {h:>4} m: y = {:.2} m, asleep {}",
                b.pose.translation_m[1], b.sleeping
            );
        }
    }
}

/// When and where does a body first pass below the ground?
#[test]
#[ignore = "diagnostic: first fall-through events"]
fn first_fall_through_events() {
    let (mut sim, bodies) = scene();
    let mut seen = std::collections::HashSet::new();
    let (mut req, mut ordinary) = (1u64, 0u64);
    let mut printed = 0;
    for t in 0..(60 * 60u64) {
        if t % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        sim.tick().unwrap();
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            if b.dormant || b.pose.translation_m[1] > -0.3 || !seen.insert(e.get()) {
                continue;
            }
            if printed < 12 {
                printed += 1;
                let p = b.pose.translation_m;
                let v = b.linvel_m_s;
                println!(
                    "t={t}: entity {} first below ground at ({:.2},{:.2},{:.2}) v=({:.1},{:.1},{:.1}) cells={}",
                    e.get(),
                    p[0],
                    p[1],
                    p[2],
                    v[0],
                    v[1],
                    v[2],
                    spall_sim::world::solid_cells(&b.volume)
                );
            }
        }
    }
    println!("total fallen: {}", seen.len());
}

/// With the dormancy policy on, what first wakes the sleeping block?
#[test]
#[ignore = "diagnostic: first sleeper reactivation"]
fn first_sleeper_wake() {
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let first_sleeper = vfix::G4_ENTITY_FIRST
        + 1
        + vfix::G4_COMB_COUNT as u64
        + vfix::G4_TOWER_COUNT as u64
        + G4_ACTIVE_BODY_COUNT as u64;
    let (mut req, mut ordinary, mut blast) = (1u64, 0u64, 0u64);
    for t in 0..(200 * 60u64) {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        let plan = sim.apply_dormancy(&mut policy, &report);
        let woke: Vec<_> = plan
            .reactivate
            .iter()
            .filter(|e| (first_sleeper..first_sleeper + 4096).contains(&e.get()))
            .collect();
        if !woke.is_empty() {
            println!(
                "t={t}: {} sleepers reactivated (of {} reactivations)",
                woke.len(),
                plan.reactivate.len()
            );
            let w = sim.world().body(*woke[0]).unwrap();
            println!("  first at {:?}", w.pose.translation_m);
            // nearest awake moving body
            let mut best = (f64::MAX, 0u64, [0.0; 3], 0.0);
            for b in sim.world().bodies() {
                if b.dormant {
                    continue;
                }
                let sp = b.linvel_m_s.iter().map(|v| v * v).sum::<f64>().sqrt();
                if sp <= 0.05 {
                    continue;
                }
                let d: f64 = (0..3)
                    .map(|i| (b.pose.translation_m[i] - w.pose.translation_m[i]).powi(2))
                    .sum::<f64>()
                    .sqrt();
                if d < best.0 {
                    best = (d, b.entity.map_or(0, |e| e.get()), b.pose.translation_m, sp);
                }
            }
            println!(
                "  nearest moving body: entity {} at {:?} dist {:.2} speed {:.2}",
                best.1, best.2, best.0, best.3
            );
            break;
        }
    }
}

/// Is the solver-awake set bounded by the *active* comb, not the accumulated
/// rubble? Runs five minutes of the stream with the dormancy policy.
#[test]
#[ignore = "diagnostic: awake / dormant counts over five minutes"]
fn awake_set_stays_bounded() {
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let (mut req, mut ordinary, mut blast) = (1u64, 0u64, 0u64);
    for t in 0..(300 * 60u64) {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        if t % 3600 == 3599 {
            let (mut awake, mut asleep, mut dormant) = (0, 0, 0);
            for b in sim.world().bodies() {
                if b.dormant {
                    dormant += 1;
                } else if b.sleeping {
                    asleep += 1;
                } else {
                    awake += 1;
                }
            }
            println!(
                "{:>3} s: bodies {} awake {awake} asleep {asleep} dormant {dormant}; solver-active {}",
                (t + 1) / 60,
                sim.world().body_count(),
                sim.world().physics().active_body_count()
            );
        }
    }
}

/// Which operation collapses the asleep set? Counts rapier-asleep, non-dormant
/// bodies after each phase of every tick and reports any phase that drops the
/// count sharply.
#[test]
#[ignore = "diagnostic: which phase mass-wakes asleep rubble"]
fn mass_wake_phase_attribution() {
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let asleep = |sim: &Simulation| -> i64 {
        sim.world()
            .bodies()
            .filter(|b| !b.dormant && b.sleeping)
            .count() as i64
    };
    let (mut req, mut ordinary, mut blast) = (1u64, 0u64, 0u64);
    let mut prev = 0i64;
    let mut reported = 0;
    for t in 0..(420 * 60u64) {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        let mut submitted = String::new();
        if t >= 120 && (t - 120) % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            submitted = format!("comb-cut#{ordinary}");
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            submitted = format!("blast#{blast}");
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        let a0 = asleep(&sim);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let a1 = asleep(&sim);
        let report = sim.tick().unwrap();
        let a2 = asleep(&sim);
        let plan = sim.apply_dormancy(&mut policy, &report);
        let a3 = asleep(&sim);
        let worst = [
            (a1 - a0, "agitate"),
            (a2 - a1, "tick"),
            (a3 - a2, "dormancy"),
        ]
        .into_iter()
        .min_by_key(|x| x.0)
        .unwrap();
        if worst.0 <= -100 && reported < 8 {
            reported += 1;
            println!(
                "t={t} ({}s): {} dropped asleep by {} ({a0}->{a3}); committed={} submitted='{submitted}' deact={} react={}",
                t / 60,
                worst.1,
                -worst.0,
                report.committed.len(),
                plan.deactivate.len(),
                plan.reactivate.len()
            );
        }
        prev = a3;
    }
    println!("final asleep {prev}");
}

/// Wake-reason attribution over the same run: which operation wakes asleep bodies?
#[test]
#[ignore = "diagnostic: wake reasons over five minutes"]
fn wake_reasons_over_the_workload() {
    let (mut sim, bodies) = scene();
    sim.world_mut().enable_wake_audit();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let (mut req, mut ordinary, mut blast) = (1u64, 0u64, 0u64);
    for t in 0..(240 * 60u64) {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            let e = vfix::g4_ordinary_edit(ordinary).unwrap();
            ordinary += 1;
            sim.submit(body_cut(req, e)).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        let probe = sim.world().wake_probe();
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        sim.world_mut().wake_probe_end("fixture.agitator", probe);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
    }
    let audit = sim.world().wake_audit().unwrap();
    for (reason, s) in &audit.reasons {
        println!(
            "{reason:<58} ops {:>6}, waking ops {:>5}, bodies woken {:>7}, max by one {:>4}",
            s.operations, s.waking_operations, s.bodies_woken, s.max_woken_by_one
        );
    }
}

/// Trace every body that ends up below the ground slab: who it is, when and by which
/// commit it was created, where it crossed, and whether it crossed over the footprint
/// of a terrain dig (an actual opening), outside the slab (escape), or elsewhere on
/// intact slab (tunnelling / missing collision geometry).
#[test]
#[ignore = "diagnostic: trace bodies that end far below the slab"]
fn escaped_body_trace() {
    use std::collections::HashMap;
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9500);
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let (mut req, mut ordinary, mut blast, mut digs, mut comb_edits) =
        (1u64, 0u64, 0u64, 0u64, 0u64);
    let mut dig_cells: Vec<[i64; 3]> = Vec::new();
    let mut born: HashMap<u64, (u64, Vec<u64>)> = HashMap::new();
    type Sample = (u64, [f64; 3], [f64; 3]);
    let mut hist: HashMap<u64, Vec<Sample>> = HashMap::new();
    let mut crossed: HashMap<u64, u64> = HashMap::new();
    let mut report_lines = Vec::new();
    for t in 0..ticks {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            if ordinary % 10 == 9 {
                let (cell, radius) = vfix::g4_terrain_dig(digs).unwrap();
                digs += 1;
                dig_cells.push(cell);
                sim.submit(EditIntent::cut(
                    RequestId(req),
                    actor(),
                    spall_sim::EditTarget::Terrain,
                    brush_cell(cell, radius),
                ))
                .unwrap();
            } else {
                let e = vfix::g4_ordinary_edit(comb_edits).unwrap();
                comb_edits += 1;
                sim.submit(body_cut(req, e)).unwrap();
            }
            ordinary += 1;
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        let commits: Vec<u64> = report.committed.iter().map(|c| c.0.0).collect();
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            let id = e.get();
            born.entry(id).or_insert_with(|| (t, commits.clone()));
            let h = hist.entry(id).or_default();
            h.push((t, b.pose.translation_m, b.linvel_m_s));
            if h.len() > 60 {
                h.remove(0);
            }
            if b.pose.translation_m[1] < -0.3 && !crossed.contains_key(&id) {
                crossed.insert(id, t);
                let p = b.pose.translation_m;
                let inside = p[0] > 1.0 && p[0] < 95.0 && p[2] > 1.0 && p[2] < 55.0;
                let near_dig = dig_cells
                    .iter()
                    .map(|c| {
                        let (cx, cz) = (c[0] as f64 * 0.25 + 0.125, c[2] as f64 * 0.25 + 0.125);
                        ((p[0] - cx).powi(2) + (p[2] - cz).powi(2)).sqrt()
                    })
                    .fold(f64::MAX, f64::min);
                let path: Vec<String> = h
                    .iter()
                    .step_by(6)
                    .map(|(tt, pp, vv)| {
                        format!(
                            "t{tt}:({:.2},{:.2},{:.2})v({:.1},{:.1},{:.1})",
                            pp[0], pp[1], pp[2], vv[0], vv[1], vv[2]
                        )
                    })
                    .collect();
                let (bt, bc) = &born[&id];
                report_lines.push(format!(
                    "entity {id}: created t={bt} in commit request(s) {bc:?}; crossed y<-0.3 at t={t} at ({:.2},{:.2},{:.2}); inside slab footprint {inside}; nearest dig centre {near_dig:.2} m (digs so far {}); dormant {} sleeping {} coarsen_k {} collider_rev {} cells {}\n    path {}",
                    p[0], p[1], p[2], dig_cells.len(), b.dormant, b.sleeping, b.coarsen_k, b.collider_revision,
                    spall_sim::world::solid_cells(&b.volume), path.join(" ")
                ));
            }
        }
    }
    let mut min_y = (f64::MAX, 0u64);
    for b in sim.world().bodies() {
        if let Some(e) = b.entity
            && b.pose.translation_m[1] < min_y.0
        {
            min_y = (b.pose.translation_m[1], e.get());
        }
    }
    println!(
        "=== {} bodies crossed below the slab; final min y {:.1} (entity {}), bodies {}, digs {}",
        crossed.len(),
        min_y.0,
        min_y.1,
        sim.world().body_count(),
        dig_cells.len()
    );
    println!("--- final state of the bodies that crossed below the slab");
    for id in crossed
        .keys()
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .take(20)
    {
        let b = sim.world().body(EntityId::new(id).unwrap()).unwrap();
        let (lo, hi) = b.collider_region;
        println!(
            "entity {id}: final pose ({:.2},{:.2},{:.2}) v({:.2},{:.2},{:.2}) sleeping {} dormant {} collider_region cells [{},{},{}]..[{},{},{}] kind {:?}",
            b.pose.translation_m[0],
            b.pose.translation_m[1],
            b.pose.translation_m[2],
            b.linvel_m_s[0],
            b.linvel_m_s[1],
            b.linvel_m_s[2],
            b.sleeping,
            b.dormant,
            lo.x,
            lo.y,
            lo.z,
            hi.x,
            hi.y,
            hi.z,
            b.kind
        );
    }
    let deep: std::collections::HashSet<u64> = sim
        .world()
        .bodies()
        .filter(|b| b.pose.translation_m[1] < -20.0)
        .filter_map(|b| b.entity.map(|e| e.get()))
        .collect();
    println!("--- bodies still below y = -20 at the end: {}", deep.len());
    for l in report_lines
        .iter()
        .filter(|l| {
            l.strip_prefix("entity ")
                .and_then(|r| r.split(':').next())
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|i| deep.contains(&i))
        })
        .take(12)
    {
        println!("{l}");
    }
}

/// Full-workload wake audit (in-process, deterministic schedule mirroring the
/// networked lane's declared mix): wake counts by reason, awake spell lengths and
/// repeated wakes per cohort. `AUDIT=0` skips the reason probes and `SCAN=0` skips the
/// per-tick census so their overhead can be timed separately (`ns per tick` printed).
#[test]
#[ignore = "diagnostic: full-workload wake audit"]
fn full_workload_wake_audit() {
    use std::collections::HashMap;
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9500);
    let audit = std::env::var("AUDIT").map(|v| v != "0").unwrap_or(true);
    let scan = std::env::var("SCAN").map(|v| v != "0").unwrap_or(true);
    let (mut sim, bodies) = scene();
    if audit {
        sim.world_mut().enable_wake_audit();
    }
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let first_sleeper = vfix::G4_ENTITY_FIRST
        + 1
        + vfix::G4_COMB_COUNT as u64
        + vfix::G4_TOWER_COUNT as u64
        + G4_ACTIVE_BODY_COUNT as u64;
    let combs =
        (vfix::G4_ENTITY_FIRST + 1)..(vfix::G4_ENTITY_FIRST + 1 + vfix::G4_COMB_COUNT as u64);
    let towers_end = combs.end + vfix::G4_TOWER_COUNT as u64;
    let cohort = |id: u64| -> &'static str {
        if id == vfix::G4_ENTITY_FIRST {
            "giant"
        } else if combs.contains(&id) {
            "comb"
        } else if id < towers_end {
            "tower"
        } else if id < first_sleeper {
            "active-debris"
        } else if id < first_sleeper + 4096 {
            "sleepers"
        } else {
            "rubble"
        }
    };
    #[derive(Default, Clone)]
    struct St {
        asleep: bool,
        seen: bool,
        since: u64,
        wakes: u32,
        awake_ticks: u64,
        spells: Vec<u64>,
        dormant_cycles: u32,
    }
    let mut st: HashMap<u64, St> = HashMap::new();
    let (mut req, mut ordinary, mut blast, mut digs, mut comb_edits) =
        (1u64, 0u64, 0u64, 0u64, 0u64);
    let wall = std::time::Instant::now();
    for t in 0..ticks {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120) % 6 == 0 {
            if ordinary % 10 == 9 {
                let (cell, radius) = vfix::g4_terrain_dig(digs).unwrap();
                digs += 1;
                sim.submit(EditIntent::cut(
                    RequestId(req),
                    actor(),
                    spall_sim::EditTarget::Terrain,
                    brush_cell(cell, radius),
                ))
                .unwrap();
            } else {
                let e = vfix::g4_ordinary_edit(comb_edits).unwrap();
                comb_edits += 1;
                sim.submit(body_cut(req, e)).unwrap();
            }
            ordinary += 1;
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            sim.submit(body_cut(req, vfix::g4_blast(blast).unwrap()))
                .unwrap();
            blast += 1;
            req += 1;
        }
        let probe = sim.world().wake_probe();
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        sim.world_mut()
            .wake_probe_end("fixture.agitator (impulses)", probe);
        let report = sim.tick().unwrap();
        let plan = sim.apply_dormancy(&mut policy, &report);
        if scan {
            for e in plan.deactivate.iter().chain(plan.reactivate.iter()) {
                st.entry(e.get()).or_default().dormant_cycles += 1;
            }
            for b in sim.world().bodies() {
                let Some(e) = b.entity else { continue };
                let s = st.entry(e.get()).or_default();
                let asleep = b.sleeping || b.dormant;
                if !s.seen {
                    s.seen = true;
                    s.asleep = asleep;
                    s.since = t;
                    continue;
                }
                if !asleep {
                    s.awake_ticks += 1;
                }
                if s.asleep && !asleep {
                    s.wakes += 1;
                    s.since = t;
                } else if !s.asleep && asleep {
                    s.spells.push(t - s.since);
                }
                s.asleep = asleep;
            }
        }
    }
    let ns_per_tick = wall.elapsed().as_nanos() as f64 / ticks as f64;
    println!(
        "=== audit={audit} scan={scan} ticks={ticks}: {:.3} ms/tick wall, digs {digs}, bodies {}",
        ns_per_tick / 1e6,
        sim.world().body_count()
    );
    if let Some(a) = sim.world().wake_audit() {
        for (reason, s) in &a.reasons {
            println!(
                "{reason:<52} ops {:>6}, waking ops {:>5}, sleeping bodies woken {:>7}, max by one {:>4}",
                s.operations, s.waking_operations, s.bodies_woken, s.max_woken_by_one
            );
        }
    }
    if scan {
        let mut by: HashMap<&str, Vec<&St>> = HashMap::new();
        for (id, s) in &st {
            by.entry(cohort(*id)).or_default().push(s);
        }
        let pct = |v: &mut Vec<u64>, p: f64| -> u64 {
            if v.is_empty() {
                return 0;
            }
            v.sort_unstable();
            v[((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1]
        };
        for name in [
            "giant",
            "comb",
            "tower",
            "active-debris",
            "sleepers",
            "rubble",
        ] {
            let Some(v) = by.get(name) else { continue };
            let n = v.len();
            let wakes: u64 = v.iter().map(|s| s.wakes as u64).sum();
            let rewoken = v.iter().filter(|s| s.wakes >= 2).count();
            let many = v.iter().filter(|s| s.wakes >= 10).count();
            let never_slept = v.iter().filter(|s| !s.asleep).count();
            let awake_ticks: u64 = v.iter().map(|s| s.awake_ticks).sum();
            let cycles: u64 = v.iter().map(|s| s.dormant_cycles as u64).sum();
            let mut spells: Vec<u64> = v.iter().flat_map(|s| s.spells.iter().copied()).collect();
            let (p50, p95, mx) = (
                pct(&mut spells, 0.5),
                pct(&mut spells, 0.95),
                spells.last().copied().unwrap_or(0),
            );
            println!(
                "{name:<14} bodies {n:>5} | wakes {wakes:>6} (>=2 wakes: {rewoken}, >=10: {many}) | awake body-ticks {awake_ticks:>9} ({:.1}% of body-ticks) | ended awake {never_slept} | completed spells p50 {p50} p95 {p95} max {mx} ticks | dormancy transitions {cycles}",
                100.0 * awake_ticks as f64 / (n as f64 * ticks as f64).max(1.0)
            );
        }
    }
}

/// Schedule shared by the long in-process diagnostics: giant at 1 s, an ordinary edit
/// every 6 ticks from tick 120 (every 10th a terrain dig unless `NO_DIGS` is set, which
/// turns those into comb cuts — a control for attributing terrain-dig wakes), a blast every
/// 600 ticks.
fn workload_step(sim: &mut Simulation, t: u64, counters: &mut (u64, u64, u64, u64, u64)) {
    let (req, ordinary, blast, digs, comb_edits) = counters;
    if t == 60 {
        sim.submit(body_cut(*req, vfix::g4_giant_cut())).unwrap();
        *req += 1;
    }
    if t >= 120 && (t - 120).is_multiple_of(6) {
        if *ordinary % 10 == 9 && std::env::var("NO_DIGS").is_err() {
            let (cell, radius) = vfix::g4_terrain_dig(*digs).unwrap();
            *digs += 1;
            sim.submit(EditIntent::cut(
                RequestId(*req),
                actor(),
                spall_sim::EditTarget::Terrain,
                brush_cell(cell, radius),
            ))
            .unwrap();
        } else {
            let e = vfix::g4_ordinary_edit(*comb_edits).unwrap();
            *comb_edits += 1;
            sim.submit(body_cut(*req, e)).unwrap();
        }
        *ordinary += 1;
        *req += 1;
    }
    if t >= 120 && (t - 120) % 600 == 300 {
        sim.submit(body_cut(*req, vfix::g4_blast(*blast).unwrap()))
            .unwrap();
        *blast += 1;
        *req += 1;
    }
}

/// Every 60 ticks, list bodies newly flagged by the containment census (deep
/// penetration in terrain / wholly outside the world) — first sightings only.
#[test]
#[ignore = "diagnostic: containment census over the long workload"]
fn containment_watch_over_the_workload() {
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9500);
    let stride: u64 = std::env::var("WATCH_STRIDE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    let mut seen = std::collections::HashSet::new();
    let (mut deep_total, mut ext_total) = (0, 0);
    for t in 0..ticks {
        workload_step(&mut sim, t, &mut counters);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        if t % stride != stride - 1 {
            continue;
        }
        let c = spall_sim::containment::containment_census(sim.world(), 4096, 0.25);
        for (kind, rows) in [
            ("deep_penetration", &c.deep_penetrations),
            ("external", &c.external),
        ] {
            for r in rows {
                if seen.insert((kind, r.entity)) {
                    if kind == "external" {
                        ext_total += 1
                    } else {
                        deep_total += 1
                    }
                    if deep_total + ext_total <= 30 {
                        println!(
                            "t={t} {kind}: entity {} depth {:.2} m cells {}/{} v({:.1},{:.1},{:.1}) aabb ({:.1},{:.1},{:.1})..({:.1},{:.1},{:.1}) sleeping {} dormant {}",
                            r.entity,
                            r.penetration_depth_m,
                            r.overlapped_cells,
                            r.solid_cells,
                            r.velocity_m_s[0],
                            r.velocity_m_s[1],
                            r.velocity_m_s[2],
                            r.aabb_min_m[0],
                            r.aabb_min_m[1],
                            r.aabb_min_m[2],
                            r.aabb_max_m[0],
                            r.aabb_max_m[1],
                            r.aabb_max_m[2],
                            r.sleeping,
                            r.dormant
                        );
                    }
                }
            }
        }
    }
    println!(
        "=== containment over {ticks} ticks: {deep_total} bodies ever deeply penetrating terrain, {ext_total} ever wholly outside the world, bodies {}",
        sim.world().body_count()
    );
}

/// Attribute the initiators of every physics-step wake burst (>= 20 sleeping bodies woken
/// in one step): who was awake and touching the woken set, how fast it was moving, how
/// long it had been awake, and how much of each burst propagated through the island
/// rather than touching an awake body directly. Audit runs are never timing runs.
#[test]
#[ignore = "diagnostic: initiators of physics-step wake bursts"]
fn wake_burst_initiators() {
    use std::collections::HashMap;
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(9500);
    let (mut sim, bodies) = scene();
    sim.world_mut().enable_wake_audit();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    let first_sleeper = vfix::G4_ENTITY_FIRST
        + 1
        + vfix::G4_COMB_COUNT as u64
        + vfix::G4_TOWER_COUNT as u64
        + G4_ACTIVE_BODY_COUNT as u64;
    let combs =
        (vfix::G4_ENTITY_FIRST + 1)..(vfix::G4_ENTITY_FIRST + 1 + vfix::G4_COMB_COUNT as u64);
    let towers_end = combs.end + vfix::G4_TOWER_COUNT as u64;
    let cohort = |id: u64| -> &'static str {
        if id == vfix::G4_ENTITY_FIRST {
            "giant"
        } else if combs.contains(&id) {
            "comb"
        } else if id < towers_end {
            "tower"
        } else if id < first_sleeper {
            "active-debris"
        } else if id < first_sleeper + 4096 {
            "sleepers"
        } else {
            "rubble"
        }
    };
    let mut awake_since: HashMap<u64, u64> = HashMap::new();
    let mut created: HashMap<u64, u64> = HashMap::new();
    let mut seen_bursts = 0usize;
    let mut dig_reqs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut by_commit: HashMap<&str, (u64, u64)> = HashMap::new();
    // (cohort, speed class, age class) -> (bursts, contacts)
    let mut agg: HashMap<(&str, &str, &str), (u64, u64)> = HashMap::new();
    let (mut total_bursts, mut total_woken, mut total_chained) = (0u64, 0u64, 0u64);
    let mut printed = 0;
    for t in 0..ticks {
        let digs_before = counters.3;
        workload_step(&mut sim, t, &mut counters);
        if counters.3 > digs_before {
            dig_reqs.insert(counters.0 - 1);
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        // Initiators are classified by the state at the *start* of the step, so read the
        // spell tables before updating them with this tick's post-step state.
        let bursts: Vec<_> = sim.world().wake_audit().unwrap().bursts[seen_bursts..].to_vec();
        seen_bursts += bursts.len();
        for b in &bursts {
            total_bursts += 1;
            total_woken += b.woken;
            total_chained += b.chained;
            let kind = if report
                .committed
                .iter()
                .any(|(r, _)| dig_reqs.contains(&r.0))
            {
                "same tick: terrain dig committed"
            } else if !report.committed.is_empty() {
                "same tick: body edit committed"
            } else {
                "no commit this tick"
            };
            let e = by_commit.entry(kind).or_default();
            e.0 += 1;
            e.1 += b.woken;
            for (rank, i) in b.initiators.iter().enumerate() {
                let speed = if i.speed_m_s > 1.0 {
                    "fast>1m/s"
                } else if i.speed_m_s > 0.1 {
                    "slow"
                } else {
                    "at-rest<0.1"
                };
                let since = awake_since.get(&i.entity).copied().unwrap_or(0);
                let born = created.get(&i.entity).copied().unwrap_or(0);
                let age = t.saturating_sub(since.max(born));
                let age_class = if born == t || born + 1 == t {
                    "new-this-tick"
                } else if age < 30 {
                    "awake<0.5s"
                } else if age < 300 {
                    "awake<5s"
                } else {
                    "awake>=5s"
                };
                if rank == 0 {
                    let e = agg.entry((cohort(i.entity), speed, age_class)).or_default();
                    e.0 += 1;
                    e.1 += i.touches_woken as u64;
                    if printed < 12 && b.woken >= 100 {
                        printed += 1;
                        println!(
                            "t={t}: burst woke {} (direct {}, chained {}); top initiator entity {} [{}] speed {:.2} m/s awake for {age} ticks touching {}",
                            b.woken,
                            b.directly_touched,
                            b.chained,
                            i.entity,
                            cohort(i.entity),
                            i.speed_m_s,
                            i.touches_woken
                        );
                    }
                }
            }
        }
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            created.entry(e.get()).or_insert(t);
            let asleep = b.sleeping || b.dormant;
            if asleep {
                awake_since.remove(&e.get());
            } else {
                awake_since.entry(e.get()).or_insert(t);
            }
        }
    }
    println!(
        "=== {ticks} ticks: {total_bursts} bursts (>= 20 woken), {total_woken} bodies woken, {total_chained} of them chained through the island ({:.1}%)",
        100.0 * total_chained as f64 / total_woken.max(1) as f64
    );
    let mut rows: Vec<_> = agg.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.0));
    for (k, (n, w)) in &by_commit {
        println!("  bursts {k}: {n} bursts, {w} bodies woken");
    }
    println!("top initiator per burst by (cohort, speed, awake age): bursts / contacts");
    for ((c, s, a), (n, k)) in rows.iter().take(20) {
        println!("  {c:<14} {s:<12} {a:<14} {n:>5} bursts, {k:>7} woken bodies touched");
    }
}

/// Full tick cost vs physics-stage cost of the unchanged short workload (declared edit mix
/// with terrain digs, dormancy on, no audit, no census), for the whole-world terrain collider
/// (`TERRAIN_BRICKS` unset) or per-brick colliders (`TERRAIN_BRICKS=1`). One mode per
/// invocation; alternate invocations to compare (`TRACE_TICKS`, default 3600).
#[test]
#[ignore = "measurement: full tick and physics cost, terrain collider mode"]
fn terrain_collider_cost() {
    use std::time::{Duration, Instant};
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    let mut total = Vec::new();
    let mut physics = Vec::new();
    let mut commit = Vec::new();
    let mut awake_sum = 0u64;
    let _ = spall_sim::prof::drain();
    for t in 0..ticks {
        workload_step(&mut sim, t, &mut counters);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let s = Instant::now();
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        let d = s.elapsed();
        let (mut ph, mut cm) = (Duration::ZERO, Duration::ZERO);
        for (name, dur) in spall_sim::prof::drain() {
            match name {
                "physics.rapier_step" | "physics.pose_extract" => ph += dur,
                "sim.commit" => cm += dur,
                _ => {}
            }
        }
        if t >= 600 {
            total.push(d);
            physics.push(ph);
            commit.push(cm);
            awake_sum += sim.world().physics().active_body_count() as u64;
        }
    }
    let stat = |v: &[Duration]| {
        let mut s = v.to_vec();
        s.sort();
        let q = |p: usize| s[(s.len() * p / 100).min(s.len() - 1)].as_secs_f64() * 1e3;
        let mean = v.iter().sum::<Duration>().as_secs_f64() * 1e3 / v.len() as f64;
        format!(
            "mean {mean:.2} p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms",
            q(50),
            q(95),
            q(99),
            q(100)
        )
    };
    println!(
        "MODE terrain_bricks={} ticks {} (after 600 warmup) digs {}: awake solver bodies avg {}",
        std::env::var("TERRAIN_BRICKS").is_ok(),
        total.len(),
        counters.3,
        awake_sum / total.len() as u64
    );
    println!("  full tick   : {}", stat(&total));
    println!("  physics     : {}", stat(&physics));
    println!("  sim.commit  : {}", stat(&commit));
}

/// Body-time and contact evidence for the awake-body population, per terrain collider mode
/// (`TERRAIN_BRICKS` unset / set). Counts, over the measured window and for every non-agitated
/// body, the awake body-ticks by what the body is touching, the awake *episodes* (wake to next
/// sleep) and how long they last, so the difference between modes can be attributed.
#[test]
#[ignore = "measurement: awake body-time and contact attribution"]
fn awake_body_time() {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600);
    let (mut sim, bodies) = scene();
    let agitated: BTreeSet<u64> = bodies.active.iter().map(|b| b.entity.get()).collect();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    // entity -> ticks awake in the current episode
    let mut episode: HashMap<u64, u64> = HashMap::new();
    let mut episodes: Vec<u64> = Vec::new();
    // Per episode: did the body sit within 0.6 m of a brick-seam line (x or z a multiple of 8 m)
    // at any point, and did it ever press on two terrain colliders at once?
    let mut near_seam_ep: HashMap<u64, (bool, bool)> = HashMap::new();
    let mut ep_groups: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    let mut rapier_active_sum = 0u64;
    // (fixed terrain bodies, dynamic awake, dynamic rapier-asleep, dormant) summed over the window
    let mut comp = (0u64, 0u64, 0u64, 0u64);
    let mut flag_awake_sum = 0u64;
    let (mut awake_ticks, mut by_contact) = (0u64, BTreeMap::<&str, u64>::new());
    let mut window = 0u64;
    let mut born_awake = 0u64;
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    for t in 0..ticks {
        workload_step(&mut sim, t, &mut counters);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        if t < 600 {
            continue;
        }
        window += 1;
        rapier_active_sum += sim.world().physics().active_body_count() as u64;
        // What `active_body_count` is made of: fixed terrain bodies + every non-dormant dynamic
        // body (including rapier-asleep ones).
        comp.0 += sim.world().terrain_physics_bodies().len() as u64;
        for b in sim.world().bodies() {
            if b.dormant {
                comp.3 += 1;
            } else if b.sleeping {
                comp.2 += 1;
            } else {
                comp.1 += 1;
            }
        }
        // Contacts of this tick: per body, how many distinct terrain bodies and dynamic bodies
        // it presses on.
        let mut terrain_of: HashMap<
            spall_physics::BodyId,
            std::collections::HashSet<spall_physics::BodyId>,
        > = HashMap::new();
        let mut dynamic_of: HashMap<spall_physics::BodyId, u32> = HashMap::new();
        for c in sim.world().physics().contact_impulses() {
            for side in 0..2 {
                let me = c.bodies[side];
                let other = c.bodies[1 - side];
                if !c.dynamic[side] {
                    continue;
                }
                if sim.world().is_terrain_physics_body(other) {
                    terrain_of.entry(me).or_default().insert(other);
                } else {
                    *dynamic_of.entry(me).or_default() += 1;
                }
            }
        }
        let mut live: BTreeSet<u64> = BTreeSet::new();
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            let id = e.get();
            if agitated.contains(&id) {
                continue;
            }
            let is_awake = !b.sleeping && !b.dormant;
            if !seen.contains(&id) {
                seen.insert(id);
                if is_awake {
                    born_awake += 1;
                }
            }
            if is_awake {
                live.insert(id);
                *episode.entry(id).or_default() += 1;
                flag_awake_sum += 1;
                let p = b.pose.translation_m;
                let near = |v: f64| {
                    let m = v.rem_euclid(8.0);
                    !(0.6..=7.4).contains(&m)
                };
                let e = near_seam_ep.entry(id).or_default();
                e.0 |= near(p[0]) || near(p[2]);
                e.1 |= terrain_of.get(&b.phys).is_some_and(|s| s.len() >= 2);
                awake_ticks += 1;
                let terr = terrain_of.get(&b.phys).map_or(0, |s| s.len());
                let dynn = dynamic_of.get(&b.phys).copied().unwrap_or(0);
                let key = match (terr, dynn) {
                    (0, 0) => "no contact (moving/airborne)",
                    (0, _) => "bodies only",
                    (1, 0) => "one terrain collider",
                    (1, _) => "one terrain collider + bodies",
                    (_, 0) => "two+ terrain colliders (seam)",
                    (_, _) => "two+ terrain colliders (seam) + bodies",
                };
                *by_contact.entry(key).or_default() += 1;
            }
        }
        let ended: Vec<u64> = episode
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in ended {
            let len = episode.remove(&id).unwrap();
            let (near, two) = near_seam_ep.remove(&id).unwrap_or_default();
            episodes.push(len);
            ep_groups
                .entry(if near {
                    "episodes near a seam line"
                } else {
                    "episodes away from seam lines"
                })
                .or_default()
                .push(len);
            if two {
                ep_groups
                    .entry("episodes that pressed two terrain colliders")
                    .or_default()
                    .push(len);
            }
        }
    }
    episodes.extend(episode.values().copied());
    episodes.sort_unstable();
    let q = |p: usize| {
        episodes
            .get((episodes.len() * p / 100).min(episodes.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    let mean = episodes.iter().sum::<u64>() as f64 / episodes.len().max(1) as f64;
    println!(
        "MODE terrain_bricks={} window {window} ticks, {} non-agitated bodies observed ({} first seen awake)",
        std::env::var("TERRAIN_BRICKS").is_ok(),
        seen.len(),
        born_awake
    );
    println!(
        "  awake body-ticks (non-agitated): {awake_ticks} = {:.1} per tick",
        awake_ticks as f64 / window as f64
    );
    for (k, v) in &by_contact {
        println!(
            "    {k}: {v} ({:.1}%)",
            100.0 * *v as f64 / awake_ticks.max(1) as f64
        );
    }
    println!(
        "  rapier active bodies avg {:.1}/tick (incl. agitated); flag-awake non-agitated {:.1}/tick",
        rapier_active_sum as f64 / window as f64,
        flag_awake_sum as f64 / window as f64
    );
    let w = window as f64;
    println!(
        "  active_body_count = fixed terrain {:.1} + dynamic awake {:.1} (incl. 256 agitated) + dynamic asleep-not-dormant {:.1}; dormant (excluded) {:.1}",
        comp.0 as f64 / w,
        comp.1 as f64 / w,
        comp.2 as f64 / w,
        comp.3 as f64 / w
    );
    for (k, v) in ep_groups.iter_mut() {
        v.sort_unstable();
        let m = v.iter().sum::<u64>() as f64 / v.len() as f64;
        println!(
            "  {k}: {} (mean {m:.0} p50 {} p95 {} max {})",
            v.len(),
            v[v.len() / 2],
            v[(v.len() * 95 / 100).min(v.len() - 1)],
            v[v.len() - 1]
        );
    }
    println!(
        "  awake episodes: {} (mean {mean:.0} ticks, p50 {} p95 {} max {})",
        episodes.len(),
        q(50),
        q(95),
        q(100)
    );
}

/// Edit-pipeline drain capacity under offered load (in-process, no network). Offers one ordinary
/// body edit every `OFFER_EVERY` ticks (default 1 = 60/s, the overload scenario) and a blast every
/// `BLAST_EVERY` ticks (default 120 = every 2 s) and reports, per tick, how the pipeline drained:
/// commits, pending depth, conflicts retried, staged results discarded as stale.
#[test]
#[ignore = "measurement: edit pipeline drain capacity"]
fn overload_drain() {
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800);
    let offer_every: u64 = std::env::var("OFFER_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let blast_every: u64 = std::env::var("BLAST_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let per_tick: u64 = std::env::var("OFFER_PER_TICK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let (mut sim, bodies) = scene();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let (mut req, mut comb, mut blast) = (1u64, 0u64, 0u64);
    let (mut offered, mut rejected_full, mut committed) = (0u64, 0u64, 0u64);
    let (mut retried, mut stale, mut serialized) = (0u64, 0u64, 0u64);
    let (mut pending_max, mut pending_sum, mut pending_ticks) = (0usize, 0usize, 0u64);
    let mut latency_ticks: Vec<u64> = Vec::new();
    let mut submitted_at: std::collections::HashMap<u64, u64> = Default::default();
    let mut rejected_other = 0u64;
    for t in 0..ticks {
        if t == 60 {
            sim.submit(body_cut(req, vfix::g4_giant_cut())).unwrap();
            submitted_at.insert(req, t);
            req += 1;
        }
        // OFFER_PER_TICK edits on every offering tick: a slow server (wall-clock clients) sees
        // several ticks' worth of edits at once.
        for _ in 0..per_tick {
            if t >= 120
                && (t - 120).is_multiple_of(offer_every)
                && let Some(e) = vfix::g4_ordinary_edit(comb)
            {
                comb += 1;
                offered += 1;
                match sim.submit(body_cut(req, e)) {
                    Ok(_) => {
                        submitted_at.insert(req, t);
                    }
                    Err(spall_sim::IntentError::QueueFull { .. }) => rejected_full += 1,
                    Err(_) => rejected_other += 1,
                }
                req += 1;
            }
        }
        if t >= 120
            && (t - 120) % blast_every == blast_every / 2
            && let Some(b) = vfix::g4_blast(blast)
        {
            blast += 1;
            offered += 1;
            match sim.submit(body_cut(req, b)) {
                Ok(_) => {
                    submitted_at.insert(req, t);
                }
                Err(spall_sim::IntentError::QueueFull { .. }) => rejected_full += 1,
                Err(_) => rejected_other += 1,
            }
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        committed += report.committed.len() as u64;
        retried += report.retried.len() as u64;
        stale += report.discarded_stale.len() as u64;
        serialized += report.serialized_regions.len() as u64;
        for (r, _) in &report.committed {
            if let Some(at) = submitted_at.remove(&r.0) {
                latency_ticks.push(t - at);
            }
        }
        pending_max = pending_max.max(report.pending_after);
        pending_sum += report.pending_after;
        if report.pending_after > 0 {
            pending_ticks += 1;
        }
    }
    latency_ticks.sort_unstable();
    let q = |p: usize| {
        latency_ticks
            .get((latency_ticks.len() * p / 100).min(latency_ticks.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    println!(
        "OFFER_PER_TICK={per_tick} OFFER_EVERY={offer_every} BLAST_EVERY={blast_every} ticks {ticks}: offered {offered}, queue-full {rejected_full}, other rejects {rejected_other}, committed {committed}, unresolved {}",
        submitted_at.len()
    );
    println!(
        "  retried(conflict) {retried}, stale-discards {stale}, serialized regions {serialized}; pending max {pending_max} mean {:.1}, ticks with pending {pending_ticks}",
        pending_sum as f64 / ticks as f64
    );
    println!(
        "  request->commit latency (ticks): p50 {} p95 {} p99 {} max {}",
        q(50),
        q(95),
        q(99),
        q(100)
    ); // Regression guard for the overload collapse (stale queued jobs starving fresh ones): with
    // more edits offered than the pipeline can commit it must still commit about one per tick
    // and never discard staged work as stale (`ASSERT_DRAIN=1`; before the fix, 3 edits/tick
    // committed 105 of 1200 ticks' worth with 15,782 stale discards).
    if std::env::var("ASSERT_DRAIN").is_ok() {
        assert_eq!(stale, 0, "queued jobs must not go stale");
        assert!(
            committed * 2 >= ticks,
            "throughput collapsed under overload: {committed} commits in {ticks} ticks"
        );
    }
}

/// Long-horizon growth of awake dynamic bodies and ordinary-tick cost on the nominal workload
/// (in-process, tick-paced: no wall-clock pile-up), one terrain-collider mode per invocation
/// (`TERRAIN_BRICKS` unset / set). Prints one row per `WINDOW` ticks so a rising ordinary-tick cost
/// can be told from occasional expensive digs, and whether new rubble ever goes to sleep.
#[test]
#[ignore = "measurement: long-horizon awake-body growth and ordinary tick cost"]
fn awake_growth() {
    use std::time::Instant;
    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(21_600);
    const WINDOW: u64 = 1_800;
    let (mut sim, bodies) = scene();
    if std::env::var("WAKE_AUDIT").is_ok() {
        sim.world_mut().enable_wake_audit();
    }
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    let _ = spall_sim::prof::drain();
    let (mut ord, mut dig, mut phys) = (Vec::new(), Vec::new(), 0.0f64);
    println!(
        "MODE terrain_bricks={} (tick-paced nominal workload)",
        std::env::var("TERRAIN_BRICKS").is_ok()
    );
    println!(
        "  tick  bodies  awake asleep dormant | ordinary p50/p95 ms | dig mean ms (n) | physics mean ms"
    );
    for t in 0..ticks {
        workload_step(&mut sim, t, &mut counters);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let s = Instant::now();
        let report = sim.tick().unwrap();
        sim.apply_dormancy(&mut policy, &report);
        let ms = s.elapsed().as_secs_f64() * 1e3;
        for (name, d) in spall_sim::prof::drain() {
            if name == "physics.rapier_step" || name == "physics.pose_extract" {
                phys += d.as_secs_f64() * 1e3;
            }
        }
        let terrain = sim.world().terrain_volume_id();
        let dug = report.committed.iter().any(|(_, c)| {
            c.topology.ops.iter().any(|op| {
                matches!(op, spall_protocol::TopologyOp::IntegerBrush { volume, .. } if *volume == terrain)
            })
        });
        if dug {
            dig.push(ms);
        } else if report.committed.is_empty() {
            ord.push(ms);
        }
        if (t + 1) % WINDOW == 0 {
            let (mut awake, mut asleep, mut dormant) = (0, 0, 0);
            for b in sim.world().bodies() {
                if b.dormant {
                    dormant += 1;
                } else if b.sleeping {
                    asleep += 1;
                } else {
                    awake += 1;
                }
            }
            ord.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: usize| {
                ord.get((ord.len() * p / 100).min(ord.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(0.0)
            };
            let dm = dig.iter().sum::<f64>() / dig.len().max(1) as f64;
            println!(
                "  {:5}  {:6} {:6} {:6} {:6} | {:6.1} / {:6.1} | {:6.1} ({:3}) | {:6.2}",
                t + 1,
                sim.world().body_count(),
                awake,
                asleep,
                dormant,
                q(50),
                q(95),
                dm,
                dig.len(),
                phys / WINDOW as f64
            );
            ord.clear();
            dig.clear();
            phys = 0.0;
        }
    }
    if let Some(audit) = sim.world().wake_audit() {
        println!("wake reasons over the run (asleep bodies woken):");
        for (reason, st) in &audit.reasons {
            println!(
                "  {reason:<52} ops {:>6}, waking ops {:>5}, bodies woken {:>8}, max by one {:>5}",
                st.operations, st.waking_operations, st.bodies_woken, st.max_woken_by_one
            );
        }
    }
}

/// Bounded per-body transition tracking for rubble: how long each body is awake, its longest
/// uninterrupted awake interval, how long it sleeps before it is woken again, how long it stays
/// dormant, and -- for representative cohorts followed from creation -- what actually woke it.
/// One terrain-collider mode per invocation (`TERRAIN_BRICKS` unset / set); instrumented, so it is
/// a diagnostic and never a timing measurement.
#[test]
#[ignore = "diagnostic: per-body rubble transitions and wake triggers"]
fn rubble_transitions() {
    use std::collections::{BTreeMap, HashMap};

    #[derive(Default, Clone)]
    struct Track {
        born: u64,
        cohort: Option<usize>,
        state: u8, // 0 awake, 1 asleep (solver), 2 dormant
        since: u64,
        awake: u64,
        asleep: u64,
        dormant: u64,
        longest_awake: u64,
        sleeps: u32,
        wakes: u32,
        dormant_entries: u32,
        reactivations: u32,
        first_sleep_after: Option<u64>,
        asleep_intervals: Vec<u64>,
        speed_ticks: u64,
        moving_ticks: u64,
    }
    fn close(t: &mut Track, now: u64) {
        let len = now - t.since;
        match t.state {
            0 => {
                t.awake += len;
                t.longest_awake = t.longest_awake.max(len);
            }
            1 => t.asleep += len,
            _ => t.dormant += len,
        }
    }

    let ticks: u64 = std::env::var("TRACE_TICKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(21_600);
    const COHORT_WINDOWS: [(u64, u64); 3] = [(2_000, 2_600), (9_000, 9_600), (16_000, 16_600)];
    const COHORT_SIZE: usize = 150;
    let (mut sim, bodies) = scene();
    sim.world_mut().enable_wake_audit();
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let mut counters = (1u64, 0u64, 0u64, 0u64, 0u64);
    let initial: std::collections::BTreeSet<u64> = sim
        .world()
        .bodies()
        .filter_map(|b| b.entity.map(|e| e.get()))
        .collect();
    let mut tracks: HashMap<u64, Track> = HashMap::new();
    let mut cohort_counts = [0usize; 3];
    let (mut react_hard, mut react_prox, mut deact) = (0u64, 0u64, 0u64);
    let mut awake_by_tick_bucket: Vec<(u64, u64, u64)> = Vec::new(); // (tick, awake rubble, total rubble)
    // What each tick committed: 0 nothing, 1 a terrain dig, 2 a body edit (cut / blast), 3 both.
    let mut tick_class: Vec<u8> = Vec::with_capacity(ticks as usize);
    // asleep -> awake transitions of all rubble, by what that tick committed.
    let mut wakes_by_class = [0u64; 4];

    for t in 0..ticks {
        workload_step(&mut sim, t, &mut counters);
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        sim.world_mut().wake_audit_set_clock(t);
        let report = sim.tick().unwrap();
        let plan = sim.apply_dormancy(&mut policy, &report);
        let terrain_id = sim.world().terrain_volume_id();
        let (mut dug, mut cut) = (false, false);
        for (_, c) in &report.committed {
            let on_terrain = c.topology.ops.iter().any(|op| {
                matches!(op, spall_protocol::TopologyOp::IntegerBrush { volume, .. } if *volume == terrain_id)
            });
            if on_terrain {
                dug = true;
            } else {
                cut = true;
            }
        }
        let class = u8::from(dug) | (u8::from(cut) << 1);
        tick_class.push(class);
        react_hard += plan.reactivate_hard.len() as u64;
        react_prox += (plan.reactivate.len() - plan.reactivate_hard.len()) as u64;
        deact += plan.deactivate.len() as u64;
        let now = t + 1;
        let mut awake_rubble = 0u64;
        let mut total_rubble = 0u64;
        let mut new_ids = Vec::new();
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            let id = e.get();
            if initial.contains(&id) {
                continue;
            }
            total_rubble += 1;
            let state = if b.dormant {
                2
            } else if b.sleeping {
                1
            } else {
                0
            };
            if state == 0 {
                awake_rubble += 1;
            }
            let speed =
                (b.linvel_m_s[0].powi(2) + b.linvel_m_s[1].powi(2) + b.linvel_m_s[2].powi(2))
                    .sqrt();
            let tr = tracks.entry(id).or_insert_with(|| {
                new_ids.push(id);
                Track {
                    born: now,
                    state,
                    since: now,
                    ..Track::default()
                }
            });
            if tr.state != state {
                let old = tr.state;
                let len = now - tr.since;
                close(tr, now);
                if old == 1 && state == 0 {
                    wakes_by_class[class as usize] += 1;
                    tr.wakes += 1;
                    tr.asleep_intervals.push(len);
                }
                if old == 2 && state == 0 {
                    tr.reactivations += 1;
                }
                if state == 1 {
                    tr.sleeps += 1;
                    if tr.first_sleep_after.is_none() {
                        tr.first_sleep_after = Some(now - tr.born);
                    }
                }
                if state == 2 {
                    tr.dormant_entries += 1;
                    if tr.first_sleep_after.is_none() {
                        tr.first_sleep_after = Some(now - tr.born);
                    }
                }
                tr.state = state;
                tr.since = now;
            }
            if state == 0 {
                tr.speed_ticks += 1;
                if speed > 0.05 {
                    tr.moving_ticks += 1;
                }
            }
        }
        for id in new_ids {
            for (c, (lo, hi)) in COHORT_WINDOWS.iter().enumerate() {
                if now >= *lo && now < *hi && cohort_counts[c] < COHORT_SIZE {
                    cohort_counts[c] += 1;
                    tracks.get_mut(&id).unwrap().cohort = Some(c);
                    sim.world_mut()
                        .wake_audit_track(spall_core::EntityId::new(id).unwrap());
                    break;
                }
            }
        }
        if now % 1_800 == 0 {
            awake_by_tick_bucket.push((now, awake_rubble, total_rubble));
        }
    }
    let end = ticks;
    for tr in tracks.values_mut() {
        close(tr, end);
        tr.since = end;
    }

    println!(
        "MODE terrain_bricks={} ticks {ticks}: rubble bodies {} (initial population excluded)",
        std::env::var("TERRAIN_BRICKS").is_ok(),
        tracks.len()
    );
    println!(
        "dormancy over the run: deactivations {deact}, reactivations by terrain edit {react_hard}, by proximity {react_prox}"
    );
    println!("awake rubble / rubble bodies every 1,800 ticks: {awake_by_tick_bucket:?}");
    let n_class = |k: u8| tick_class.iter().filter(|c| **c == k).count() as u64;
    println!(
        "asleep->awake transitions of all rubble by what the tick committed: nothing {} over {} ticks, terrain dig {} over {} ticks ({:.0} per dig tick), body edit {} over {} ticks, both {}",
        wakes_by_class[0],
        n_class(0),
        wakes_by_class[1],
        n_class(1),
        wakes_by_class[1] as f64 / n_class(1).max(1) as f64,
        wakes_by_class[2],
        n_class(2),
        wakes_by_class[3]
    );

    // Population classes for rubble alive at least 1,800 ticks.
    let mature: Vec<&Track> = tracks.values().filter(|t| end - t.born >= 1_800).collect();
    let never: Vec<&&Track> = mature
        .iter()
        .filter(|t| t.first_sleep_after.is_none())
        .collect();
    let once: Vec<&&Track> = mature
        .iter()
        .filter(|t| t.first_sleep_after.is_some() && t.wakes + t.reactivations == 0)
        .collect();
    let repeat: Vec<&&Track> = mature
        .iter()
        .filter(|t| t.wakes + t.reactivations >= 1)
        .collect();
    let tot_awake: u64 = mature.iter().map(|t| t.awake).sum();
    let share = |v: &[&&Track]| {
        100.0 * v.iter().map(|t| t.awake).sum::<u64>() as f64 / tot_awake.max(1) as f64
    };
    let med = |mut v: Vec<u64>| {
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    };
    println!("rubble alive >= 1,800 ticks: {}", mature.len());
    for (name, v) in [
        ("never settled (never asleep, never dormant)", &never),
        ("settled and stayed (no later wake)", &once),
        (
            "settled then woke again (>= 1 wake or reactivation)",
            &repeat,
        ),
    ] {
        let mv: f64 = v
            .iter()
            .map(|t| t.moving_ticks as f64 / t.speed_ticks.max(1) as f64)
            .sum::<f64>()
            / v.len().max(1) as f64;
        println!(
            "  {name}: {} bodies, {:.1}% of awake body-ticks, median awake {} ticks, median longest awake interval {} ticks, mean moving fraction while awake {:.2}",
            v.len(),
            share(v),
            med(v.iter().map(|t| t.awake).collect()),
            med(v.iter().map(|t| t.longest_awake).collect()),
            mv
        );
    }
    let cycles = |t: &Track| t.wakes + t.reactivations;
    println!(
        "  settle-and-wake cycles per body (mature): 0: {}, 1: {}, 2-3: {}, 4+: {}",
        mature.iter().filter(|t| cycles(t) == 0).count(),
        mature.iter().filter(|t| cycles(t) == 1).count(),
        mature
            .iter()
            .filter(|t| (2..=3).contains(&cycles(t)))
            .count(),
        mature.iter().filter(|t| cycles(t) >= 4).count()
    );
    let ai: Vec<u64> = mature
        .iter()
        .flat_map(|t| t.asleep_intervals.clone())
        .collect();
    println!(
        "  sleep durations before a re-wake: {} intervals, median {} ticks, p90 {} ticks",
        ai.len(),
        med(ai.clone()),
        {
            let mut v = ai.clone();
            v.sort_unstable();
            v.get(v.len() * 9 / 10).copied().unwrap_or(0)
        }
    );
    println!(
        "  dormancy residence: {} bodies dormant at the end, total dormant body-ticks {}, entries {}, reactivations {}",
        mature.iter().filter(|t| t.state == 2).count(),
        mature.iter().map(|t| t.dormant).sum::<u64>(),
        mature
            .iter()
            .map(|t| u64::from(t.dormant_entries))
            .sum::<u64>(),
        mature
            .iter()
            .map(|t| u64::from(t.reactivations))
            .sum::<u64>()
    );

    // Cohorts followed from creation, with the actual triggers of their wakes.
    let audit = sim.world().wake_audit().unwrap();
    let mut by_cohort: [BTreeMap<&'static str, u64>; 3] = Default::default();
    let mut bodies_with: [BTreeMap<&'static str, std::collections::BTreeSet<u64>>; 3] =
        Default::default();
    for ev in &audit.events {
        if let Some(c) = tracks.get(&ev.entity).and_then(|t| t.cohort) {
            // A physics-step wake is split by what the same tick committed: a terrain dig, a body
            // edit, or nothing (a contact / island wake with no edit).
            let label: &'static str = if ev.reason.starts_with("physics.step") {
                match tick_class.get(ev.tick as usize).copied().unwrap_or(0) {
                    0 => "physics.step, no edit committed that tick (contact/island)",
                    1 => "physics.step, terrain dig committed that tick",
                    2 => "physics.step, body edit committed that tick",
                    _ => "physics.step, dig and body edit that tick",
                }
            } else {
                ev.reason
            };
            *by_cohort[c].entry(label).or_default() += 1;
            bodies_with[c].entry(label).or_default().insert(ev.entity);
        }
    }
    for (c, (lo, hi)) in COHORT_WINDOWS.iter().enumerate() {
        let members: Vec<&Track> = tracks.values().filter(|t| t.cohort == Some(c)).collect();
        if members.is_empty() {
            continue;
        }
        let n = members.len();
        let life: Vec<u64> = members.iter().map(|t| end - t.born).collect();
        let fs: Vec<u64> = members.iter().filter_map(|t| t.first_sleep_after).collect();
        println!(
            "COHORT {c} (created ticks {lo}-{hi}): {n} bodies, lifetime to end {} ticks (median)",
            med(life)
        );
        println!(
            "  ever asleep or dormant: {}/{n}; never settled: {}; time to first settle: median {} ticks (n={})",
            fs.len(),
            n - fs.len(),
            med(fs.clone()),
            fs.len()
        );
        println!(
            "  awake fraction of life: median {:.2}; longest awake interval: median {} ticks, max {}",
            {
                let mut v: Vec<f64> = members
                    .iter()
                    .map(|t| t.awake as f64 / (end - t.born).max(1) as f64)
                    .collect();
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            },
            med(members.iter().map(|t| t.longest_awake).collect()),
            members.iter().map(|t| t.longest_awake).max().unwrap_or(0)
        );
        println!(
            "  cycles (asleep->awake + reactivations) per body: 0: {}, 1: {}, 2-3: {}, 4+: {}; dormant entries {}; final state awake/asleep/dormant: {}/{}/{}",
            members.iter().filter(|t| cycles(t) == 0).count(),
            members.iter().filter(|t| cycles(t) == 1).count(),
            members
                .iter()
                .filter(|t| (2..=3).contains(&cycles(t)))
                .count(),
            members.iter().filter(|t| cycles(t) >= 4).count(),
            members
                .iter()
                .map(|t| u64::from(t.dormant_entries))
                .sum::<u64>(),
            members.iter().filter(|t| t.state == 0).count(),
            members.iter().filter(|t| t.state == 1).count(),
            members.iter().filter(|t| t.state == 2).count()
        );
        println!("  wake triggers (events / distinct bodies):");
        for (reason, n_ev) in &by_cohort[c] {
            println!(
                "    {reason:<48} {n_ev:>6} events, {:>4} bodies",
                bodies_with[c][reason].len()
            );
        }
    }
}
