//! Bears: the shared land walk, and hunting deer when hungry (ENG-238).
//!
//! A hungry bear that notices a deer stalks it slowly, unseen, then charges at
//! a sprint it can hold for a few seconds. Reaching the deer kills it through
//! the engine's creature kill, which leaves the deer as a terrain carcass. The
//! bear then eats that carcass a cell at a time, through single-cell server
//! edits, until it is gone or the bear is full, and is hungry again a few
//! minutes later. Deer see a bear as a threat unless it is stalking; a charging
//! bear counts as a sprinting one.
//!
//! Bears do not like being run at. A player who comes close, or sprints at a
//! bear from further off, makes it rear up and watch; one who keeps coming gets
//! a short bluff charge and a cuffing swipe, then the bear backs off. A bear
//! that has just backed off tolerates a walker nearby for a while. Most bluffs
//! stop out of reach; a player who stays close while the bear rears (or, now
//! and then, any bluffed player) gets a real charge, and a swipe that lands
//! hurts through the engine's player damage (ENG-240). Two hits kill.
//!
//! A hungry bear with no deer to hunt fishes instead (ENG-239): it walks to
//! shallow water (or the bank) beside a trout it knows of, waits there, and
//! swipes at a trout that swims within reach of its front. A catch despawns
//! the trout outright, with no carcass, and the bear eats it briefly; a fish
//! keeps it fed for less time than a deer.

use std::collections::HashMap;

use spall_core::{EntityId, GlobalCell, MaterialId};
use spall_server::creature_service::CreatureAction;
use spall_sim::{CreatureKill, Simulation};
use spall_voxel::Sample;

use super::{
    AnimalDirector, Cx, Family, Gait, LOCOMOTION_EAT, LOCOMOTION_IDLE, LOCOMOTION_MOVE,
    LOCOMOTION_POUNCE, LOCOMOTION_REAR, LOCOMOTION_RUN, LOCOMOTION_STALK, MOVE_DT_S, SPECIES, Step,
    Walker, land, wrap_angle,
};

/// A player this close bothers a bear.
const WARY_M: f64 = 10.0;
/// A sprinting player bothers a bear from this far.
const WARY_SPRINT_M: f64 = 22.0;
/// A player this close bothers even a bear that just backed off.
const CROWDED_M: f64 = 4.0;
/// Horizontal speed above which a player counts as sprinting.
const SPRINT_M_S: f32 = 4.5;
/// Rearing up to look at the player, before deciding.
const REAR_TICKS: u64 = 90;
/// A player still this close (or sprinting inside [`WARY_M`]) once the bear
/// has looked is bluff-charged.
const BLUFF_M: f64 = 7.0;
const BLUFF_SPEED_M_S: f64 = 7.0;
const BLUFF_TICKS: u64 = 120;
/// A bluff stops this far from the player, out of [`HIT_REACH_M`], and cuffs
/// the air in front of it.
const BLUFF_STOP_M: f64 = 3.5;
/// A player still inside this once the bear has looked gets a real attack.
const PRESS_M: f64 = 4.0;
/// Chance any other bluff is a real attack.
const ATTACK_CHANCE: f64 = 0.15;
/// A real attack closes to this distance before swiping.
const ATTACK_STOP_M: f64 = 1.8;
const CUFF_TICKS: u64 = 45;
/// The paw lands this long into the swipe, on a player within
/// [`HIT_REACH_M`] (horizontally, root to feet) at that moment.
const HIT_AT_TICKS: u64 = 18;
const HIT_REACH_M: f64 = 2.5;
/// Backing off, at a walk, away from the player.
const RETREAT_SPEED_M_S: f64 = 1.8;
const RETREAT_TICKS: u64 = 60 * 6;
/// After backing off, only a player inside [`CROWDED_M`] bothers it.
const TOLERANT_TICKS: u64 = 60 * 15;

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

