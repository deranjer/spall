//! Bears: the shared land walk, and hunting deer when hungry (ENG-238).
//!
//! A hungry bear that notices a deer stalks it slowly, unseen, then charges at
//! a sprint it can hold for a few seconds. Reaching the deer kills it through
//! the engine's creature kill, which leaves the deer as a terrain carcass. The
//! bear then eats that carcass a cell at a time, through single-cell server
//! edits, until it is gone or the bear is full, and is hungry again a few
//! minutes later. Bears ignore players. Deer see a bear as a threat unless it
//! is stalking; a charging bear counts as a sprinting one.

use std::collections::HashMap;

use spall_core::{GlobalCell, MaterialId};
use spall_server::creature_service::CreatureAction;
use spall_sim::{CreatureKill, Simulation};
use spall_voxel::Sample;

use super::{
    AnimalDirector, Cx, Family, Gait, LOCOMOTION_EAT, LOCOMOTION_MOVE, LOCOMOTION_POUNCE,
    LOCOMOTION_RUN, LOCOMOTION_STALK, MOVE_DT_S, SPECIES, Step, Walker, land, wrap_angle,
};

/// A hungry bear notices a deer this close, horizontally.
const NOTICE_M: f64 = 40.0;
/// A stalk is given up once the deer is this far away.
const LOSE_M: f64 = 55.0;
/// Stalking pace: slow enough to stay unseen.
const STALK_SPEED_M_S: f64 = 0.7;
/// Longest stalk (60 s).
const STALK_TICKS: u64 = 60 * 60;
/// A stalking bear charges from this close. The deer bolts once it sees the
/// charge, so the bear has this lead to close at its speed advantage.
const CHARGE_M: f64 = 14.0;
/// Charging sprint: faster than a fleeing deer (7.5 m/s), but held only for
/// [`CHARGE_TICKS`], so a chase from [`CHARGE_M`] is won only when the deer
/// is slow to turn or meets an obstacle.
const CHARGE_SPEED_M_S: f64 = 10.0;
const CHARGE_TICKS: u64 = 60 * 5;
/// Rest after a charge that missed (20 s).
const TIRED_TICKS: u64 = 60 * 20;
/// Root-to-root distance at which a charge reaches the deer.
const KILL_REACH_M: f64 = 2.0;
/// Time spent on the killing swipe before eating (0.75 s).
const POUNCE_TICKS: u64 = 45;
/// How long a fresh carcass may take to commit (10 s); it is placed as one
/// queued edit per cell.
const SETTLE_TICKS: u64 = 60 * 10;
/// Ticks between bites (0.5 s).
const BITE_TICKS: u64 = 30;
/// Bites that fill a bear: a 24 s meal.
const FULL_BITES: u32 = 48;
/// A bear bites carcass cells whose centre is this close to its root.
const EAT_REACH_M: f64 = 1.6;
/// A bear blocked short of a carcass cell still bites it from this close.
const STRETCH_M: f64 = 2.6;
/// A fed bear is hungry again this long after its meal (4 min).
const HUNGER_TICKS: u64 = 60 * 60 * 4;

/// Bear-only states.
#[derive(Debug, Clone, Copy)]
pub(super) enum BearGait {
    /// Creep toward the deer `prey`, unseen, until close enough to charge.
    Stalk { prey: u64, until: u64 },
    /// Sprint at `prey` until reaching it or `until`.
    Charge { prey: u64, until: u64 },
    /// The killing swipe, then eat whatever carcass the kill left.
    Pounce { until: u64 },
    /// Eat the carcass, a bite every [`BITE_TICKS`].
    Eat { next_bite: u64 },
}

/// What a bear keeps between gaits.
#[derive(Debug, Default)]
pub(super) struct Bear {
    /// Hungry from this tick on; a new bear is hungry at once.
    hungry_at: u64,
    /// The carcass being eaten, ground up; bites take from the top.
    carcass: Vec<(GlobalCell, MaterialId)>,
    /// Bites taken this meal.
    bites: u32,
    /// Until this tick the carcass may still be committing.
    settle_until: u64,
}

/// The clip a bear plays for a locomotion tag of its own. The bear asset has
/// no `stalk` or `pounce` clip: it stalks at a slow `walk` and kills with a
/// paw `swipe`.
pub(super) fn clip(tag: u8) -> Option<&'static str> {
    match tag {
        LOCOMOTION_RUN => Some("run"),
        LOCOMOTION_STALK => Some("walk"),
        LOCOMOTION_POUNCE => Some("swipe"),
        LOCOMOTION_EAT => Some("eat"),
        _ => None,
    }
}

