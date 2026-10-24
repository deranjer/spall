//! Fish (trout): born only in live water deep enough for the whole body, and
//! wandering inside it. Water is read from the authoritative column water's
//! published frame (lock-free, the same quantized levels clients see), so a
//! trout always judges the water as it is now: one whose water drains sinks
//! to the bed and lies still until the water comes back.

use spall_fluid::columns::{REGION_EDGE, region_of};
use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;
use spall_sim::column_water::{ColumnWaterFrame, FRAME_DEPTH_SCALE};

use super::{
    AnimalDirector, Cx, GROUND_CLEARANCE_M, Gait, LOCOMOTION_MOVE, MOVE_DT_S, Placement, Step,
    Walker, wrap_angle,
};
use crate::animal_habitat::{
    AnimalHabitat, CandidateEnvelope, CurrentWaterQuery, ValidationLimits, WaterSpan,
    validate_candidate,
};

/// Fish look for water this many times farther from the anchor than land
/// animals: lakes are rarer than ground.
const SEARCH_SCALE: f64 = 6.0;
/// Water a newborn trout keeps below its body, metres.
const BIRTH_DEPTH_BELOW_M: f64 = 0.25;
/// Cruising pace, metres per second.
const SWIM_M_S: f64 = 0.6;
/// How fast a stranded trout drops to the bed, metres per second.
const SINK_M_S: f64 = 2.0;
/// Largest heading change per update, radians.
const MAX_TURN_RAD: f64 = 0.5;
const CELL_M: f64 = 0.25;
/// A hovering trout swims off from a bear standing in the water this close,
/// horizontally.
const SCATTER_M: f64 = 2.0;

/// Fish-only states.
#[derive(Debug, Clone, Copy)]
pub(super) enum FishGait {
    /// Hang in the water until `until`.
    Hover { until: u64 },
    /// Swim straight toward root `target` until there, blocked, or `until`.
    /// `asked_from` is where it stood when it last asked to move.
    Swim {
        target: [f64; 3],
        until: u64,
        asked_from: Option<[f64; 3]>,
    },
}

/// The published water frame as a habitat water query.
struct FrameWater<'a>(&'a ColumnWaterFrame);

impl CurrentWaterQuery for FrameWater<'_> {
    fn visit_spans_in_column(
        &self,
        x: i64,
        z: i64,
        max_spans: usize,
        visit: &mut dyn FnMut(WaterSpan),
    ) -> bool {
        let Some(region) = self.0.regions.get(&region_of(x, z)) else {
            return true;
        };
        let column = (x.rem_euclid(REGION_EDGE) + REGION_EDGE * z.rem_euclid(REGION_EDGE)) as u16;
        let start = region.spans.partition_point(|s| s.column < column);
        let spans = region.spans[start..]
            .iter()
            .take_while(|s| s.column == column);
        if spans.clone().count() > max_spans {
            return false;
        }
        for span in spans {
            visit(WaterSpan {
                floor_y: i64::from(span.floor_y),
                depth_cells: f64::from(span.depth_q) / FRAME_DEPTH_SCALE,
            });
        }
        true
    }
}

impl AnimalDirector {
    /// Whether the body of a fish of `species` with its root at `root` is all
    /// in open water, with at least `below_m` of water under it.
    fn fish_fits(
        &self,
        sim: &Simulation,
        water: &ColumnWaterFrame,
        species: usize,
        root: [f64; 3],
        below_m: f64,
    ) -> bool {
        let size = self.envelope_m[species];
        let bottom = root[1] + f64::from(self.walk_feet[species]);
        validate_candidate(
            &sim.world().terrain().volume,
            Some(&FrameWater(water)),
            AnimalHabitat::Aquatic,
            CandidateEnvelope {
                center_m: glam::DVec3::new(root[0], bottom + size.y * 0.5, root[2]),
                size_m: size,
            },
            ValidationLimits {
                aquatic_min_depth_m: below_m,
                ..ValidationLimits::default()
            },
        )
        .is_ok()
    }

    /// A root height in the water span of column (`x`, `z`) nearest `near_y`
    /// that leaves `below_m` under the body, if one is deep enough.
    fn fish_root_y(
        &self,
        water: &ColumnWaterFrame,
        species: usize,
        x_m: f64,
        z_m: f64,
        near_y: f64,
        below_m: f64,
    ) -> Option<f64> {
        let (x, z) = ((x_m / CELL_M).floor() as i64, (z_m / CELL_M).floor() as i64);
        let height = self.envelope_m[species].y;
        // Body bottom is the root plus the (negative) feet offset.
        let feet = f64::from(self.walk_feet[species]);
        let near_bottom = near_y + feet;
        // (bottom height, distance from `near_bottom`) of the closest fit.
        let mut best: Option<(f64, f64)> = None;
        FrameWater(water).visit_spans_in_column(x, z, 8, &mut |span| {
            let floor = span.floor_y as f64 * CELL_M;
            // A centimetre inside each limit, clear of rounding at the edges.
            let lo = floor + below_m + 0.01;
            let hi = floor + span.depth_cells * CELL_M - height - 0.01;
            if hi >= lo {
                let bottom = near_bottom.clamp(lo, hi);
                let gap = (bottom - near_bottom).abs();
                if best.is_none_or(|(_, g)| gap < g) {
                    best = Some((bottom, gap));
                }
            }
        });
        best.map(|(bottom, _)| bottom - feet)
    }
}

