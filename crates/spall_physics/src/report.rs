//! The T06 feasibility harness: runs every acceptance scenario against both
//! collider representations and collects the measurements the ticket asks for
//! (p50/p95/p99 step time, rebuild cost, primitive count, estimated memory,
//! settle behaviour, mass agreement, tunnelling).
//!
//! [`run_feasibility`] is deterministic in *structure* (same scenes, same
//! iteration counts) but its timing numbers are machine-dependent, so the
//! CI-run tests here assert only on behaviour (settles, no blow-up, interior
//! preserved, no tunnel, mass within tolerance). The absolute percentile
//! numbers are produced by the `collision-bench` binary and recorded in
//! `docs/collision-decision.md`.

use crate::collider::{Representation, build_collider};
use crate::fixtures;
use crate::mass::analytic_mass_properties;
use crate::metrics::{DurationSamples, PercentileSummary};
use crate::occupancy::OccupancyGrid;
use crate::world::{BodyKind, BodySpec, PhysicsConfig, PhysicsWorld};
use spall_core::VolumeId;

/// Knobs for one feasibility run. `small()` is the CI-sized configuration;
/// `collision-bench` uses larger counts.
#[derive(Debug, Clone, Copy)]
pub struct FeasibilityParams {
    /// Connected-body size, in bricks per axis, for the build/stress scenario.
    pub multibrick: [i64; 3],
    /// Number of loose debris pieces for the settle scenario.
    pub debris_count: usize,
    /// Steps to run each drop/settle scene.
    pub settle_steps: u32,
    /// Number of collider rebuilds to time for the edit-cost scenario.
    pub rebuild_iters: u32,
    /// Number of one-shot 64-brick collider builds to time for the build-cost
    /// percentiles.
    pub build_iters: u32,
    /// Projectile speed, m/s, for the fast-object scenario.
    pub projectile_speed: f32,
}

impl FeasibilityParams {
    /// A configuration cheap enough for `cargo test` on CI.
    pub fn small() -> Self {
        Self {
            multibrick: [2, 2, 2],
            debris_count: 64,
            settle_steps: 180,
            rebuild_iters: 12,
            build_iters: 24,
            projectile_speed: 220.0,
        }
    }

    /// A heavier configuration for the standalone bench: the full 64-brick body
    /// and 256 debris pieces from the ticket.
    pub fn gate() -> Self {
        Self {
            multibrick: [4, 4, 4],
            debris_count: 256,
            settle_steps: 420,
            rebuild_iters: 60,
            build_iters: 64,
            projectile_speed: 220.0,
        }
    }
}

/// Result of the sleep/wake feasibility scenario for one representation. Every
/// field is observed from the solver directly — `sleeping` transitions, contact
/// pairs, and travelled distance — so waking is never inferred from a count of
/// bodies that slept.
#[derive(Debug, Clone, Copy)]
pub struct SleepWakeReport {
    /// The body settled to sleep on the floor.
    pub slept: bool,
    /// It woke when its collider was rebuilt in place (the T08 nearby-edit path).
    pub woke_on_rebuild: bool,
    /// It settled back to sleep after the rebuild.
    pub reslept_after_rebuild: bool,
    /// It woke when a blast impulse was applied.
    pub woke_on_impulse: bool,
    /// Furthest it travelled from its rest pose after the impulse, metres.
    pub travel_m: f64,
    /// It left the floor and then re-established a floor contact (it collides).
    pub recontact: bool,
    /// It settled back to sleep a second time.
    pub reslept: bool,
    /// Every state stayed finite and contact penetration stayed bounded across
    /// the whole scenario (no blow-up on wake).
    pub stable: bool,
}

