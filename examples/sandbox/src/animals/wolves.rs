//! Wolves: pack animals (ENG-243).
//!
//! Wolves are born in packs of up to [`PACK_SIZE`] around a den. The first
//! living member leads: it walks the shared land walk, and the others follow in
//! a loose file behind it, trotting or running to catch up. The leader makes
//! the pack's decisions; the others read them.
//!
//! A hungry pack hunts a deer it notices by encircling it: each wolf takes a
//! slot evenly spaced on a ring round the deer and works its way round to it,
//! unseen (deer do not see encircling wolves, like a stalking bear). Once the
//! ring is closed, or after a while, every wolf runs in at once, faster than a
//! deer flees, so a deer bolting from the nearest wolf runs at another. The
//! first to reach it kills it through the engine's creature kill, and the pack
//! eats the carcass together, a cell per bite, through single-cell server
//! edits.
//!
//! With no deer about, a hungry pack now and then (more often at night) hunts a
//! player the same way and bites through the engine's player damage, each
//! wolf darting back out after a bite. It gives up after a few bites.
//!
//! Wolves avoid bears: a bear near any member sends the whole pack away and
//! ends its hunt. They fear fire: the sandbox has no campfire yet, so the
//! torch's emissive lamp voxel counts as fire. A wolf near one backs away, and
//! a pack gives up prey that stands near one.
//!
//! At night a calm pack sometimes howls together, each wolf joining a moment
//! after the one before (`howl` clip).

use std::collections::HashMap;

use spall_core::{BrickCoord, EntityId, GlobalCell, LocalCell, MaterialId};
use spall_server::creature_service::{CreatureAction, CreatureActionOutcome};
use spall_sim::{CreatureKill, PlayerDamage, Simulation};
use spall_voxel::Sample;
use spall_voxel::volume::BrickState;

use super::bears::{creature_at, horizontal, toward};
use super::players::ThreatSource;
use super::{
    AnimalDirector, Cx, Family, Gait, LOCOMOTION_CHASE, LOCOMOTION_EAT, LOCOMOTION_HOWL,
    LOCOMOTION_MOVE, LOCOMOTION_POUNCE, LOCOMOTION_RUN, LOCOMOTION_SHY, LOCOMOTION_STALK,
    MOVE_DT_S, SPECIES, Step, Walker, land, wrap_angle,
};

/// Wolves per pack at birth.
pub(super) const PACK_SIZE: usize = 4;
/// Pack members are born this far from the first member, metres.
const DEN_SPREAD_M: (f64, f64) = (2.0, 7.0);
/// Where followers walk relative to the leader: (behind, to the side), metres.
const FOLLOW_SLOTS: [[f64; 2]; 5] = [
    [-2.5, 1.5],
    [-2.5, -1.5],
    [-4.5, 0.0],
    [-5.5, 2.5],
    [-5.5, -2.5],
];
/// A follower this far from its slot runs to catch up.
const CATCH_UP_M: f64 = 15.0;
const CATCH_UP_SPEED_M_S: f64 = 6.0;

/// A new pack is hungry this long after birth (30 s).
const FIRST_HUNGER_TICKS: u64 = 60 * 30;
/// A hungry pack notices a deer this close to its leader.
const NOTICE_M: f64 = 60.0;
/// A hunt is given up once the prey is this far from the leader.
const LOSE_M: f64 = 90.0;
/// Encircling ring round a deer: outside the 14 m a deer bolts at.
const DEER_RING_M: f64 = 18.0;
/// A deer sees an encircling wolf this close, inside its 14 m bolt distance,
/// so a wolf that cuts in too close sets it running.
const SPOTTED_M: f64 = 12.0;
/// Encircling ring round a player.
const PLAYER_RING_M: f64 = 12.0;
/// A wolf this close to its ring slot is in place.
const SLOT_M: f64 = 4.0;
/// A wolf this far from its slot runs there; closer it creeps.
const ENCIRCLE_RUN_M: f64 = 10.0;
const ENCIRCLE_RUN_SPEED_M_S: f64 = 6.0;
const ENCIRCLE_CREEP_SPEED_M_S: f64 = 2.0;
/// The ring closes after this long even if a wolf is not in place (30 s).
const ENCIRCLE_TICKS: u64 = 60 * 30;
/// Closing at a run: faster than a fleeing deer (7.5 m/s) and a sprinting
/// player (5.8 m/s).
const CLOSE_SPEED_M_S: f64 = 8.5;
const CLOSE_PLAYER_SPEED_M_S: f64 = 7.0;
/// Longest run-in at a deer (8 s) and at a player (20 s).
const CLOSE_TICKS: u64 = 60 * 6;
const CLOSE_PLAYER_TICKS: u64 = 60 * 20;
/// Root-to-root distance at which a wolf lunges at a deer...
const KILL_REACH_M: f64 = 1.8;
/// ...and the chance the lunge misses. Each wolf lunges once per hunt.
const MISS_CHANCE: f64 = 0.75;
/// Root-to-feet distance at which a wolf bites a player.
const BITE_REACH_M: f64 = 1.6;
/// The bite clip, then a dart back out at a run.
const BITE_TICKS: u64 = 30;
const BACK_OFF_TICKS: u64 = 90;
/// Bites a pack lands on a player before it gives up.
const PLAYER_BITES: u32 = 3;
/// Rest after a hunt that failed (40 s).
const TIRED_TICKS: u64 = 60 * 40;