/// Where to place a fish of `species`: a random column of deep enough water
/// within reach of an anchor. When there is no such water, or fish have used
/// half the shared attempt budget, the species is given up (counted as all
/// placed until the world resets) so it does not starve the species after it.
pub(super) fn find_placement(
    dir: &mut AnimalDirector,
    sim: &Simulation,
    species: usize,
) -> Option<Placement> {
    let give_up = |dir: &mut AnimalDirector, why: &str| {
        tracing::info!(
            species = dir.species[species].key,
            placed = dir.spawned[species],
            why,
            "no more fish placed"
        );
        dir.spawned[species] = dir.targets[species];
    };
    if dir.attempts > dir.max_attempts / 2 {
        give_up(dir, "attempt budget");
        return None;
    }
    let Some(water) = sim.column_water().map(|w| w.frame()) else {
        give_up(dir, "no water in this world");
        return None;
    };
    let which = (dir.next() as usize) % dir.anchors.len();
    let anchor = dir.anchors[which];
    let reach = dir.radius_m.1 * SEARCH_SCALE;
    let need_cells = (dir.envelope_m[species].y + BIRTH_DEPTH_BELOW_M) / CELL_M;
    let region_m = REGION_EDGE as f64 * CELL_M;
    // Every deep enough column in reach: (x, z, floor) in metres.
    let columns = || {
        water
            .regions
            .iter()
            .filter(move |(key, _)| {
                let (x0, z0) = (f64::from(key.0) * region_m, f64::from(key.1) * region_m);
                let dx = anchor[0] - anchor[0].clamp(x0, x0 + region_m);
                let dz = anchor[2] - anchor[2].clamp(z0, z0 + region_m);
                dx.hypot(dz) <= reach
            })
            .flat_map(move |(key, region)| {
                let (x0, z0) = (f64::from(key.0) * region_m, f64::from(key.1) * region_m);
                region.spans.iter().filter_map(move |span| {
                    let x = x0 + (f64::from(span.column % REGION_EDGE as u16) + 0.5) * CELL_M;
                    let z = z0 + (f64::from(span.column / REGION_EDGE as u16) + 0.5) * CELL_M;
                    (f64::from(span.depth_q) / FRAME_DEPTH_SCALE >= need_cells
                        && (x - anchor[0]).hypot(z - anchor[2]) <= reach)
                        .then(|| (x, z, f64::from(span.floor_y) * CELL_M))
                })
            })
    };
    let count = columns().count();
    if count == 0 {
        give_up(dir, "no deep water in reach");
        return None;
    }
    let pick = columns().nth((dir.next() % count as u64) as usize);
    let Some((x, z, floor)) = pick else {
        give_up(dir, "no deep water in reach");
        return None;
    };
    let near = floor + dir.unit() * 4.0;
    let y = dir.fish_root_y(&water, species, x, z, near, BIRTH_DEPTH_BELOW_M)?;
    let root = [x, y, z];
    dir.fish_fits(sim, &water, species, root, BIRTH_DEPTH_BELOW_M)
        .then(|| Placement {
            species,
            root_m: glam::DVec3::from_array(root),
            yaw_rad: dir.unit() * std::f64::consts::TAU,
        })
}

/// The first gait and home of a fish placed with its root at `root`.
pub(super) fn start(dir: &mut AnimalDirector, tick: u64, root: [f64; 3]) -> (Gait, [f64; 3]) {
    (hover(dir, tick), root)
}

fn hover(dir: &mut AnimalDirector, tick: u64) -> Gait {
    Gait::Fish(FishGait::Hover {
        until: tick + 60 + dir.next() % 240,
    })
}