impl SleepWakeReport {
    /// Compact JSON object for the bench output.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"slept\":{},\"woke_on_rebuild\":{},\"reslept_after_rebuild\":{},\"woke_on_impulse\":{},\"travel_m\":{:.4},\"recontact\":{},\"reslept\":{},\"stable\":{}}}",
            self.slept,
            self.woke_on_rebuild,
            self.reslept_after_rebuild,
            self.woke_on_impulse,
            self.travel_m,
            self.recontact,
            self.reslept,
            self.stable,
        )
    }

    /// Whole scenario passed: slept, both interactions woke it, it moved and
    /// re-collided, it re-slept, and nothing blew up.
    pub fn ok(&self) -> bool {
        self.slept
            && self.woke_on_rebuild
            && self.reslept_after_rebuild
            && self.woke_on_impulse
            && self.travel_m > 0.1
            && self.recontact
            && self.reslept
            && self.stable
    }
}

/// Per-representation results.
#[derive(Debug, Clone)]
pub struct RepresentationReport {
    /// `"native_voxels"` or `"merged_cuboids"`.
    pub representation: &'static str,

    /// Primitive count for the 64-brick connected body.
    pub multibrick_primitives: usize,
    /// **Complete** occupancy → collider build time for that body across
    /// `build_iters` builds, microseconds: native index extraction / greedy
    /// decomposition, all shape allocation, and the Rapier wrapping.
    pub multibrick_build: PercentileSummary,
    /// Component of the build spent only in the final Rapier shape wrapping,
    /// across the same builds, microseconds. Retained so the wrap-only figure
    /// stays visible next to the complete cost.
    pub multibrick_wrap: PercentileSummary,
    /// Estimated collider memory for that body, bytes.
    pub multibrick_est_bytes: usize,

    /// Per-step pipeline time across the debris settle scene.
    pub settle_step: PercentileSummary,
    /// Fraction of debris pieces asleep at the end of the settle scene.
    pub settle_sleep_fraction: f64,
    /// Largest linear speed of any debris piece over the final 30 steps, m/s.
    pub settle_max_speed: f64,
    /// True if no body produced a non-finite state at any step.
    pub settle_finite: bool,

    /// Editable-collider sleep/wake scenario (measured directly, never inferred
    /// from a sleeping count): a dynamic voxel body is settled to sleep, then two
    /// documented nearby interactions are applied in turn — an in-place collider
    /// rebuild (the T08 edit path) and a blast impulse — and its response is
    /// checked. `true` means the body actually reached that state.
    pub sleep_wake: SleepWakeReport,

    /// Per-step pipeline time while the hollow building falls and lands.
    pub building_step: PercentileSummary,
    /// Interior clearance (ray from the centre) after settling, metres.
    pub building_interior_clearance_m: f64,
    /// True if the building came to rest.
    pub building_settled: bool,

    /// Collider rebuild cost after an edit, across `rebuild_iters` rebuilds.
    pub rebuild: PercentileSummary,

    /// Relative mass error vs the analytic reference for the hollow building.
    pub mass_rel_err: f64,
    /// Absolute centre-of-mass error vs the analytic reference, metres.
    pub com_abs_err_m: f64,
    /// Relative error of the sorted principal inertia vs the analytic reference.
    pub inertia_rel_err: f64,

    /// Highest CCD projectile speed (m/s) the thin wall still stopped.
    pub projectile_max_stop_m_s: f64,

    /// Solid cells in the worst-case fragmentation probe (a 3D checkerboard).
    pub worst_case_solid_cells: u64,
    /// Collider primitives for that probe: `1` for the voxel shape, or the box
    /// count the greedy decomposition degenerates to (one per isolated cell).
    pub worst_case_primitives: usize,
}