/// Bears as threats to deer: position and whether charging. A stalking bear
/// is not seen.
pub(super) fn threats(dir: &AnimalDirector) -> impl Iterator<Item = ([f64; 3], bool)> + '_ {
    dir.walkers
        .values()
        .filter(|walker| matches!(SPECIES[walker.species].family, Family::Bear))
        .filter_map(|walker| match walker.gait {
            Gait::Bear(BearGait::Stalk { .. }) => None,
            Gait::Bear(BearGait::Charge { .. }) => Some((walker.at, true)),
            _ => Some((walker.at, false)),
        })
}

/// A kill's outcome: the bear eats the carcass it left.
pub(super) fn on_killed(bears: &mut HashMap<u64, Bear>, bear: u64, kill: CreatureKill, tick: u64) {
    let bear = bears.entry(bear).or_default();
    bear.carcass = kill.carcass;
    bear.bites = 0;
    bear.settle_until = tick + SETTLE_TICKS;
}

/// One update of a bear.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let calm = match walker.gait {
        Gait::Rest { until } => tick >= until,
        Gait::Walk { .. } => true,
        _ => false,
    };
    if calm
        && dir.bears.entry(cx.id).or_default().hungry_at <= tick
        && let Some(prey) = nearest_deer(dir, cx.here)
    {
        walker.stuck = 0;
        walker.gait = Gait::Bear(BearGait::Stalk {
            prey,
            until: tick + STALK_TICKS,
        });
    }
    match walker.gait {
        Gait::Bear(BearGait::Stalk { prey, until }) => stalk(walker, cx, prey, until),
        Gait::Bear(BearGait::Charge { prey, until }) => charge(walker, cx, prey, until, actions),
        Gait::Bear(BearGait::Pounce { until }) => {
            if tick >= until {
                walker.gait = if dir.bears.get(&cx.id).is_some_and(|b| !b.carcass.is_empty()) {
                    Gait::Bear(BearGait::Eat { next_bite: tick })
                } else {
                    Gait::Rest { until: tick + 120 }
                };
            }
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_POUNCE;
            step
        }
        Gait::Bear(BearGait::Eat { next_bite }) => eat(dir, walker, cx, next_bite, actions),
        _ => land::step(dir, walker, cx, land::wander, |_| true),
    }
}

/// The nearest deer a bear at `here` notices.
fn nearest_deer(dir: &AnimalDirector, here: [f64; 3]) -> Option<u64> {
    dir.walkers
        .iter()
        .filter(|(_, walker)| matches!(SPECIES[walker.species].family, Family::Deer))
        .map(|(&id, walker)| (id, horizontal(here, walker.at), walker.at[1]))
        .filter(|&(_, distance, y)| distance < NOTICE_M && (y - here[1]).abs() < 8.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _, _)| id)
}

fn creature_at(sim: &Simulation, id: u64) -> Option<[f64; 3]> {
    sim.creature_poses()
        .into_iter()
        .find(|pose| pose.creature_id == id)
        .map(|pose| pose.translation_m)
}

