//! Fish (trout). None are placed yet: placing them needs the live
//! column-water query (ENG-225), so `start` and `step` are never reached
//! today.

use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;

use super::{AnimalDirector, Cx, Gait, Placement, Step, Walker, land};

/// Where to place a fish of `species`: nowhere yet.
pub(super) fn find_placement(
    _dir: &mut AnimalDirector,
    _sim: &Simulation,
    _species: usize,
) -> Option<Placement> {
    None
}

/// The first gait and home of a fish placed with its root at `root`.
pub(super) fn start(dir: &mut AnimalDirector, tick: u64, root: [f64; 3]) -> (Gait, [f64; 3]) {
    (land::start(dir, tick), root)
}

/// One update of a fish: it holds still.
pub(super) fn step(
    _dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    _actions: &mut Vec<CreatureAction>,
) -> Step {
    Step::still(walker, cx.here)
}