impl RepresentationReport {
    /// Compact JSON object for the bench output.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"representation\":\"{}\",\"multibrick_primitives\":{},\"multibrick_build\":{},\"multibrick_wrap\":{},\"multibrick_est_bytes\":{},\"settle_step\":{},\"settle_sleep_fraction\":{:.4},\"settle_max_speed\":{:.4},\"settle_finite\":{},\"sleep_wake\":{},\"building_step\":{},\"building_interior_clearance_m\":{:.4},\"building_settled\":{},\"rebuild\":{},\"mass_rel_err\":{:.6},\"com_abs_err_m\":{:.6},\"inertia_rel_err\":{:.6},\"projectile_max_stop_m_s\":{:.1},\"worst_case_solid_cells\":{},\"worst_case_primitives\":{}}}",
            self.representation,
            self.multibrick_primitives,
            self.multibrick_build.to_json(),
            self.multibrick_wrap.to_json(),
            self.multibrick_est_bytes,
            self.settle_step.to_json(),
            self.settle_sleep_fraction,
            self.settle_max_speed,
            self.settle_finite,
            self.sleep_wake.to_json(),
            self.building_step.to_json(),
            self.building_interior_clearance_m,
            self.building_settled,
            self.rebuild.to_json(),
            self.mass_rel_err,
            self.com_abs_err_m,
            self.inertia_rel_err,
            self.projectile_max_stop_m_s,
            self.worst_case_solid_cells,
            self.worst_case_primitives,
        )
    }
}

/// Full feasibility report: both representations under identical scenes.
#[derive(Debug, Clone)]
pub struct FeasibilityReport {
    /// Parameters used.
    pub params: FeasibilityParams,
    /// Native voxel shape results.
    pub native: RepresentationReport,
    /// Merged-cuboid compound results.
    pub compound: RepresentationReport,
}

impl FeasibilityReport {
    /// Compact JSON for the bench output.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"debris_count\":{},\"multibrick_bricks\":[{},{},{}],\"settle_steps\":{},\"rebuild_iters\":{},\"native\":{},\"compound\":{}}}",
            self.params.debris_count,
            self.params.multibrick[0],
            self.params.multibrick[1],
            self.params.multibrick[2],
            self.params.settle_steps,
            self.params.rebuild_iters,
            self.native.to_json(),
            self.compound.to_json(),
        )
    }
}

/// Runs the whole feasibility suite for both representations.
pub fn run_feasibility(params: FeasibilityParams) -> FeasibilityReport {
    FeasibilityReport {
        params,
        native: run_one(Representation::NativeVoxels, params),
        compound: run_one(Representation::MergedCuboids, params),
    }
}

fn run_one(rep: Representation, params: FeasibilityParams) -> RepresentationReport {
    let (mb_primitives, mb_build, mb_wrap, mb_bytes) =
        multibrick_build(rep, params.multibrick, params.build_iters);
    let (settle_step, sleep_fraction, max_speed, finite) = debris_settle(rep, params);
    let sleep_wake = sleep_wake_cycle(rep);
    let (building_step, clearance, settled) = building_drop(rep, params.settle_steps);
    let rebuild = rebuild_cost(rep, params.rebuild_iters);
    let (mass_rel, com_abs, inertia_rel) = mass_agreement(rep);
    let projectile_max_stop_m_s = projectile_threshold(rep, params.projectile_speed);
    let (worst_case_solid_cells, worst_case_primitives) = worst_case_fragmentation(rep);

    RepresentationReport {
        representation: rep.label(),
        multibrick_primitives: mb_primitives,
        multibrick_build: mb_build,
        multibrick_wrap: mb_wrap,
        multibrick_est_bytes: mb_bytes,
        settle_step,
        settle_sleep_fraction: sleep_fraction,
        settle_max_speed: max_speed,
        settle_finite: finite,
        sleep_wake,
        building_step,
        building_interior_clearance_m: clearance,
        building_settled: settled,
        rebuild,
        mass_rel_err: mass_rel,
        com_abs_err_m: com_abs,
        inertia_rel_err: inertia_rel,
        projectile_max_stop_m_s,
        worst_case_solid_cells,
        worst_case_primitives,
    }
}