/// A hungry bear with no deer goes fishing for a trout this close.
const FISH_NOTICE_M: f64 = 40.0;
/// Deepest water a bear wades into, metres; it stops short of deeper water.
const WADE_MAX_M: f64 = 0.8;
/// Longest a bear goes to the water and waits there for a catch (90 s).
const FISH_TICKS: u64 = 60 * 90;
/// The paw's reach point, this far ahead of the root.
const FRONT_M: f64 = 0.8;
/// A trout this close to the reach point, horizontally, can be swiped.
const SWIPE_REACH_M: f64 = 1.5;
/// Chance a swipe catches the trout.
const CATCH_CHANCE: f64 = 0.5;
/// Time spent eating a caught fish (5 s).
const FISH_MEAL_TICKS: u64 = 60 * 5;
/// A bear fed on a fish is hungry again this long after (90 s).
const FISH_HUNGER_TICKS: u64 = 60 * 90;

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
    /// Walk to the wading spot `spot` (x, z), then stand there (`standing`)
    /// and watch for trout, until `until`.
    Wade {
        spot: [f64; 2],
        standing: bool,
        until: u64,
    },
    /// A swipe at a trout until `until`; `caught` says whether it landed.
    /// A miss goes back to waiting until `give_up`.
    Swipe {
        caught: bool,
        until: u64,
        give_up: u64,
    },
    /// Eat a caught fish until `until`.
    EatFish { until: u64 },
    /// Rear up on the hind legs facing a player who came too close.
    Rear { until: u64 },
    /// A short charge at that player: a real `attack` closes to swiping
    /// distance, a bluff stops short of it.
    Bluff { until: u64, attack: bool },
    /// A swipe in front of the player; the paw lands at `hit_at` (then
    /// `None`), hurting a player still in reach.
    Cuff { until: u64, hit_at: Option<u64> },
    /// Walk away from the player.
    Retreat { until: u64 },
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
    /// A trout just caught, for the director to despawn.
    caught: Option<u64>,
    /// Until this tick only a crowding player bothers the bear.
    tolerant_until: u64,
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
        LOCOMOTION_REAR => Some("stand"),
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
            Gait::Bear(BearGait::Charge { .. } | BearGait::Bluff { .. }) => Some((walker.at, true)),
            _ => Some((walker.at, false)),
        })
}

/// Where bears stand fishing in the water, for trout to scatter from.
pub(super) fn wading(dir: &AnimalDirector) -> impl Iterator<Item = [f64; 3]> + '_ {
    dir.walkers
        .values()
        .filter(|walker| {
            matches!(
                walker.gait,
                Gait::Bear(BearGait::Wade { standing: true, .. } | BearGait::Swipe { .. })
            )
        })
        .map(|walker| walker.at)
}

