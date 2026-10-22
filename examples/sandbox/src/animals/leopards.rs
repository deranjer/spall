//! Snow leopards: upland animals. They are born on high or steep ground and
//! walk the shared land walk, choosing targets and detours no lower than a
//! little under where they were born.

use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;

use super::{AnimalDirector, Cx, Step, Walker, land};

/// An upland species is born on ground at least this far above the spawn
/// anchor it was placed around, or on steep ground.
const UPLAND_RISE_M: f64 = 8.0;
/// Ground is steep where it rises or falls this much within 1 m (about 39°).
const STEEP_RISE_M: f64 = 0.8;
/// An upland species wanders to ground no lower than this under where it was
/// born.
const UPLAND_SLACK_M: f64 = 6.0;
/// An upland species searches this many times farther than the spawn ring
/// for its mountain.
pub(super) const UPLAND_SEARCH_SCALE: f64 = 3.0;

/// One update of a snow leopard.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    _actions: &mut Vec<CreatureAction>,
) -> Step {
    let (sim, home_y) = (cx.sim, walker.home[1]);
    // An upland animal does not detour off its mountain.
    land::step(
        dir,
        walker,
        cx,
        |dir, here| upland_target(dir, sim, here, home_y),
        |detour| upland_ok(sim, detour, home_y),
    )
}

/// Whether an upland animal may be born on a column (top face `ground`
/// metres) placed around an anchor at height `anchor_y`.
pub(super) fn upland_birthplace(
    sim: &Simulation,
    x_cell: i64,
    z_cell: i64,
    ground: f64,
    anchor_y: f64,
) -> bool {
    ground >= anchor_y + UPLAND_RISE_M || steep(sim, x_cell, z_cell, ground)
}

/// Whether the ground at a column (top face `ground` metres) rises or
/// falls [`STEEP_RISE_M`] within 1 m along x or z.
fn steep(sim: &Simulation, x_cell: i64, z_cell: i64, ground: f64) -> bool {
    [(4, 0), (-4, 0), (0, 4), (0, -4)]
        .into_iter()
        .any(|(dx, dz)| {
            AnimalDirector::ground_y_m(sim, x_cell + dx, z_cell + dz)
                .is_some_and(|y| (y - ground).abs() >= STEEP_RISE_M)
        })
}

/// A wander target for an upland animal at `here`, born at root height
/// `home_y`: the first of a few random ones on ground high enough, else
/// where it stands (it rests and tries again later).
fn upland_target(
    dir: &mut AnimalDirector,
    sim: &Simulation,
    here: [f64; 3],
    home_y: f64,
) -> [f64; 2] {
    for _ in 0..8 {
        let angle = dir.unit() * std::f64::consts::TAU;
        let distance = 4.0 + dir.unit() * 14.0;
        let target = [
            here[0] + distance * angle.cos(),
            here[2] - distance * angle.sin(),
        ];
        if upland_ok(sim, target, home_y) {
            return target;
        }
    }
    [here[0], here[2]]
}

/// Whether `target` (x, z) is on ground high enough for an upland animal
/// born at root height `home_y`. Height alone: steep ground below would
/// include the foot of a cliff, and the walk there goes over its edge.
fn upland_ok(sim: &Simulation, target: [f64; 2], home_y: f64) -> bool {
    let (x, z) = (
        (target[0] / 0.25).floor() as i64,
        (target[1] / 0.25).floor() as i64,
    );
    AnimalDirector::ground_y_m(sim, x, z).is_some_and(|ground| ground >= home_y - UPLAND_SLACK_M)
}