/// A 3D checkerboard in a 16³ region: every occupied cell is isolated, so the
/// greedy box decomposition cannot merge anything and degenerates to one box
/// per cell. Reports `(solid cells, collider primitives)` — the number that
/// bounds when the merged-cuboid path needs a coarse-fracture fallback.
fn worst_case_fragmentation(rep: Representation) -> (u64, usize) {
    use spall_core::{CellSizeCode, GlobalCell, VolumeId};
    use spall_voxel::{EditPlan, Volume, fixtures as vox};
    let id = VolumeId::new(1).unwrap();
    let mut v = Volume::new(id, CellSizeCode::Quarter);
    let mut plan = EditPlan::new(id);
    for z in 0..16 {
        for y in 0..16 {
            for x in 0..16 {
                if (x + y + z) % 2 == 0 {
                    plan.set(GlobalCell::new(x, y, z), vox::STONE);
                }
            }
        }
    }
    v.apply_edit(&plan).expect("checkerboard edit");
    let grid =
        OccupancyGrid::from_region(&v, GlobalCell::new(0, 0, 0), GlobalCell::new(15, 15, 15))
            .unwrap();
    let built = build_collider(&grid, fixtures::CELL_M, rep);
    (grid.solid_count(), built.primitives)
}

fn vid(n: u64) -> VolumeId {
    VolumeId::new(n).unwrap()
}

/// Times the **complete** occupancy → collider build of the named 64-brick
/// connected body (`iters` builds): native solid-index extraction / greedy
/// decomposition, every shape allocation, and the Rapier wrapping. Returns the
/// primitive count, the complete-build percentile summary, the wrap-only
/// component summary, and the estimated collider memory.
fn multibrick_build(
    rep: Representation,
    bricks: [i64; 3],
    iters: u32,
) -> (usize, PercentileSummary, PercentileSummary, usize) {
    let v = fixtures::connected_multibrick(vid(1), bricks, true);
    let grid = OccupancyGrid::from_volume(&v)
        .expect("resident")
        .expect("non-empty");

    let mut build = DurationSamples::new();
    let mut wrap = DurationSamples::new();
    let mut primitives = 0;
    let mut est_bytes = 0;
    for _ in 0..iters.max(1) {
        let built = build_collider(&grid, fixtures::CELL_M, rep);
        build.push(built.build);
        wrap.push(built.wrap);
        primitives = built.primitives;
        est_bytes = built.est_bytes;
    }
    (primitives, build.summary_us(), wrap.summary_us(), est_bytes)
}

fn debris_settle(
    rep: Representation,
    params: FeasibilityParams,
) -> (PercentileSummary, f64, f64, bool) {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());

    // Fixed floor: shallow slab, top surface at y = 1.0 m.
    let floor = fixtures::floor_slab(vid(1), 2, 2, 4);
    let floor_grid = OccupancyGrid::from_volume(&floor).unwrap().unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: rep,
        grid: floor_grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    // Loose grid of pieces above the floor.
    let pieces = fixtures::debris_pieces(100, params.debris_count, 2);
    let per_row = (params.debris_count as f64).cbrt().ceil() as usize;
    let mut ids = Vec::with_capacity(pieces.len());
    for (n, (_, v)) in pieces.iter().enumerate() {
        let grid = OccupancyGrid::from_volume(v).unwrap().unwrap();
        let (ix, iy, iz) = (
            n % per_row,
            (n / per_row) % per_row,
            n / (per_row * per_row),
        );
        let id = world.add_body(BodySpec {
            kind: BodyKind::Dynamic { ccd: false },
            representation: rep,
            grid,
            cell_m: fixtures::CELL_M,
            density_kg_m3: fixtures::STONE_DENSITY,
            translation_m: [
                2.0 + ix as f32 * 0.9,
                2.0 + iy as f32 * 0.9,
                2.0 + iz as f32 * 0.9,
            ],
            linvel_m_s: [0.0; 3],
        });
        ids.push(id);
    }

    let mut step = DurationSamples::new();
    let mut finite = true;
    let mut max_speed = 0.0_f64;
    for s in 0..params.settle_steps {
        let t = world.step();
        step.push(t.pipeline);
        let tail = s + 30 >= params.settle_steps;
        for id in &ids {
            let st = world.body_state(*id);
            if !st.is_finite() {
                finite = false;
            }
            if tail {
                max_speed = max_speed.max(st.speed_m_s() as f64);
            }
        }
    }
    let asleep = ids
        .iter()
        .filter(|id| world.body_state(**id).sleeping)
        .count();
    (
        step.summary_us(),
        asleep as f64 / ids.len() as f64,
        max_speed,
        finite,
    )
}