/// The trout bears caught since the last call. They leave the world without
/// a carcass, through the director's despawn hook.
pub(super) fn caught_fish(bears: &mut HashMap<u64, Bear>) -> Vec<u64> {
    bears
        .values_mut()
        .filter_map(|bear| bear.caught.take())
        .collect()
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
    let player = nearest_player(cx.sim, cx.here);
    if let Some((_, distance, sprinting, _)) = player {
        let tolerant = dir
            .bears
            .get(&cx.id)
            .is_some_and(|b| tick < b.tolerant_until);
        let bothered = distance < CROWDED_M
            || (!tolerant && (distance < WARY_M || (sprinting && distance < WARY_SPRINT_M)));
        // A bear mid-kill finishes it; one already reacting carries on.
        let busy = matches!(
            walker.gait,
            Gait::Bear(
                BearGait::Charge { .. }
                    | BearGait::Pounce { .. }
                    | BearGait::Rear { .. }
                    | BearGait::Bluff { .. }
                    | BearGait::Cuff { .. }
                    | BearGait::Retreat { .. }
            )
        );
        if bothered && !busy {
            walker.stuck = 0;
            walker.gait = Gait::Bear(BearGait::Rear {
                until: tick + REAR_TICKS,
            });
        }
    }
    if let Some(step) = react(dir, walker, cx, player, actions) {
        return step;
    }
    let calm = match walker.gait {
        Gait::Rest { until } => tick >= until,
        Gait::Walk { .. } => true,
        _ => false,
    };
    // A bear chased off its meal goes back to it once it has calmed down.
    if calm && dir.bears.get(&cx.id).is_some_and(|b| !b.carcass.is_empty()) {
        walker.gait = Gait::Bear(BearGait::Eat { next_bite: tick });
    } else if calm && dir.bears.entry(cx.id).or_default().hungry_at <= tick {
        if let Some(prey) = nearest_deer(dir, cx.here) {
            walker.stuck = 0;
            walker.gait = Gait::Bear(BearGait::Stalk {
                prey,
                until: tick + STALK_TICKS,
            });
        } else if let Some(spot) = nearest_trout(dir, cx.here, FISH_NOTICE_M)
            .and_then(|(_, trout)| wading_spot(cx.sim, cx.here, trout))
        {
            walker.stuck = 0;
            walker.gait = Gait::Bear(BearGait::Wade {
                spot,
                standing: false,
                until: tick + FISH_TICKS,
            });
        }
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
        Gait::Bear(BearGait::Wade {
            spot,
            standing,
            until,
        }) => wade(dir, walker, cx, spot, standing, until),
        Gait::Bear(BearGait::Swipe {
            caught,
            until,
            give_up,
        }) => {
            if tick >= until {
                walker.gait = if caught {
                    Gait::Bear(BearGait::EatFish {
                        until: tick + FISH_MEAL_TICKS,
                    })
                } else {
                    // Missed: back to watching from where it stands.
                    Gait::Bear(BearGait::Wade {
                        spot: [cx.here[0], cx.here[2]],
                        standing: true,
                        until: give_up,
                    })
                };
            }
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_POUNCE;
            step
        }
        Gait::Bear(BearGait::EatFish { until }) => {
            if tick >= until {
                dir.bears.entry(cx.id).or_default().hungry_at = tick + FISH_HUNGER_TICKS;
                walker.gait = Gait::Rest { until: tick + 120 };
                return Step::still(walker, cx.here);
            }
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_EAT;
            step
        }
        _ => land::step(dir, walker, cx, land::wander, |_| true),
    }
}

/// The nearest player to `here`: feet, horizontal distance, sprinting, id.
type NearPlayer = ([f64; 3], f64, bool, EntityId);

