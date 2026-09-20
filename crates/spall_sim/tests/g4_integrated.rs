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