/// T06's sleep/wake acceptance scenario, run for one representation.
///
/// Settles one dynamic voxel body to sleep on a fixed floor, then applies two
/// documented nearby interactions in sequence and checks the body's real
/// response each time:
///
/// 1. an **in-place collider rebuild** — exactly what the T08 authoritative edit
///    path does to a body after an accepted topology transaction;
/// 2. a **blast impulse** sized from the body's own mass for a ~4.5 m/s kick —
///    a stand-in for a nearby explosion or impact.
///
/// After each it verifies the body left the sleeping state; after the impulse it
/// verifies the body travelled, left the floor, and re-established a floor
/// contact; and it checks that the body settles back to sleep both times with
/// every state finite and contact penetration bounded. Nothing is inferred from
/// a count of bodies that slept.
fn sleep_wake_cycle(rep: Representation) -> SleepWakeReport {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());

    let floor = fixtures::floor_slab(vid(1), 2, 2, 4);
    let floor_grid = OccupancyGrid::from_volume(&floor).unwrap().unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: rep,
        grid: floor_grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    // One small dynamic cube just above the slab (its top face at y = 1.0 m).
    let piece = fixtures::debris_pieces(500, 1, 2).pop().unwrap().1;
    let grid = OccupancyGrid::from_volume(&piece).unwrap().unwrap();
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: rep,
        grid: grid.clone(),
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [4.0, 1.2, 4.0],
        linvel_m_s: [0.0; 3],
    });

    let mut stable = true;
    let check = |w: &PhysicsWorld| {
        w.body_state(id).is_finite() && w.max_penetration_m() < 0.5 * fixtures::CELL_M
    };
    let settle = |w: &mut PhysicsWorld, stable: &mut bool, budget: u32| -> bool {
        let mut slept = false;
        for _ in 0..budget {
            w.step();
            *stable &= check(w);
            if w.body_state(id).sleeping {
                slept = true;
                break;
            }
        }
        // A few extra steps so the body is firmly at rest.
        for _ in 0..10 {
            w.step();
            *stable &= check(w);
        }
        slept
    };

    // Phase 1: settle to sleep.
    let slept = settle(&mut world, &mut stable, 600);

    // Phase 2: rebuild the collider in place (the T08 nearby-edit path).
    world.rebuild_collider(id, &grid, rep);
    world.step();
    stable &= check(&world);
    let woke_on_rebuild = !world.body_state(id).sleeping;
    let reslept_after_rebuild = settle(&mut world, &mut stable, 400);
    let rest = world.body_state(id).translation_m;

    // Phase 3: a blast impulse sized from the body's own mass (~4.5 m/s kick).
    let m = world.body_state(id).mass_kg.max(f32::MIN_POSITIVE);
    world.apply_impulse(id, [2.0 * m, 4.0 * m, 0.0]);
    world.step();
    stable &= check(&world);
    let after = world.body_state(id);
    let woke_on_impulse = !after.sleeping && after.speed_m_s() > 1.0;

    // Phase 4: it flies up off the floor, then falls back and lands on it — a
    // clear rise clear of the surface followed by a live floor contact back near
    // the rest height is unambiguous "woke, moved, and collided".
    let mut max_rise_m = 0.0_f64;
    let mut recontact = false;
    let mut travel_m = 0.0_f64;
    for _ in 0..240 {
        world.step();
        stable &= check(&world);
        let st = world.body_state(id);
        let d = ((st.translation_m[0] - rest[0]).powi(2)
            + (st.translation_m[1] - rest[1]).powi(2)
            + (st.translation_m[2] - rest[2]).powi(2))
        .sqrt() as f64;
        travel_m = travel_m.max(d);
        let rise_m = f64::from(st.translation_m[1] - rest[1]);
        max_rise_m = max_rise_m.max(rise_m);
        if max_rise_m > 0.25 && rise_m < 0.15 && world.contact_pair_count() > 0 {
            recontact = true;
        }
    }

    // Phase 5: settle back to sleep a second time.
    let reslept = settle(&mut world, &mut stable, 600);

    SleepWakeReport {
        slept,
        woke_on_rebuild,
        reslept_after_rebuild,
        woke_on_impulse,
        travel_m,
        recontact,
        reslept,
        stable,
    }
}