fn nearest_player(sim: &Simulation, here: [f64; 3]) -> Option<NearPlayer> {
    sim.world()
        .players()
        .map(|player| {
            let at = player.state.position_m;
            let [vx, _, vz] = player.state.velocity_m_s;
            let sprinting = vx.hypot(vz) > SPRINT_M_S;
            (at, horizontal(here, at), sprinting, player.entity)
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

/// The bear's reaction to a player, while it is reacting: rear, bluff (or
/// attack), cuff, back off. `None` once it is doing anything else.
fn react(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    player: Option<NearPlayer>,
    actions: &mut Vec<CreatureAction>,
) -> Option<Step> {
    let tick = cx.tick;
    let Gait::Bear(gait) = walker.gait else {
        return None;
    };
    let retreat = |walker: &mut Walker| {
        walker.gait = Gait::Bear(BearGait::Retreat {
            until: tick + RETREAT_TICKS,
        });
    };
    Some(match gait {
        BearGait::Rear { until } => {
            if tick >= until {
                match player {
                    Some((_, distance, sprinting, _))
                        if distance < BLUFF_M || (sprinting && distance < WARY_M) =>
                    {
                        let attack = distance < PRESS_M || dir.unit() < ATTACK_CHANCE;
                        walker.gait = Gait::Bear(BearGait::Bluff {
                            until: tick + BLUFF_TICKS,
                            attack,
                        });
                    }
                    _ => retreat(walker),
                }
            }
            let mut step = match player {
                Some((at, ..)) => toward(walker, cx.here, at, 0.0, 0.6, 0.0, LOCOMOTION_REAR),
                None => Step::still(walker, cx.here),
            };
            step.tag = LOCOMOTION_REAR;
            step
        }
        BearGait::Bluff { until, attack } => {
            let stop = if attack { ATTACK_STOP_M } else { BLUFF_STOP_M };
            match player {
                Some((at, distance, ..)) if distance > stop && tick < until => toward(
                    walker,
                    cx.here,
                    at,
                    BLUFF_SPEED_M_S,
                    1.2,
                    stop,
                    LOCOMOTION_RUN,
                ),
                _ => {
                    walker.gait = Gait::Bear(BearGait::Cuff {
                        until: tick + CUFF_TICKS,
                        hit_at: Some(tick + HIT_AT_TICKS),
                    });
                    let mut step = Step::still(walker, cx.here);
                    step.tag = LOCOMOTION_POUNCE;
                    step
                }
            }
        }
        BearGait::Cuff { until, hit_at } => {
            if hit_at.is_some_and(|at| tick >= at) {
                walker.gait = Gait::Bear(BearGait::Cuff {
                    until,
                    hit_at: None,
                });
                if let Some((at, distance, _, entity)) = player
                    && distance <= HIT_REACH_M
                    && (at[1] - cx.here[1]).abs() < 2.0
                {
                    actions.push(CreatureAction::HitPlayer {
                        creature_id: cx.id,
                        player: entity,
                        damage: crate::game::BEAR_SWIPE_DAMAGE,
                    });
                }
            }
            if tick >= until {
                retreat(walker);
            }
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_POUNCE;
            step
        }
        BearGait::Retreat { until } => {
            if tick >= until {
                dir.bears.entry(cx.id).or_default().tolerant_until = tick + TOLERANT_TICKS;
                walker.gait = Gait::Rest { until: tick + 120 };
                return Some(Step::still(walker, cx.here));
            }
            // Directly away from the player, or on along its heading.
            let away = match player {
                Some((at, ..)) => {
                    let (dx, dz) = (cx.here[0] - at[0], cx.here[2] - at[2]);
                    let length = dx.hypot(dz).max(1.0e-6);
                    [dx / length, dz / length]
                }
                None => [walker.yaw.cos(), -walker.yaw.sin()],
            };
            let target = [
                cx.here[0] + away[0] * 10.0,
                cx.here[1],
                cx.here[2] + away[1] * 10.0,
            ];
            toward(
                walker,
                cx.here,
                target,
                RETREAT_SPEED_M_S,
                0.8,
                0.0,
                LOCOMOTION_MOVE,
            )
        }
        _ => return None,
    })
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

/// The nearest trout within `within_m` of `here`, horizontally, that no bear
/// has caught already: (id, root).
fn nearest_trout(dir: &AnimalDirector, here: [f64; 3], within_m: f64) -> Option<(u64, [f64; 3])> {
    dir.walkers
        .iter()
        .filter(|(_, walker)| matches!(SPECIES[walker.species].family, Family::Fish))
        .filter(|(id, _)| dir.bears.values().all(|bear| bear.caught != Some(**id)))
        .map(|(&id, walker)| (id, walker.at, horizontal(here, walker.at)))
        .filter(|&(_, _, distance)| distance < within_m)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(id, at, _)| (id, at))
}

/// Water depth over the ground of the column under (`x`, `z`), metres: zero
/// on dry ground, `None` where the ground is unknown.
fn water_depth_m(sim: &Simulation, x: f64, z: f64) -> Option<f64> {
    let ground =
        AnimalDirector::ground_y_m(sim, (x / 0.25).floor() as i64, (z / 0.25).floor() as i64)?;
    let surface = sim
        .column_water()
        .and_then(|water| water.water_sample_at_m(x, ground + 0.05, z))
        .map_or(ground, |sample| sample.surface_y);
    Some((surface - ground).max(0.0))
}

/// Where a bear at `here` stands to fish for the trout at `trout`: of the
/// columns on the nearest ring (1 to 4 m) around the trout with ground near
/// its level and water no deeper than [`WADE_MAX_M`] (dry bank counts), the
/// one closest to the bear.
fn wading_spot(sim: &Simulation, here: [f64; 3], trout: [f64; 3]) -> Option<[f64; 2]> {
    let from_bear = |p: &[f64; 2]| (p[0] - here[0]).hypot(p[1] - here[2]);
    [1.0, 1.5, 2.0, 2.5, 3.0, 4.0]
        .into_iter()
        .find_map(|radius| {
            (0..16)
                .map(|i| f64::from(i) * std::f64::consts::TAU / 16.0)
                .map(|angle| {
                    [
                        trout[0] + radius * angle.cos(),
                        trout[2] + radius * angle.sin(),
                    ]
                })
                .filter(|&[x, z]| {
                    let cell = ((x / 0.25).floor() as i64, (z / 0.25).floor() as i64);
                    AnimalDirector::ground_y_m(sim, cell.0, cell.1)
                        .is_some_and(|ground| (ground - trout[1]).abs() < 3.0)
                        && water_depth_m(sim, x, z).is_some_and(|depth| depth <= WADE_MAX_M)
                })
                .min_by(|a, b| from_bear(a).total_cmp(&from_bear(b)))
        })
}

/// Fishing: walk to `spot`, never into water deeper than [`WADE_MAX_M`],
/// then stand facing the nearest trout and swipe at one in reach of the paw.
fn wade(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    spot: [f64; 2],
    standing: bool,
    until: u64,
) -> Step {
    let tick = cx.tick;
    if tick >= until {
        walker.gait = Gait::Rest { until: tick + 600 };
        return Step::still(walker, cx.here);
    }
    if !standing {
        let (dx, dz) = (spot[0] - cx.here[0], spot[1] - cx.here[2]);
        let distance = dx.hypot(dz);
        let too_deep = distance > 0.3 && {
            let ahead = 0.6 / distance;
            water_depth_m(cx.sim, cx.here[0] + dx * ahead, cx.here[2] + dz * ahead)
                .is_none_or(|depth| depth > WADE_MAX_M)
        };
        if distance > 0.3 && !too_deep && walker.stuck < 4 {
            let speed = dir.species[walker.species].walk_speed_m_s;
            let target = [spot[0], cx.here[1], spot[1]];
            return toward(walker, cx.here, target, speed, 0.35, 0.0, LOCOMOTION_MOVE);
        }
        // There, or as close as shallow water and the ground allow.
        walker.stuck = 0;
        walker.gait = Gait::Bear(BearGait::Wade {
            spot,
            standing: true,
            until,
        });
    }
    let Some((trout, at)) = nearest_trout(dir, cx.here, 8.0) else {
        return Step::still(walker, cx.here);
    };
    let front = [
        cx.here[0] + FRONT_M * walker.yaw.cos(),
        cx.here[2] - FRONT_M * walker.yaw.sin(),
    ];
    let in_reach = (at[0] - front[0]).hypot(at[2] - front[1]) <= SWIPE_REACH_M
        && (at[1] - cx.here[1]).abs() < 2.0;
    let mut step = toward(walker, cx.here, at, 0.0, 0.35, 0.0, LOCOMOTION_IDLE);
    step.desired = [0.0; 2];
    if in_reach {
        let caught = dir.unit() < CATCH_CHANCE;
        if caught {
            dir.bears.entry(cx.id).or_default().caught = Some(trout);
            tracing::info!(bear = cx.id, trout, "bear caught a trout");
        }
        walker.gait = Gait::Bear(BearGait::Swipe {
            caught,
            until: tick + POUNCE_TICKS,
            give_up: until,
        });
        step.tag = LOCOMOTION_POUNCE;
    }
    step
}

pub(super) fn creature_at(sim: &Simulation, id: u64) -> Option<[f64; 3]> {
    sim.creature_poses()
        .into_iter()
        .find(|pose| pose.creature_id == id)
        .map(|pose| pose.translation_m)
}

pub(super) fn horizontal(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[2] - b[2])
}

/// A step toward `target` at `speed_m_s`, turning at most `max_turn` and
/// stopping `short_m` before it.
pub(super) fn toward(
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
