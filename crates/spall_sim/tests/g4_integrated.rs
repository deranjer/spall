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
    for i in 0..vfix::G4_TOWER_COUNT as u64 {
        let e = vfix::g4_blast(i).expect("tower available");
        assert_eq!(e.radius, vfix::G4_BLAST_RADIUS_CELLS);
        assert!(blasts.insert(e.entity), "blast {i} repeats a tower");
    }
    assert_eq!(blasts.len(), 180, "180 blasts in the 30-minute lane");
    assert!(vfix::g4_blast(180).is_none());
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
        spall_sim::world::solid_cells(&shed.volume) >= 500,
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