fn building_drop(rep: Representation, steps: u32) -> (PercentileSummary, f64, bool) {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());

    let floor = fixtures::floor_slab(vid(1), 2, 2, 4);
    let floor_grid = OccupancyGrid::from_volume(&floor).unwrap().unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: rep,
        grid: floor_grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    // Floor slab top is at y = 4 cells * 0.25 m = 1.0 m; drop the building a
    // short distance onto it, centred over the slab (16 m wide, building 3 m).
    let building = fixtures::hollow_building(vid(2));
    let grid = OccupancyGrid::from_volume(&building).unwrap().unwrap();
    let dims = grid.dims();
    // The collider honours the grid origin as a body-local offset, so place the
    // body so the building's solid cells still drop from just above the slab.
    let origin = grid.origin();
    let cell = fixtures::CELL_M;
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: rep,
        grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [
            6.5 - origin.x as f32 * cell,
            1.3 - origin.y as f32 * cell,
            6.5 - origin.z as f32 * cell,
        ],
        linvel_m_s: [0.0; 3],
    });

    let mut step = DurationSamples::new();
    let mut finite = true;
    for _ in 0..steps {
        step.push(world.step().pipeline);
        if !world.body_state(id).is_finite() {
            finite = false;
        }
    }

    let st = world.body_state(id);
    let settled = finite && st.is_finite() && st.speed_m_s() < 0.35;

    // Interior clearance: rebuild the collider shape at the settled pose is
    // unnecessary — the shape is rigid, so query the freshly built shape.
    let clearance = {
        let building = fixtures::hollow_building(vid(2));
        let grid = OccupancyGrid::from_volume(&building).unwrap().unwrap();
        let built = build_collider(&grid, fixtures::CELL_M, rep);
        interior_clearance(built.collider.shape(), dims, fixtures::CELL_M) as f64
    };

    (step.summary_us(), clearance, settled)
}

fn interior_clearance(
    shape: &dyn rapier3d::parry::shape::Shape,
    dims: [u32; 3],
    cell_m: f32,
) -> f32 {
    use rapier3d::parry::math::{Pose, Vector};
    use rapier3d::parry::query::Ray;
    let centre = Vector::new(
        dims[0] as f32 * cell_m * 0.5,
        dims[1] as f32 * cell_m * 0.5,
        dims[2] as f32 * cell_m * 0.5,
    );
    let ray = Ray::new(centre, Vector::new(1.0, 0.0, 0.0));
    shape
        .cast_ray(&Pose::IDENTITY, &ray, 100.0, true)
        .unwrap_or(0.0)
}

fn rebuild_cost(rep: Representation, iters: u32) -> PercentileSummary {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let building = fixtures::hollow_building(vid(1));
    let grid = OccupancyGrid::from_volume(&building).unwrap().unwrap();
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: rep,
        grid: grid.clone(),
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 5.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    let mut samples = DurationSamples::new();
    for _ in 0..iters {
        let d = world.rebuild_collider(id, &grid, rep);
        samples.push(d);
        world.step();
    }
    samples.summary_us()
}