/// A hungry pack with no deer decides this often whether to hunt a player...
const PLAYER_CHOICE_TICKS: u64 = 60 * 20;
/// ...with this chance by day and by night...
const PLAYER_HUNT_CHANCE: f64 = 0.2;
const PLAYER_HUNT_NIGHT_CHANCE: f64 = 0.5;
/// ...for a player this close to its leader...
const PLAYER_NOTICE_M: f64 = 40.0;
/// ...and leaves players alone this long after a player hunt (3 min).
const PLAYER_REST_TICKS: u64 = 60 * 60 * 3;

/// A bear this close to any member sends the pack away.
const BEAR_AVOID_M: f64 = 22.0;
/// A wolf this close to a fire backs away from it.
const FIRE_FEAR_M: f64 = 8.0;
/// Prey this close to a fire is given up.
const FIRE_SAFE_M: f64 = 12.0;
/// Fires are looked for this far round the leader, horizontally...
const FIRE_SCAN_M: f64 = 30.0;
/// ...and this far above and below it...
const FIRE_SCAN_UP_M: f64 = 8.0;
/// ...this often (2 s).
const FIRE_SCAN_TICKS: u64 = 120;
/// Pace when backing away from a bear or a fire.
const AVOID_SPEED_M_S: f64 = 6.0;
/// The pack keeps away from a bear this long after last seeing it near.
const AVOID_TICKS: u64 = 60 * 5;

/// How long a fresh carcass may take to commit (10 s).
const SETTLE_TICKS: u64 = 60 * 10;
/// Ticks between one wolf's bites (0.75 s).
const EAT_BITE_TICKS: u64 = 45;
/// Bites that fill a pack.
const FULL_BITES: u32 = 40;
/// A wolf bites carcass cells whose centre is this close to its root.
const EAT_REACH_M: f64 = 1.4;
/// A wolf blocked short of a carcass cell still bites it from this close.
const STRETCH_M: f64 = 2.4;
/// A fed pack is hungry again this long after its meal (5 min).
const HUNGER_TICKS: u64 = 60 * 60 * 5;

/// One wolf's howl (5 s), each member starting this long after the one
/// before.
const HOWL_TICKS: u64 = 60 * 5;
const HOWL_STAGGER_TICKS: u64 = 50;
/// A pack howls again after this long, ticks (90 s to 4 min).
const HOWL_REST_TICKS: (u64, u64) = (60 * 90, 60 * 240);

// A wolf in place on the deer ring stays outside a deer's 14 m bolt distance,
// and a closing pack outruns a fleeing deer (7.5 m/s) and a sprinting player
// (5.8 m/s).
const _: () =
    assert!(DEER_RING_M - SLOT_M >= 14.0 && CLOSE_SPEED_M_S > 7.5 && CLOSE_PLAYER_SPEED_M_S > 5.8);

/// What a wolf is doing, beyond the shared calm walk the leader uses.
#[derive(Debug, Clone, Copy)]
pub(super) enum WolfGait {
    /// Keeping its place behind the leader.
    Follow,
    /// Working round to its slot on the ring about the prey.
    Encircle,
    /// Running in at the prey.
    Close,
    /// Biting a player until `until`.
    Bite {
        until: u64,
    },
    /// Darting back out from a bitten player until `until`.
    BackOff {
        until: u64,
    },
    /// Missed its lunge at a deer: trots after the chase without lunging
    /// again this hunt.
    Spent,
    /// Eating the pack's carcass, biting next at `next_bite`.
    Eat {
        next_bite: u64,
    },
    Howl,
    /// Backing away from a bear or a fire.
    Avoid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Prey {
    Deer(u64),
    Player(EntityId),
}

#[derive(Debug, Clone, Copy)]
struct Hunt {
    prey: Prey,
    /// Angle (radians, x toward z) of the first ring slot about the prey.
    base: f64,
    /// Running in, after encircling.
    closing: bool,
    /// The current phase ends (encircling) or fails (closing) at this tick.
    until: u64,
    /// Bites landed on a player prey.
    bites: u32,
}

/// One pack's shared state; the leader decides it.
#[derive(Debug, Default)]
pub(super) struct Pack {
    /// Living members, leader first.
    members: Vec<u64>,
    hungry_at: u64,
    hunt: Option<Hunt>,
    /// The carcass being eaten, ground up; bites take from the top.
    carcass: Vec<(GlobalCell, MaterialId)>,
    bites: u32,
    /// Until this tick a kill's carcass may still be arriving or committing.
    settle_until: u64,
    /// Fires (lamp cells) near the leader at the last scan.
    fires: Vec<[f64; 3]>,
    next_fire_scan: u64,
    /// Keep away from this point until the tick.
    danger: Option<([f64; 3], u64)>,
    /// The tick the current howl started.
    howl_at: Option<u64>,
    next_howl: u64,
    next_player_choice: u64,
    players_left_until: u64,
}

/// Every pack, and which pack each wolf is in.
#[derive(Debug, Default)]
pub(super) struct Packs {
    packs: Vec<Pack>,
    of: HashMap<u64, usize>,
}

impl Packs {
    pub(super) fn clear(&mut self) {
        self.packs.clear();
        self.of.clear();
    }