/// One update of a fish.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    _actions: &mut Vec<CreatureAction>,
) -> Step {
    let (here, tick, species) = (cx.here, cx.tick, walker.species);
    // Even holding still a fish "flies" (to where it is): the walk the motion
    // loop sends for a stop would drop it toward the bed like a land animal.
    let mut step = Step::still(walker, here);
    step.fly = Some(false);
    let Some(water) = cx.sim.column_water().map(|w| w.frame()) else {
        return strand(dir, walker, cx, step);
    };
    if !dir.fish_fits(cx.sim, &water, species, here, 0.0) {
        return strand(dir, walker, cx, step);
    }
    let Gait::Fish(gait) = walker.gait else {
        walker.gait = hover(dir, tick);
        return step;
    };
    match gait {
        FishGait::Hover { until } => {
            // A bear standing in the water close by scares a hovering trout
            // off, away from it.
            let bear = super::bears::wading(dir)
                .find(|at| (at[0] - here[0]).hypot(at[2] - here[2]) < SCATTER_M);
            if tick >= until || bear.is_some() {
                walker.gait = match wander(dir, cx.sim, &water, species, here, bear) {
                    Some(target) => Gait::Fish(FishGait::Swim {
                        target,
                        until: tick + 60 * 20,
                        asked_from: None,
                    }),
                    None => hover(dir, tick),
                };
            }
        }
        FishGait::Swim {
            target,
            until,
            asked_from,
        } => {
            let d = [
                target[0] - here[0],
                target[1] - here[1],
                target[2] - here[2],
            ];
            let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            // The last move was refused (something solid in the way).
            let blocked = asked_from
                .is_some_and(|from| (0..3).map(|i| (from[i] - here[i]).abs()).sum::<f64>() < 1e-6);
            if distance < 0.1 || tick >= until || blocked {
                walker.gait = hover(dir, tick);
                return step;
            }
            let reach = (SWIM_M_S * MOVE_DT_S).min(distance);
            let next = [
                here[0] + d[0] / distance * reach,
                here[1] + d[1] / distance * reach,
                here[2] + d[2] / distance * reach,
            ];
            // The water may have changed since the target was chosen: never
            // swim a step that would leave it.
            if !dir.fish_fits(cx.sim, &water, species, next, 0.0) {
                walker.gait = hover(dir, tick);
                return step;
            }
            if d[0].hypot(d[2]) > 0.05 {
                let want = f64::atan2(-d[2], d[0]);
                step.yaw =
                    walker.yaw + wrap_angle(want - walker.yaw).clamp(-MAX_TURN_RAD, MAX_TURN_RAD);
            }
            step.tag = LOCOMOTION_MOVE;
            step.fly_target = next;
            step.fly = Some(false);
            walker.gait = Gait::Fish(FishGait::Swim {
                target,
                until,
                asked_from: Some(here),
            });
        }
    }
    step
}

/// A wander target 1.5 to 6 m away whose whole body is in water, if one of a
/// few tries finds one. Fleeing a bear at `away`, only targets farther from
/// it than here count, and the farthest is taken.
fn wander(
    dir: &mut AnimalDirector,
    sim: &Simulation,
    water: &ColumnWaterFrame,
    species: usize,
    here: [f64; 3],
    away: Option<[f64; 3]>,
) -> Option<[f64; 3]> {
    let Some(bear) = away else {
        return wander_once(dir, sim, water, species, here);
    };
    let from_bear = |p: &[f64; 3]| (p[0] - bear[0]).hypot(p[2] - bear[2]);
    (0..4)
        .filter_map(|_| wander_once(dir, sim, water, species, here))
        .filter(|target| from_bear(target) > from_bear(&here))
        .max_by(|a, b| from_bear(a).total_cmp(&from_bear(b)))
}

fn wander_once(
    dir: &mut AnimalDirector,
    sim: &Simulation,
    water: &ColumnWaterFrame,
    species: usize,
    here: [f64; 3],
) -> Option<[f64; 3]> {
    for _ in 0..4 {
        let angle = dir.unit() * std::f64::consts::TAU;
        let distance = 1.5 + dir.unit() * 4.5;
        let (x, z) = (
            here[0] + distance * angle.cos(),
            here[2] - distance * angle.sin(),
        );
        let near = here[1] + (dir.unit() - 0.5) * 2.0;
        let Some(y) = dir.fish_root_y(water, species, x, z, near, 0.0) else {
            continue;
        };
        let target = [x, y, z];
        if dir.fish_fits(sim, water, species, target, 0.0) {
            return Some(target);
        }
    }
    None
}

/// A fish out of water: it drops straight to the bed (flapping), then lies
/// still until the water comes back. It never moves sideways or up.
fn strand(dir: &mut AnimalDirector, walker: &mut Walker, cx: &Cx, mut step: Step) -> Step {
    let here = cx.here;
    walker.gait = hover(dir, cx.tick);
    let (x, z) = (
        (here[0] / CELL_M).floor() as i64,
        (here[2] / CELL_M).floor() as i64,
    );
    let Some(ground) = AnimalDirector::ground_y_m(cx.sim, x, z) else {
        return step;
    };
    let rest_y = ground - f64::from(dir.walk_feet[walker.species]) + GROUND_CLEARANCE_M;
    if here[1] - rest_y > 0.01 {
        step.tag = LOCOMOTION_MOVE;
        step.fly_target = [
            here[0],
            (here[1] - SINK_M_S * MOVE_DT_S).max(rest_y),
            here[2],
        ];
        step.fly = Some(false);
    }
    step
}