fn mass_agreement(rep: Representation) -> (f64, f64, f64) {
    let building = fixtures::hollow_building(vid(1));
    let grid = OccupancyGrid::from_volume(&building).unwrap().unwrap();
    let analytic =
        analytic_mass_properties(&grid, fixtures::CELL_M as f64, fixtures::stone_density);

    let mut world = PhysicsWorld::new(PhysicsConfig::default());
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: false },
        representation: rep,
        grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });
    let (mass, com, inertia) = world.derived_mass_properties(id);

    let mass_rel = ((analytic.mass_kg - mass as f64) / analytic.mass_kg).abs();
    let com_abs = (0..3)
        .map(|i| (analytic.com_m[i] - com[i] as f64).abs())
        .fold(0.0_f64, f64::max);

    // The hollow building is axis-aligned and mirror-symmetric, so its analytic
    // inertia tensor is diagonal and its diagonal *is* the principal inertia.
    // Rapier may return the three principal values in a different axis order, so
    // compare the sorted triples.
    let mut a_diag = analytic.principal_diagonal();
    let mut d_diag = [inertia[0] as f64, inertia[1] as f64, inertia[2] as f64];
    a_diag.sort_by(|x, y| x.partial_cmp(y).unwrap());
    d_diag.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let scale = a_diag.iter().cloned().fold(1e-9_f64, f64::max);
    let inertia_rel = (0..3)
        .map(|i| (a_diag[i] - d_diag[i]).abs() / scale)
        .fold(0.0_f64, f64::max);
    (mass_rel, com_abs, inertia_rel)
}

/// Fires a CCD pellet at a 0.5 m thin wall at rising speeds and returns the
/// highest speed (m/s) at which the pellet was still stopped on the near side.
fn projectile_threshold(rep: Representation, top_speed: f32) -> f64 {
    let ladder = [
        20.0_f32,
        40.0,
        60.0,
        90.0,
        130.0,
        180.0,
        top_speed.max(180.0),
    ];
    let mut best = 0.0_f64;
    for &speed in &ladder {
        if projectile_stops(rep, speed) {
            best = speed as f64;
        } else {
            break;
        }
    }
    best
}