    /// Forgets wolves that are gone; the next living member leads.
    pub(super) fn retain(&mut self, alive: impl Fn(u64) -> bool) {
        self.of.retain(|&id, _| alive(id));
        for pack in &mut self.packs {
            pack.members.retain(|&id| alive(id));
        }
    }
}

/// The clip a wolf plays for a locomotion tag of its own.
pub(super) fn clip(tag: u8) -> Option<&'static str> {
    match tag {
        // Running in at prey and backing off are runs too; their own tags
        // let clients play a growl or an alarm bark.
        LOCOMOTION_RUN | LOCOMOTION_CHASE | LOCOMOTION_SHY => Some("run"),
        LOCOMOTION_STALK => Some("stalk"),
        LOCOMOTION_POUNCE => Some("bite"),
        LOCOMOTION_EAT => Some("eat"),
        LOCOMOTION_HOWL => Some("howl"),
        _ => None,
    }
}

/// Where the next wolf is born: beside the first member of a pack that is
/// still forming, or `None` to start a new pack at an ordinary spot.
pub(super) fn den_spot(dir: &mut AnimalDirector) -> Option<[f64; 2]> {
    let pack = dir.wolves.packs.last()?;
    if pack.members.len() >= PACK_SIZE {
        return None;
    }
    let den = dir.walkers.get(pack.members.first()?)?.home;
    let angle = dir.unit() * std::f64::consts::TAU;
    let distance = DEN_SPREAD_M.0 + dir.unit() * (DEN_SPREAD_M.1 - DEN_SPREAD_M.0);
    Some([
        den[0] + distance * angle.cos(),
        den[2] + distance * angle.sin(),
    ])
}

/// Adds a newborn wolf to the forming pack, or starts a new one.
pub(super) fn join(dir: &mut AnimalDirector, id: u64, tick: u64) {
    let open = dir
        .wolves
        .packs
        .last()
        .is_some_and(|pack| pack.members.len() < PACK_SIZE);
    if !open {
        let next_howl = dir.next() % HOWL_REST_TICKS.0;
        dir.wolves.packs.push(Pack {
            hungry_at: tick + FIRST_HUNGER_TICKS,
            next_howl,
            ..Pack::default()
        });
    }
    let index = dir.wolves.packs.len() - 1;
    dir.wolves.packs[index].members.push(id);
    dir.wolves.of.insert(id, index);
}

/// Wolves as threats to deer. A wolf running in counts as sprinting; an
/// encircling wolf is seen only within [`SPOTTED_M`].
pub(super) fn threats(dir: &AnimalDirector) -> impl Iterator<Item = ThreatSource> + '_ {
    dir.walkers
        .values()
        .filter(|walker| matches!(SPECIES[walker.species].family, Family::Wolf))
        .map(|walker| match walker.gait {
            Gait::Wolf(WolfGait::Encircle) => ThreatSource {
                at: walker.at,
                sprinting: false,
                seen_within_m: SPOTTED_M,
            },
            Gait::Wolf(WolfGait::Close) => ThreatSource::open(walker.at, true),
            _ => ThreatSource::open(walker.at, false),
        })
}

/// A wolf's kill or bite outcome, for its pack.
pub(super) fn on_outcome(packs: &mut Packs, wolf: u64, outcome: CreatureActionOutcome, tick: u64) {
    let Some(&index) = packs.of.get(&wolf) else {
        return;
    };
    let pack = &mut packs.packs[index];
    match outcome {
        CreatureActionOutcome::Killed(CreatureKill { carcass, .. }) => {
            pack.carcass = carcass;
            pack.bites = 0;
            pack.settle_until = tick + SETTLE_TICKS;
        }
        // A player the pack killed is left alone: it respawns elsewhere.
        CreatureActionOutcome::HitPlayer(PlayerDamage::Killed) => {
            pack.hunt = None;
            pack.players_left_until = tick + PLAYER_REST_TICKS;
        }
        _ => {}
    }
}

fn is_night(tick: u64) -> bool {
    spall_core::time_of_day::state_at_tick(tick).sun_height() < 0.0
}

/// The lamp cells (the torch's fire) within `radius_m` of `centre`
/// horizontally and [`FIRE_SCAN_UP_M`] vertically, as cell centres. Lamps
/// exist only where a player placed one, so only edited bricks are read.
pub(super) fn fires_near(sim: &Simulation, centre: [f64; 3], radius_m: f64) -> Vec<[f64; 3]> {
    let volume = &sim.world().terrain().volume;
    let cell = |m: f64| (m / 0.25).floor() as i64;
    let lo = GlobalCell::new(
        cell(centre[0] - radius_m),
        cell(centre[1] - FIRE_SCAN_UP_M),
        cell(centre[2] - radius_m),
    )
    .split()
    .0;
    let hi = GlobalCell::new(
        cell(centre[0] + radius_m),
        cell(centre[1] + FIRE_SCAN_UP_M),
        cell(centre[2] + radius_m),
    )
    .split()
    .0;
    let edge = i64::from(spall_core::BRICK_EDGE);
    let mut fires = Vec::new();
    for z in lo.z..=hi.z {
        for y in lo.y..=hi.y {
            for x in lo.x..=hi.x {
                let coord = BrickCoord { x, y, z };
                if !matches!(
                    volume.brick_state(coord),
                    Ok(BrickState::Resident { edited: true, .. })
                ) {
                    continue;
                }
                let Ok(Some(brick)) = volume.snapshot_brick(coord) else {
                    continue;
                };
                for (index, material) in brick.material_cells().into_iter().enumerate() {
                    if material != crate::game::materials::LAMP {
                        continue;
                    }
                    let Some(local) = LocalCell::from_linear_index(index as u16) else {
                        continue;
                    };
                    let at = [
                        ((x * edge + i64::from(local.x())) as f64 + 0.5) * 0.25,
                        ((y * edge + i64::from(local.y())) as f64 + 0.5) * 0.25,
                        ((z * edge + i64::from(local.z())) as f64 + 0.5) * 0.25,
                    ];
                    if horizontal(at, centre) <= radius_m {
                        fires.push(at);
                    }
                }
            }
        }
    }
    fires
}

