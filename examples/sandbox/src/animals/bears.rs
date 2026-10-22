//! Bears: today they only walk the shared land walk and ignore players.

use spall_server::creature_service::CreatureAction;

use super::{AnimalDirector, Cx, Step, Walker, land};

/// One update of a bear.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    _actions: &mut Vec<CreatureAction>,
) -> Step {
    land::step(dir, walker, cx, land::wander, |_| true)
}