fn horizontal(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

/// A step toward `target` at `speed_m_s`, turning at most `max_turn` and
/// stopping `short_m` before it.
fn toward(
    walker: &Walker,
    here: [f64; 3],
    target: [f64; 3],
    speed_m_s: f64,
    max_turn: f64,
    short_m: f64,
    tag: u8,
) -> Step {
    let (dx, dz) = (target[0] - here[0], target[2] - here[2]);
    let mut step = Step::still(walker, here);
    step.yaw = walker.yaw + wrap_angle(f64::atan2(-dz, dx) - walker.yaw).clamp(-max_turn, max_turn);
    let reach = (speed_m_s * MOVE_DT_S).min((dx.hypot(dz) - short_m).max(0.0));
    step.desired = [reach * step.yaw.cos(), -reach * step.yaw.sin()];
    step.tag = tag;
    step
}

fn stalk(walker: &mut Walker, cx: &Cx, prey: u64, until: u64) -> Step {
    let tick = cx.tick;
    let Some(at) = creature_at(cx.sim, prey) else {
        walker.gait = Gait::Rest { until: tick + 120 };
        return Step::still(walker, cx.here);
    };
    let distance = horizontal(cx.here, at);
    if distance > LOSE_M || tick >= until || walker.stuck >= 6 {
        walker.stuck = 0;
        walker.gait = Gait::Rest { until: tick + 120 };
        return Step::still(walker, cx.here);
    }
    if distance <= CHARGE_M {
        walker.gait = Gait::Bear(BearGait::Charge {
            prey,
            until: tick + CHARGE_TICKS,
        });
        return toward(
            walker,
            cx.here,
            at,
            CHARGE_SPEED_M_S,
            1.2,
            0.0,
            LOCOMOTION_RUN,
        );
    }
    toward(
        walker,
        cx.here,
        at,
        STALK_SPEED_M_S,
        0.35,
        0.0,
        LOCOMOTION_STALK,
    )
}

fn charge(
    walker: &mut Walker,
    cx: &Cx,
    prey: u64,
    until: u64,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let Some(at) = creature_at(cx.sim, prey) else {
        walker.gait = Gait::Rest { until: tick + 120 };
        return Step::still(walker, cx.here);
    };
    if horizontal(cx.here, at) <= KILL_REACH_M && (at[1] - cx.here[1]).abs() < 2.0 {
        actions.push(CreatureAction::Kill {
            creature_id: cx.id,
            prey,
        });
        walker.gait = Gait::Bear(BearGait::Pounce {
            until: tick + POUNCE_TICKS,
        });
        let mut step = toward(walker, cx.here, at, 0.0, 1.2, 0.0, LOCOMOTION_POUNCE);
        step.desired = [0.0; 2];
        return step;
    }
    if tick >= until {
        walker.gait = Gait::Rest {
            until: tick + TIRED_TICKS,
        };
        return Step::still(walker, cx.here);
    }
    toward(
        walker,
        cx.here,
        at,
        CHARGE_SPEED_M_S,
        1.2,
        0.0,
        LOCOMOTION_RUN,
    )
}

fn eat(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    next_bite: u64,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let walk_speed = dir.species[walker.species].walk_speed_m_s;
    let bear = dir.bears.entry(cx.id).or_default();
    // The carcass commits a cell at a time, ground up: wait for the top cell
    // a while, then skip cells that are gone (cut by a player, or refused).
    while let Some(&(cell, material)) = bear.carcass.last() {
        let held = cx.sim.world().terrain().volume.sample(cell);
        if matches!(held, Ok(Sample::Filled(m)) if m == material) {
            break;
        }
        if tick < bear.settle_until && !cx.sim.is_idle() {
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_EAT;
            return step;
        }
        bear.carcass.pop();
    }
    let next = bear.carcass.last().map(|&(cell, _)| cell);
    let Some(cell) = next.filter(|_| bear.bites < FULL_BITES) else {
        // Full, or nothing left: done eating.
        bear.hungry_at = tick + HUNGER_TICKS;
        bear.carcass.clear();
        bear.bites = 0;
        walker.gait = Gait::Rest { until: tick + 120 };
        return Step::still(walker, cx.here);
    };
    let centre = [
        (cell.x as f64 + 0.5) * 0.25,
        (cell.y as f64 + 0.5) * 0.25,
        (cell.z as f64 + 0.5) * 0.25,
    ];
    let distance = horizontal(cx.here, centre);
    // Blocked short of the cell (by the carcass itself, say): bite from here
    // if the cell is close, else leave it.
    let blocked = walker.stuck >= 2;
    if blocked && distance > STRETCH_M {
        walker.stuck = 0;
        bear.carcass.pop();
        return Step::still(walker, cx.here);
    }
    if distance > EAT_REACH_M && !blocked {
        // Step up to the carcass first.
        return toward(
            walker,
            cx.here,
            centre,
            walk_speed,
            0.35,
            EAT_REACH_M - 0.2,
            LOCOMOTION_MOVE,
        );
    }
    let mut step = toward(walker, cx.here, centre, 0.0, 0.6, 0.0, LOCOMOTION_EAT);
    if tick >= next_bite {
        walker.stuck = 0;
        actions.push(CreatureAction::EditCell {
            creature_id: cx.id,
            cell,
            place: None,
        });
        bear.carcass.pop();
        bear.bites += 1;
        walker.gait = Gait::Bear(BearGait::Eat {
            next_bite: tick + BITE_TICKS,
        });
    }
    step.desired = [0.0; 2];
    step
}