fn projectile_stops(rep: Representation, speed: f32) -> bool {
    let mut world = PhysicsWorld::new(PhysicsConfig::default());

    let wall = fixtures::thin_wall(vid(1), 2);
    let wall_grid = OccupancyGrid::from_volume(&wall).unwrap().unwrap();
    world.add_body(BodySpec {
        kind: BodyKind::Fixed,
        representation: rep,
        grid: wall_grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [4.0, 0.0, 0.0],
        linvel_m_s: [0.0; 3],
    });

    let pellet = fixtures::debris_pieces(50, 1, 1).pop().unwrap().1;
    let pellet_grid = OccupancyGrid::from_volume(&pellet).unwrap().unwrap();
    let id = world.add_body(BodySpec {
        kind: BodyKind::Dynamic { ccd: true },
        representation: rep,
        grid: pellet_grid,
        cell_m: fixtures::CELL_M,
        density_kg_m3: fixtures::STONE_DENSITY,
        translation_m: [0.0, 3.0, 3.0],
        linvel_m_s: [speed, 0.0, 0.0],
    });

    // Only step long enough for the pellet to have crossed the wall many times
    // over if it were going to tunnel; keep it short so gravity stays irrelevant.
    for _ in 0..30 {
        world.step();
    }
    let x = world.body_state(id).translation_m[0];
    // Wall front face is at x = 4.0; a stopped pellet rests near x = 3.75.
    x.is_finite() && x < 4.3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_feasibility_run_behaves() {
        let report = run_feasibility(FeasibilityParams::small());
        for r in [&report.native, &report.compound] {
            assert!(r.settle_finite, "{}: non-finite state", r.representation);
            assert!(
                r.settle_max_speed < 1.0,
                "{}: debris still moving at {} m/s",
                r.representation,
                r.settle_max_speed
            );
            assert!(
                r.settle_sleep_fraction >= 0.9,
                "{}: only {:.0}% of debris asleep",
                r.representation,
                r.settle_sleep_fraction * 100.0
            );
            assert!(
                r.building_settled,
                "{}: hollow building did not come to rest",
                r.representation
            );
            assert!(
                r.building_interior_clearance_m > 3.0 * fixtures::CELL_M as f64,
                "{}: interior clearance {} collapsed",
                r.representation,
                r.building_interior_clearance_m
            );
            assert!(
                r.mass_rel_err < 0.02,
                "{}: mass error {:.3}",
                r.representation,
                r.mass_rel_err
            );
            assert!(
                r.com_abs_err_m < 0.05,
                "{}: COM error {} m",
                r.representation,
                r.com_abs_err_m
            );
            assert!(
                r.inertia_rel_err < 0.15,
                "{}: principal-inertia error {:.3}",
                r.representation,
                r.inertia_rel_err
            );
            assert!(r.rebuild.p50_us > 0.0);

            // The reported 64-brick build cost is the complete occupancy →
            // collider work, so it must be at least the wrap-only component it
            // contains (ENG-40: no wrap-only figure under the build name).
            assert!(
                r.multibrick_build.p50_us >= r.multibrick_wrap.p50_us,
                "{}: complete build p50 {:.3} us < wrap-only p50 {:.3} us",
                r.representation,
                r.multibrick_build.p50_us,
                r.multibrick_wrap.p50_us
            );
            assert!(r.multibrick_build.p50_us > 0.0);

            // Sleep/wake acceptance: settled asleep, woke on the in-place
            // collider rebuild *and* on a blast impulse, moved and re-collided,
            // then settled back to sleep — all observed from the solver, not
            // inferred from a sleeping count.
            let sw = &r.sleep_wake;
            assert!(
                sw.slept,
                "{}: body never settled to sleep",
                r.representation
            );
            assert!(
                sw.woke_on_rebuild,
                "{}: an in-place collider rebuild did not wake the sleeping body",
                r.representation
            );
            assert!(
                sw.reslept_after_rebuild,
                "{}: body did not settle back to sleep after the rebuild",
                r.representation
            );
            assert!(
                sw.woke_on_impulse,
                "{}: a blast impulse did not wake the sleeping body",
                r.representation
            );
            assert!(
                sw.travel_m > 0.1,
                "{}: woken body barely moved ({:.3} m)",
                r.representation,
                sw.travel_m
            );
            assert!(
                sw.recontact,
                "{}: woken body never left the floor and re-collided with it",
                r.representation
            );
            assert!(
                sw.reslept,
                "{}: body did not settle back to sleep after the impulse",
                r.representation
            );
            assert!(
                sw.stable,
                "{}: sleep/wake scenario went unstable (non-finite or deep penetration)",
                r.representation
            );
            assert!(
                sw.ok(),
                "{}: sleep/wake scenario did not pass",
                r.representation
            );
        }
        // The merged compound is the correctness baseline: its interior must be
        // at least as open as the native shape's, and it must stop a fast CCD
        // projectile hitting a 0.5 m wall.
        assert!(
            report.compound.building_interior_clearance_m
                >= report.native.building_interior_clearance_m - 0.05
        );
        assert!(
            report.compound.projectile_max_stop_m_s >= 60.0,
            "compound tunnelled at {} m/s",
            report.compound.projectile_max_stop_m_s
        );

        // Fragmentation probe: the voxel shape stays one primitive; the greedy
        // compound degenerates to one box per isolated cell — the bound that
        // drives the coarse-fracture fallback in docs/collision-decision.md.
        assert_eq!(report.native.worst_case_primitives, 1);
        assert_eq!(
            report.compound.worst_case_primitives as u64,
            report.compound.worst_case_solid_cells
        );
    }
}