/// The nearest of `points` to `here`, horizontally, with its distance.
fn nearest(points: &[[f64; 3]], here: [f64; 3]) -> Option<([f64; 3], f64)> {
    points
        .iter()
        .map(|&at| (at, horizontal(at, here)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

fn player_at(sim: &Simulation, entity: EntityId) -> Option<[f64; 3]> {
    sim.world()
        .players()
        .find(|player| player.entity == entity)
        .map(|player| player.state.position_m)
}

fn prey_at(sim: &Simulation, prey: Prey) -> Option<[f64; 3]> {
    match prey {
        Prey::Deer(id) => creature_at(sim, id),
        Prey::Player(entity) => player_at(sim, entity),
    }
}

/// Whether the pack member at `rank` is howling at `tick` in a howl that
/// started at `start`: each joins [`HOWL_STAGGER_TICKS`] after the one before.
fn howling(start: u64, rank: usize, tick: u64) -> bool {
    let mine = start + rank as u64 * HOWL_STAGGER_TICKS;
    (mine..mine + HOWL_TICKS).contains(&tick)
}

/// The leader's decisions for the pack, once per leader update.
fn lead(dir: &mut AnimalDirector, index: usize, cx: &Cx) {
    let tick = cx.tick;
    let members: Vec<[f64; 3]> = dir.wolves.packs[index]
        .members
        .iter()
        .filter_map(|id| dir.walkers.get(id).map(|walker| walker.at))
        .collect();
    let bear = dir
        .walkers
        .values()
        .filter(|walker| matches!(SPECIES[walker.species].family, Family::Bear))
        .map(|walker| walker.at)
        .find(|&bear| {
            members
                .iter()
                .any(|&at| horizontal(at, bear) < BEAR_AVOID_M)
        });
    let prey = dir.wolves.packs[index]
        .hunt
        .and_then(|hunt| prey_at(cx.sim, hunt.prey));
    {
        let pack = &mut dir.wolves.packs[index];
        if tick >= pack.next_fire_scan {
            pack.next_fire_scan = tick + FIRE_SCAN_TICKS;
            pack.fires = fires_near(cx.sim, cx.here, FIRE_SCAN_M);
            if let Some(at) = prey {
                pack.fires.extend(fires_near(cx.sim, at, FIRE_SAFE_M));
            }
        }
        if let Some(bear) = bear {
            if pack.hunt.take().is_some() {
                tracing::info!(leader = cx.id, "wolf pack gives up a hunt for a bear");
            }
            pack.howl_at = None;
            pack.danger = Some((bear, tick + AVOID_TICKS));
        }
        if pack.danger.is_some_and(|(_, until)| tick >= until) {
            pack.danger = None;
        }
    }
    if let Some(mut hunt) = dir.wolves.packs[index].hunt {
        // Every wolf has lunged at the deer and missed: the chase is lost.
        let all_missed = hunt.closing
            && dir.wolves.packs[index].members.iter().all(|id| {
                dir.walkers.get(id).is_some_and(|wolf| {
                    matches!(
                        wolf.gait,
                        Gait::Wolf(WolfGait::Spent | WolfGait::Bite { .. })
                    )
                })
            });
        // A deer that saw a wolf and bolted is chased at once.
        let bolted = match hunt.prey {
            Prey::Deer(id) => dir
                .walkers
                .get(&id)
                .is_some_and(|deer| matches!(deer.gait, Gait::Flee { .. })),
            Prey::Player(_) => false,
        };
        let pack = &dir.wolves.packs[index];
        let near_fire = |at: [f64; 3]| {
            nearest(&pack.fires, at).is_some_and(|(_, distance)| distance < FIRE_SAFE_M)
        };
        let over = match prey {
            None => true,
            Some(at) => {
                horizontal(at, cx.here) > LOSE_M
                    || near_fire(at)
                    || (hunt.closing && tick >= hunt.until)
                    || all_missed
                    || hunt.bites >= PLAYER_BITES
            }
        };
        let pack = &mut dir.wolves.packs[index];
        if over {
            tracing::info!(leader = cx.id, prey = ?hunt.prey, "wolf pack ends a hunt");
            pack.hunt = None;
            if matches!(hunt.prey, Prey::Player(_)) {
                pack.players_left_until = tick + PLAYER_REST_TICKS;
            }
            if pack.carcass.is_empty() && tick >= pack.settle_until {
                pack.hungry_at = pack.hungry_at.max(tick + TIRED_TICKS);
            }
        } else if !hunt.closing {
            let at = prey.unwrap_or(cx.here);
            let ring = ring_m(hunt.prey);
            let count = members.len();
            let in_place = members.iter().enumerate().all(|(i, &wolf)| {
                let slot = ring_slot(at, hunt.base, i, count, ring);
                horizontal(wolf, slot) < SLOT_M
            });
            if in_place || bolted || tick >= hunt.until {
                tracing::info!(
                    leader = cx.id,
                    prey = ?hunt.prey,
                    in_place,
                    bolted,
                    "wolf pack closes in"
                );
                hunt.closing = true;
                hunt.until = tick
                    + match hunt.prey {
                        Prey::Deer(_) => CLOSE_TICKS,
                        Prey::Player(_) => CLOSE_PLAYER_TICKS,
                    };
                pack.hunt = Some(hunt);
            }
        }
    }
    let pack = &dir.wolves.packs[index];
    let calm = pack.hunt.is_none() && pack.carcass.is_empty() && pack.danger.is_none();
    if calm && tick >= pack.hungry_at && tick >= pack.settle_until {
        choose_prey(dir, index, cx, &members);
    }
    let pack = &mut dir.wolves.packs[index];
    let calm = pack.hunt.is_none() && pack.carcass.is_empty() && pack.danger.is_none();
    match pack.howl_at {
        Some(start) => {
            let end = start + pack.members.len() as u64 * HOWL_STAGGER_TICKS + HOWL_TICKS;
            if !calm || tick >= end {
                pack.howl_at = None;
            }
        }
        None if calm && is_night(tick) && tick >= pack.next_howl => {
            tracing::info!(
                leader = cx.id,
                wolves = pack.members.len(),
                "wolf pack howls"
            );
            pack.howl_at = Some(tick);
            let (lo, hi) = HOWL_REST_TICKS;
            let rest = lo + dir.next() % (hi - lo);
            dir.wolves.packs[index].next_howl = tick + rest;
        }
        None => {}
    }
}

/// Starts a hunt on the nearest deer the leader notices, or now and then on
/// a player when there is none; prey near a fire is left alone.
fn choose_prey(dir: &mut AnimalDirector, index: usize, cx: &Cx, members: &[[f64; 3]]) {
    let tick = cx.tick;
    let deer = dir
        .walkers
        .iter()
        .filter(|(_, walker)| matches!(SPECIES[walker.species].family, Family::Deer))
        .map(|(&id, walker)| (Prey::Deer(id), walker.at))
        .filter(|&(_, at)| horizontal(at, cx.here) < NOTICE_M && (at[1] - cx.here[1]).abs() < 10.0)
        .min_by(|a, b| horizontal(a.1, cx.here).total_cmp(&horizontal(b.1, cx.here)));
    let mut prey = deer.filter(|&(_, at)| fires_near(cx.sim, at, FIRE_SAFE_M).is_empty());
    let pack = &dir.wolves.packs[index];
    if prey.is_none()
        && deer.is_none()
        && tick >= pack.next_player_choice
        && tick >= pack.players_left_until
    {
        dir.wolves.packs[index].next_player_choice = tick + PLAYER_CHOICE_TICKS;
        let chance = if is_night(tick) {
            PLAYER_HUNT_NIGHT_CHANCE
        } else {
            PLAYER_HUNT_CHANCE
        };
        if dir.unit() < chance {
            prey = cx
                .sim
                .world()
                .players()
                .map(|player| (Prey::Player(player.entity), player.state.position_m))
                .filter(|&(_, at)| horizontal(at, cx.here) < PLAYER_NOTICE_M)
                .min_by(|a, b| horizontal(a.1, cx.here).total_cmp(&horizontal(b.1, cx.here)))
                .filter(|&(_, at)| fires_near(cx.sim, at, FIRE_SAFE_M).is_empty());
        }
    }
    let Some((prey, at)) = prey else {
        return;
    };
    // The first slot faces the pack, so the ring forms round the far side.
    let centre = members
        .iter()
        .fold([0.0; 2], |sum, at| [sum[0] + at[0], sum[1] + at[2]]);
    let count = members.len().max(1) as f64;
    let base = f64::atan2(centre[1] / count - at[2], centre[0] / count - at[0]);
    tracing::info!(
        leader = cx.id,
        ?prey,
        wolves = members.len(),
        "wolf pack starts a hunt"
    );
    let pack = &mut dir.wolves.packs[index];
    pack.howl_at = None;
    pack.hunt = Some(Hunt {
        prey,
        base,
        closing: false,
        until: tick + ENCIRCLE_TICKS,
        bites: 0,
    });
}

fn ring_m(prey: Prey) -> f64 {
    match prey {
        Prey::Deer(_) => DEER_RING_M,
        Prey::Player(_) => PLAYER_RING_M,
    }
}

/// Member `i` of `count`'s slot on a ring of `radius_m` about `prey`.
fn ring_slot(prey: [f64; 3], base: f64, i: usize, count: usize, radius_m: f64) -> [f64; 3] {
    let angle = base + i as f64 * std::f64::consts::TAU / count.max(1) as f64;
    [
        prey[0] + radius_m * angle.cos(),
        prey[1],
        prey[2] + radius_m * angle.sin(),
    ]
}

/// One update of a wolf.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let Some(&index) = dir.wolves.of.get(&cx.id) else {
        return land::step(dir, walker, cx, land::wander, |_| true);
    };
    let Some(rank) = dir.wolves.packs[index]
        .members
        .iter()
        .position(|&id| id == cx.id)
    else {
        return Step::still(walker, cx.here);
    };
    if rank == 0 {
        lead(dir, index, cx);
    }
    let pack = &dir.wolves.packs[index];
    // Bears and fire come first.
    let fire = nearest(&pack.fires, cx.here).filter(|&(_, distance)| distance < FIRE_FEAR_M);
    let danger = pack.danger.map(|(at, _)| at).or(fire.map(|(at, _)| at));
    if let Some(from) = danger {
        walker.gait = Gait::Wolf(WolfGait::Avoid);
        return away(walker, cx.here, from);
    }
    if !pack.carcass.is_empty() {
        return eat(dir, index, rank, walker, cx, actions);
    }
    if let Some(hunt) = pack.hunt {
        return hunt_step(dir, index, rank, hunt, walker, cx, actions);
    }
    if let Some(start) = pack.howl_at
        && howling(start, rank, tick)
    {
        walker.gait = Gait::Wolf(WolfGait::Howl);
        let mut step = Step::still(walker, cx.here);
        step.tag = LOCOMOTION_HOWL;
        return step;
    }
    if rank == 0 {
        if !matches!(walker.gait, Gait::Rest { .. } | Gait::Walk { .. }) {
            walker.gait = Gait::Rest { until: tick + 60 };
        }
        land::step(dir, walker, cx, land::wander, |_| true)
    } else {
        walker.gait = Gait::Wolf(WolfGait::Follow);
        follow(dir, index, rank, walker, cx)
    }
}

/// A follower keeps its slot behind the leader.
fn follow(dir: &AnimalDirector, index: usize, rank: usize, walker: &Walker, cx: &Cx) -> Step {
    let leader = dir.wolves.packs[index].members[0];
    let Some(lead) = dir.walkers.get(&leader) else {
        return Step::still(walker, cx.here);
    };
    let [behind, side] = FOLLOW_SLOTS[(rank - 1) % FOLLOW_SLOTS.len()];
    let (forward, across) = (
        [lead.yaw.cos(), -lead.yaw.sin()],
        [lead.yaw.sin(), lead.yaw.cos()],
    );
    let slot = [
        lead.at[0] + forward[0] * behind + across[0] * side,
        cx.here[1],
        lead.at[2] + forward[1] * behind + across[1] * side,
    ];
    let distance = horizontal(slot, cx.here);
    let walk = dir.species[walker.species].walk_speed_m_s;
    if distance < 1.2 {
        // In place: face the way the leader faces.
        let mut step = Step::still(walker, cx.here);
        step.yaw = walker.yaw + wrap_angle(lead.yaw - walker.yaw).clamp(-0.35, 0.35);
        return step;
    }
    let (speed, tag) = if distance > CATCH_UP_M {
        (CATCH_UP_SPEED_M_S, LOCOMOTION_RUN)
    } else if distance > 4.0 {
        (walk * 1.8, LOCOMOTION_MOVE)
    } else {
        (walk * 1.1, LOCOMOTION_MOVE)
    };
    toward(walker, cx.here, slot, speed, 0.6, 0.0, tag)
}

/// Backing away from `from` at a run.
fn away(walker: &Walker, here: [f64; 3], from: [f64; 3]) -> Step {
    let (dx, dz) = (here[0] - from[0], here[2] - from[2]);
    let length = dx.hypot(dz).max(1.0e-6);
    let target = [
        here[0] + dx / length * 10.0,
        here[1],
        here[2] + dz / length * 10.0,
    ];
    toward(
        walker,
        here,
        target,
        AVOID_SPEED_M_S,
        1.2,
        0.0,
        LOCOMOTION_SHY,
    )
}

fn hunt_step(
    dir: &mut AnimalDirector,
    index: usize,
    rank: usize,
    hunt: Hunt,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let Some(at) = prey_at(cx.sim, hunt.prey) else {
        return Step::still(walker, cx.here);
    };
    // A wolf that just bit darts back out before running in again.
    match walker.gait {
        Gait::Wolf(WolfGait::Bite { until }) if tick < until => {
            let mut step = toward(walker, cx.here, at, 0.0, 0.6, 0.0, LOCOMOTION_POUNCE);
            step.desired = [0.0; 2];
            return step;
        }
        // A wolf that missed a deer is out of this chase; after a bite on a
        // player the wolf darts back out first.
        Gait::Wolf(WolfGait::Bite { .. }) if matches!(hunt.prey, Prey::Deer(_)) => {
            walker.gait = Gait::Wolf(WolfGait::Spent);
        }
        Gait::Wolf(WolfGait::Bite { .. }) => {
            walker.gait = Gait::Wolf(WolfGait::BackOff {
                until: tick + BACK_OFF_TICKS,
            });
        }
        _ => {}
    }
    if let Gait::Wolf(WolfGait::BackOff { until }) = walker.gait
        && tick < until
    {
        return away(walker, cx.here, at);
    }
    if let Gait::Wolf(WolfGait::Spent) = walker.gait {
        let walk = dir.species[walker.species].walk_speed_m_s;
        return toward(walker, cx.here, at, walk * 2.0, 0.8, 6.0, LOCOMOTION_MOVE);
    }
    if !hunt.closing {
        walker.gait = Gait::Wolf(WolfGait::Encircle);
        let count = dir.wolves.packs[index].members.len();
        return encircle(walker, cx, at, hunt, rank, count);
    }
    walker.gait = Gait::Wolf(WolfGait::Close);
    let distance = horizontal(at, cx.here);
    let level = (at[1] - cx.here[1]).abs() < 2.0;
    match hunt.prey {
        Prey::Deer(prey) if distance <= KILL_REACH_M && level && dir.unit() < MISS_CHANCE => {
            // The lunge misses: the wolf snaps at air while the deer runs on,
            // and drops out of the chase.
            tracing::info!(wolf = cx.id, deer = prey, "wolf misses a deer");
            walker.gait = Gait::Wolf(WolfGait::Bite {
                until: tick + BITE_TICKS,
            });
            let mut step = toward(walker, cx.here, at, 0.0, 1.2, 0.0, LOCOMOTION_POUNCE);
            step.desired = [0.0; 2];
            step
        }
        Prey::Deer(prey) if distance <= KILL_REACH_M && level => {
            actions.push(CreatureAction::Kill {
                creature_id: cx.id,
                prey,
            });
            tracing::info!(wolf = cx.id, deer = prey, "wolf takes a deer");
            // The carcass arrives through the kill's outcome; until it does
            // (or the settle time runs out) the pack waits.
            let pack = &mut dir.wolves.packs[index];
            pack.hunt = None;
            pack.settle_until = tick + SETTLE_TICKS;
            walker.gait = Gait::Wolf(WolfGait::Eat { next_bite: tick });
            let mut step = toward(walker, cx.here, at, 0.0, 1.2, 0.0, LOCOMOTION_POUNCE);
            step.desired = [0.0; 2];
            step
        }
        Prey::Player(entity) if distance <= BITE_REACH_M && level && hunt.bites < PLAYER_BITES => {
            actions.push(CreatureAction::HitPlayer {
                creature_id: cx.id,
                player: entity,
                damage: crate::game::WOLF_BITE_DAMAGE,
            });
            if let Some(hunt) = &mut dir.wolves.packs[index].hunt {
                hunt.bites += 1;
            }
            walker.gait = Gait::Wolf(WolfGait::Bite {
                until: tick + BITE_TICKS,
            });
            let mut step = toward(walker, cx.here, at, 0.0, 1.2, 0.0, LOCOMOTION_POUNCE);
            step.desired = [0.0; 2];
            step
        }
        Prey::Deer(_) => toward(
            walker,
            cx.here,
            at,
            CLOSE_SPEED_M_S,
            1.2,
            0.0,
            LOCOMOTION_CHASE,
        ),
        Prey::Player(_) => toward(
            walker,
            cx.here,
            at,
            CLOSE_PLAYER_SPEED_M_S,
            1.2,
            0.0,
            LOCOMOTION_CHASE,
        ),
    }
}

/// Works round the ring toward this wolf's slot: along the ring at most as far
/// as its pace allows, and in or out toward the ring's radius, so it never
/// cuts across the prey.
fn encircle(
    walker: &Walker,
    cx: &Cx,
    prey: [f64; 3],
    hunt: Hunt,
    rank: usize,
    count: usize,
) -> Step {
    let ring = ring_m(hunt.prey);
    let slot = ring_slot(prey, hunt.base, rank, count, ring);
    let to_slot = horizontal(slot, cx.here);
    let (speed, tag) = if to_slot > ENCIRCLE_RUN_M {
        (ENCIRCLE_RUN_SPEED_M_S, LOCOMOTION_RUN)
    } else {
        (ENCIRCLE_CREEP_SPEED_M_S, LOCOMOTION_STALK)
    };
    if to_slot < 1.0 {
        // In place: crouch facing the prey.
        let mut step = toward(walker, cx.here, prey, 0.0, 0.6, 0.0, LOCOMOTION_STALK);
        step.desired = [0.0; 2];
        return step;
    }
    let (dx, dz) = (cx.here[0] - prey[0], cx.here[2] - prey[2]);
    let radius = dx.hypot(dz).max(0.5);
    let angle = f64::atan2(dz, dx);
    let slot_angle = f64::atan2(slot[2] - prey[2], slot[0] - prey[0]);
    let reach = speed * MOVE_DT_S * 2.0;
    let turn = wrap_angle(slot_angle - angle).clamp(-reach / radius, reach / radius);
    let next_radius = radius + (ring - radius).clamp(-reach, reach);
    let target = [
        prey[0] + next_radius * (angle + turn).cos(),
        cx.here[1],
        prey[2] + next_radius * (angle + turn).sin(),
    ];
    toward(walker, cx.here, target, speed, 0.8, 0.0, tag)
}

/// Each wolf eats the pack's carcass, standing at its own side of it.
fn eat(
    dir: &mut AnimalDirector,
    index: usize,
    rank: usize,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let tick = cx.tick;
    let walk_speed = dir.species[walker.species].walk_speed_m_s;
    let next_bite = match walker.gait {
        Gait::Wolf(WolfGait::Eat { next_bite }) => next_bite,
        _ => tick + rank as u64 * 10,
    };
    let pack = &mut dir.wolves.packs[index];
    // The carcass commits a cell at a time, ground up: wait for the top cell
    // a while, then skip cells that are gone (cut, refused, or eaten).
    while let Some(&(cell, material)) = pack.carcass.last() {
        let held = cx.sim.world().terrain().volume.sample(cell);
        if matches!(held, Ok(Sample::Filled(m)) if m == material) {
            break;
        }
        if tick < pack.settle_until && !cx.sim.is_idle() {
            walker.gait = Gait::Wolf(WolfGait::Eat { next_bite });
            let mut step = Step::still(walker, cx.here);
            step.tag = LOCOMOTION_EAT;
            return step;
        }
        pack.carcass.pop();
    }
    let next = pack.carcass.last().map(|&(cell, _)| cell);
    let Some(cell) = next.filter(|_| pack.bites < FULL_BITES) else {
        // Full, or nothing left: the pack is done eating.
        tracing::info!(
            wolf = cx.id,
            bites = pack.bites,
            "wolf pack finished eating"
        );
        pack.hungry_at = tick + HUNGER_TICKS;
        pack.carcass.clear();
        pack.bites = 0;
        walker.gait = Gait::Rest { until: tick + 120 };
        return Step::still(walker, cx.here);
    };
    let centre = [
        (cell.x as f64 + 0.5) * 0.25,
        (cell.y as f64 + 0.5) * 0.25,
        (cell.z as f64 + 0.5) * 0.25,
    ];
    let distance = horizontal(cx.here, centre);
    let blocked = walker.stuck >= 2;
    if blocked && distance > STRETCH_M {
        // Cannot get to this cell: stand and wait for the others to eat it.
        walker.stuck = 0;
        walker.gait = Gait::Wolf(WolfGait::Eat { next_bite });
        let mut step = Step::still(walker, cx.here);
        step.tag = LOCOMOTION_EAT;
        return step;
    }
    if distance > EAT_REACH_M && !blocked {
        walker.gait = Gait::Wolf(WolfGait::Eat { next_bite });
        // Each wolf comes at the carcass from its own side.
        let angle = rank as f64 * std::f64::consts::TAU / pack.members.len().max(1) as f64;
        let side = [
            centre[0] + (EAT_REACH_M - 0.4) * angle.cos(),
            centre[1],
            centre[2] + (EAT_REACH_M - 0.4) * angle.sin(),
        ];
        return toward(
            walker,
            cx.here,
            side,
            walk_speed * 1.5,
            0.6,
            0.0,
            LOCOMOTION_MOVE,
        );
    }
    let mut step = toward(walker, cx.here, centre, 0.0, 0.6, 0.0, LOCOMOTION_EAT);
    step.desired = [0.0; 2];
    if tick >= next_bite {
        walker.stuck = 0;
        actions.push(CreatureAction::EditCell {
            creature_id: cx.id,
            cell,
            place: None,
        });
        pack.carcass.pop();
        pack.bites += 1;
        walker.gait = Gait::Wolf(WolfGait::Eat {
            next_bite: tick + EAT_BITE_TICKS,
        });
    } else {
        walker.gait = Gait::Wolf(WolfGait::Eat { next_bite });
    }
    step
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_slots_are_spread_evenly_round_the_prey() {
        let prey = [10.0, 2.0, -4.0];
        let slots: Vec<_> = (0..4)
            .map(|i| ring_slot(prey, 0.3, i, 4, DEER_RING_M))
            .collect();
        for (i, slot) in slots.iter().enumerate() {
            assert!((horizontal(*slot, prey) - DEER_RING_M).abs() < 1.0e-9);
            let next = slots[(i + 1) % 4];
            // Neighbours a quarter turn apart: sqrt(2) radii.
            assert!((horizontal(*slot, next) - DEER_RING_M * 2f64.sqrt()).abs() < 1.0e-9);
        }
    }

    #[test]
    fn packs_howl_only_at_night_each_joining_after_the_last() {
        use spall_core::time_of_day::DAY_CYCLE_TICKS;
        let hour = DAY_CYCLE_TICKS / 24;
        // The server tick starts at 06:00.
        let at = |h: u64| DAY_CYCLE_TICKS * 2 + ((h + 24 - 6) % 24) * hour;
        assert!(is_night(at(0)) && is_night(at(22)));
        assert!(!is_night(at(12)) && !is_night(at(9)));
        let start = at(23);
        assert!(howling(start, 0, start) && !howling(start, 1, start));
        assert!(howling(start, 1, start + HOWL_STAGGER_TICKS));
        // The pack overlaps: the leader still howls when the last joins.
        assert!(howling(start, 0, start + 3 * HOWL_STAGGER_TICKS));
        assert!(!howling(start, 0, start + HOWL_TICKS));
    }

    #[test]
    fn a_deer_sees_an_encircling_wolf_only_close_up() {
        use super::super::players::nearest_threat;
        let wolf = |x: f64| ThreatSource {
            at: [x, 0.0, 0.0],
            sprinting: false,
            seen_within_m: SPOTTED_M,
        };
        let deer = [0.0; 3];
        assert!(nearest_threat(&[wolf(DEER_RING_M - SLOT_M)], deer).is_none());
        let seen = nearest_threat(&[wolf(SPOTTED_M - 1.0)], deer).unwrap();
        assert!(seen.distance_m < 14.0, "close enough to bolt");
        // A farther player is still seen, and the near hidden wolf is not.
        let player = ThreatSource::open([30.0, 0.0, 0.0], false);
        let nearest = nearest_threat(&[wolf(15.0), player], deer).unwrap();
        assert_eq!(nearest.distance_m, 30.0);
    }

    #[test]
    fn packs_hand_the_lead_to_the_next_living_member() {
        let mut packs = Packs::default();
        packs.packs.push(Pack {
            members: vec![4, 7, 9],
            ..Pack::default()
        });
        for id in [4, 7, 9] {
            packs.of.insert(id, 0);
        }
        packs.retain(|id| id != 4);
        assert_eq!(packs.packs[0].members, vec![7, 9]);
        assert!(!packs.of.contains_key(&4));
    }
}
