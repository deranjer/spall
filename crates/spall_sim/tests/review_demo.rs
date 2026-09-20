//! Review demonstrations (2026-09-20): frame dumps for two short visual demos, written as
//! JSON for `.local/reviews/2026-09-20-wake-locality/demos/`. Both are `#[ignore]`d
//! diagnostics; they only read simulation state.
//!
//! * `demo_lever_dump` — the review's unbalanced lever on a broad dynamic base.
//! * `demo_escape_dump` — a flight recorder around one rubble rod in the long workload:
//!   pose, velocity, sleep state, collider revision, nearby bodies and the edits that
//!   committed, for a window of ticks.
//!
//! `DEMO_OUT` names the output file; `ESCAPE_ENTITY`, `ESCAPE_FROM`, `ESCAPE_TO` pick the
//! entity and tick window of the flight recorder.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{CELLS_PER_BRICK, CellSizeCode, EntityId, GlobalCell, LocalCell, SphereBrush};
use spall_protocol::RequestId;
use spall_sim::fixtures;
use spall_sim::{BodyPose, EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::fixtures as vfix;

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

/// Local-space column runs `[x0, x1, y0, y1]` (metres) of a body, projected on z, so the
/// browser can draw the body's real cells without shipping every voxel.
fn body_runs(body: &spall_sim::body::Body) -> Vec<[f64; 4]> {
    let cm = body.cell_size().metres();
    let mut cols: BTreeMap<(i64, i64), ()> = BTreeMap::new();
    let mut total = 0u64;
    for coord in body.volume.resident_brick_coords() {
        let Some(snap) = body.volume.snapshot_brick(coord).ok().flatten() else {
            continue;
        };
        for index in 0..CELLS_PER_BRICK as u16 {
            let local = LocalCell::from_linear_index(index).expect("index < 32768");
            if snap.get(local).is_air() {
                continue;
            }
            total += 1;
            cols.insert(
                (
                    coord.x * 32 + local.x() as i64,
                    coord.y * 32 + local.y() as i64,
                ),
                (),
            );
        }
    }
    if total == 0 {
        return Vec::new();
    }
    // Merge horizontally adjacent columns of one row into runs.
    let mut runs = Vec::new();
    let mut by_row: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (x, y) in cols.keys() {
        by_row.entry(*y).or_default().push(*x);
    }
    for (y, xs) in by_row {
        let mut start = xs[0];
        let mut prev = xs[0];
        for &x in &xs[1..] {
            if x != prev + 1 {
                runs.push([
                    start as f64 * cm,
                    (prev + 1) as f64 * cm,
                    y as f64 * cm,
                    (y + 1) as f64 * cm,
                ]);
                start = x;
            }
            prev = x;
        }
        runs.push([
            start as f64 * cm,
            (prev + 1) as f64 * cm,
            y as f64 * cm,
            (y + 1) as f64 * cm,
        ]);
    }
    runs
}

struct Dump {
    title: String,
    terrain: Vec<[f64; 4]>,
    geometry: BTreeMap<String, (String, Vec<[f64; 4]>)>,
    frames: Vec<String>,
    focus: u64,
}

impl Dump {
    fn new(title: &str, focus: u64, terrain: Vec<[f64; 4]>) -> Self {
        Self {
            title: title.into(),
            terrain,
            geometry: BTreeMap::new(),
            frames: Vec::new(),
            focus,
        }
    }

    fn frame(&mut self, tick: u64, sim: &Simulation, ids: &[u64], note: &str) {
        let mut s = String::new();
        write!(s, "{{\"t\":{tick},\"note\":{:?},\"b\":[", note).unwrap();
        let mut first = true;
        for id in ids {
            let Some(b) = EntityId::new(*id).ok().and_then(|e| sim.world().body(e)) else {
                continue;
            };
            self.geometry
                .entry(format!("{id}:{}", b.collider_revision))
                .or_insert_with(|| (format!("entity {id}"), body_runs(b)));
            if !first {
                s.push(',');
            }
            first = false;
            let q = b.pose.rotation;
            let t = b.pose.translation_m;
            write!(
                s,
                "[{id},{:.4},{:.4},{:.4},{:.5},{:.5},{:.5},{:.5},{},{},{:.3},{:.3},{:.3},{}]",
                t[0],
                t[1],
                t[2],
                q.x,
                q.y,
                q.z,
                q.w,
                u8::from(b.sleeping),
                u8::from(b.dormant),
                b.linvel_m_s[0],
                b.linvel_m_s[1],
                b.linvel_m_s[2],
                b.collider_revision
            )
            .unwrap();
        }
        s.push_str("]}");
        self.frames.push(s);
    }

    fn write(&self, path: &str) {
        let mut out = String::new();
        write!(
            out,
            "{{\"title\":{:?},\"focus\":{},\"terrain\":{:?},\"geometry\":{{",
            self.title, self.focus, self.terrain
        )
        .unwrap();
        let mut first = true;
        for (id, (name, runs)) in &self.geometry {
            if !first {
                out.push(',');
            }
            first = false;
            write!(out, "\"{id}\":{{\"name\":{:?},\"runs\":{:?}}}", name, runs).unwrap();
        }
        out.push_str("},\"frames\":[");
        out.push_str(&self.frames.join(","));
        out.push_str("]}");
        std::fs::write(path, out).unwrap();
        println!("wrote {} frames to {path}", self.frames.len());
    }
}

fn lever(id: spall_core::VolumeId) -> spall_voxel::Volume {
    let mut v = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
    for (lo, hi) in [([30, 0, 8], [30, 3, 15]), ([9, 4, 8], [51, 11, 15])] {
        v.apply_edit(&spall_voxel::EditPlan::filled_box(
            id,
            GlobalCell::new(lo[0], lo[1], lo[2]),
            GlobalCell::new(hi[0], hi[1], hi[2]),
            fixtures::STONE,
        ))
        .unwrap();
    }
    v
}

/// The review's case: the unbalanced lever rests on a separate broad dynamic base.
#[test]
#[ignore = "demo frame dump"]
fn demo_lever_dump() {
    let out = std::env::var("DEMO_OUT").expect("DEMO_OUT");
    let title = std::env::var("DEMO_TITLE").unwrap_or_else(|_| "lever".into());
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    let base = sim
        .world_mut()
        .spawn_body(
            |id| {
                let mut v = spall_voxel::Volume::new(id, CellSizeCode::Quarter);
                v.apply_edit(&spall_voxel::EditPlan::filled_box(
                    id,
                    GlobalCell::new(0, 0, 0),
                    GlobalCell::new(63, 3, 47),
                    fixtures::STONE,
                ))
                .unwrap();
                v
            },
            BodyPose::new(glam::DQuat::IDENTITY, [28.0, 1.0, 28.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    let lever_id = sim
        .world_mut()
        .spawn_body(
            lever,
            BodyPose::new(glam::DQuat::IDENTITY, [30.0, 2.0, 30.0]),
            [0.0; 3],
            [0.0; 3],
            2600.0,
            0,
        )
        .unwrap();
    for _ in 0..600 {
        sim.tick().unwrap();
    }
    let ids = [base.get(), lever_id.get()];
    // Ground slab top y = 1 m; view window x 25..48 m.
    let mut d = Dump::new(&title, lever_id.get(), vec![[0.0, 0.0, 96.0, 1.0]]);
    for t in 0..30 {
        d.frame(t, &sim, &ids, "settled: both bodies asleep, no edit yet");
        sim.tick().unwrap();
    }
    sim.submit(EditIntent::cut(
        RequestId(1),
        actor(),
        EditTarget::Body(lever_id),
        brush_cell([13, 7, 11], 4),
    ))
    .unwrap();
    for t in 30..330 {
        let note = if t < 32 {
            "edit: counterweight cut off the left end"
        } else {
            ""
        };
        sim.tick().unwrap();
        d.frame(t, &sim, &ids, note);
    }
    d.write(&out);
}

/// Flight recorder around one entity of the long workload.
#[test]
#[ignore = "demo frame dump"]
fn demo_escape_dump() {
    let out = std::env::var("DEMO_OUT").expect("DEMO_OUT");
    let title = std::env::var("DEMO_TITLE").unwrap_or_else(|_| "escape".into());
    let focus: u64 = std::env::var("ESCAPE_ENTITY").unwrap().parse().unwrap();
    let from: u64 = std::env::var("ESCAPE_FROM").unwrap().parse().unwrap();
    let to: u64 = std::env::var("ESCAPE_TO").unwrap().parse().unwrap();
    let mut sim = Simulation::new(SimulationConfig::new(fixtures::g4_integrated_setup())).unwrap();
    let bodies = fixtures::spawn_g4_integrated_bodies(
        sim.world_mut(),
        fixtures::G4_INTEGRATED_SEPARATED_SPAWNS[0],
    );
    let mut policy = spall_sim::DormancyPolicy::new(spall_sim::DormancyConfig::DEFAULT);
    let (mut req, mut ordinary, mut blast, mut digs, mut comb_edits) =
        (1u64, 0u64, 0u64, 0u64, 0u64);
    let mut desc: HashMap<u64, String> = HashMap::new();
    let mut terrain_rev = 0u64;
    // Terrain silhouette: slab 96 m x 1 m, west wall 1 m x 6 m.
    let mut d = Dump::new(
        &title,
        focus,
        vec![[0.0, 0.0, 96.0, 1.0], [0.0, 1.0, 1.0, 7.5]],
    );
    for t in 0..to {
        if t == 60 {
            let e = vfix::g4_giant_cut();
            desc.insert(req, "giant cut".into());
            sim.submit(EditIntent::cut(
                RequestId(req),
                actor(),
                EditTarget::Body(EntityId::new(e.entity).unwrap()),
                brush_cell(e.cell, e.radius),
            ))
            .unwrap();
            req += 1;
        }
        if t >= 120 && (t - 120).is_multiple_of(6) {
            if ordinary % 10 == 9 {
                let (cell, radius) = vfix::g4_terrain_dig(digs).unwrap();
                digs += 1;
                desc.insert(req, format!("terrain dig at cell {cell:?}"));
                sim.submit(EditIntent::cut(
                    RequestId(req),
                    actor(),
                    EditTarget::Terrain,
                    brush_cell(cell, radius),
                ))
                .unwrap();
            } else {
                let e = vfix::g4_ordinary_edit(comb_edits).unwrap();
                comb_edits += 1;
                desc.insert(
                    req,
                    format!("comb cut: entity {} cell {:?}", e.entity, e.cell),
                );
                sim.submit(EditIntent::cut(
                    RequestId(req),
                    actor(),
                    EditTarget::Body(EntityId::new(e.entity).unwrap()),
                    brush_cell(e.cell, e.radius),
                ))
                .unwrap();
            }
            ordinary += 1;
            req += 1;
        }
        if t >= 120 && (t - 120) % 600 == 300 {
            let e = vfix::g4_blast(blast).unwrap();
            blast += 1;
            desc.insert(req, format!("blast on tower {}", e.entity));
            sim.submit(EditIntent::cut(
                RequestId(req),
                actor(),
                EditTarget::Body(EntityId::new(e.entity).unwrap()),
                brush_cell(e.cell, e.radius),
            ))
            .unwrap();
            req += 1;
        }
        fixtures::agitate_g4_bodies(sim.world_mut(), &bodies.active, t);
        let report = sim.tick().unwrap();
        let plan = sim.apply_dormancy(&mut policy, &report);
        if t + 1 < from {
            continue;
        }
        let Some(fb) = EntityId::new(focus).ok().and_then(|e| sim.world().body(e)) else {
            continue;
        };
        let fp = fb.pose.translation_m;
        let mut note = String::new();
        for (rid, _) in &report.committed {
            let _ = write!(
                note,
                "commit #{}: {}; ",
                rid.0,
                desc.get(&rid.0).cloned().unwrap_or_default()
            );
        }
        let trev = sim.world().terrain().collider_revision;
        if trev != terrain_rev {
            terrain_rev = trev;
            note.push_str("terrain collider rebuilt; ");
        }
        for e in plan.deactivate.iter().chain(plan.reactivate.iter()) {
            let (dx, dz) = (
                sim.world()
                    .body(*e)
                    .map_or(1e9, |b| b.pose.translation_m[0] - fp[0]),
                sim.world()
                    .body(*e)
                    .map_or(1e9, |b| b.pose.translation_m[2] - fp[2]),
            );
            if dx.abs() < 4.0 && dz.abs() < 4.0 {
                let _ = write!(note, "dormancy change on nearby entity {}; ", e.get());
            }
        }
        // Nearby bodies: within 3 m in x/z of the focus body and low enough to touch it.
        let mut ids = vec![focus];
        for b in sim.world().bodies() {
            let Some(e) = b.entity else { continue };
            if e.get() == focus {
                continue;
            }
            let p = b.pose.translation_m;
            let region_ok = b.solid_estimate_small();
            if (p[0] - fp[0]).abs() < 3.0
                && (p[2] - fp[2]).abs() < 3.0
                && p[1] < fp[1] + 8.0
                && p[1] > fp[1] - 8.0
                && region_ok
            {
                ids.push(e.get());
            }
        }
        d.frame(t + 1, &sim, &ids, &note);
    }
    d.write(&out);
}

trait SmallBody {
    fn solid_estimate_small(&self) -> bool;
}
impl SmallBody for spall_sim::body::Body {
    /// Drawing every huge body (the giant, combs) would bloat the dump; draw bodies
    /// whose collider region is at most 24 m on a side.
    fn solid_estimate_small(&self) -> bool {
        let (lo, hi) = self.collider_region;
        let m = self.cell_size().metres();
        ((hi.x - lo.x + 1) as f64 * m) <= 24.0
            && ((hi.y - lo.y + 1) as f64 * m) <= 24.0
            && ((hi.z - lo.z + 1) as f64 * m) <= 24.0
    }
}
