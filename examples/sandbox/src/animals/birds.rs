//! Birds: circling flight, and for perching species landing, hopping and
//! pecking on the ground, then climbing back to the circle. A player who comes
//! close spooks a landing or grounded bird back up.
//!
//! Crows also nest (ENG-236): each picks the top of a tree under its circle and
//! builds a small cup of wood cells on it over several trips, one single-cell
//! server edit at a time, ground up so every cell rests on the tree or an
//! earlier nest cell. At night it roosts in the cup until morning. A nest is
//! ordinary terrain: a crow whose nest is damaged rebuilds the missing cells,
//! and one whose tree top is gone picks a new site.

use spall_core::time_of_day::DayClock;
use spall_core::{GlobalCell, MaterialId};
use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;
use spall_voxel::Sample;

use super::{
    AnimalDirector, Cx, GROUND_CLEARANCE_M, Gait, LOCOMOTION_HOP, LOCOMOTION_LAND, LOCOMOTION_MOVE,
    LOCOMOTION_PECK, LOCOMOTION_ROOST, LOCOMOTION_TAKEOFF, MOVE_DT_S, Step, Walker, players,
    wrap_angle,
};

/// Chance per update (7.5 Hz) that a circling perching flyer goes down: about
/// once every 30 s of circling.
const LAND_CHANCE: f64 = 1.0 / 225.0;
/// Highest drop a flyer descends to land, metres.
const MAX_LANDING_DROP_M: f64 = 25.0;
const DESCEND_M_S: f64 = 3.0;
const CLIMB_M_S: f64 = 4.0;
/// A player this close to a landing or grounded flyer, or a sprinting one
/// within twice this, sends it back up.
const SPOOK_M: f64 = 10.0;
/// How far a circling bird may climb over its first height to get round
/// whatever blocks its circle; past that it turns round instead (ENG-230).
pub(super) const MAX_ORBIT_CLIMB_M: f64 = 12.0;
/// Flying pace to and from the nest.
const NEST_FLY_M_S: f64 = 5.0;
/// Nest cells placed per visit before flying off again.
const CELLS_PER_TRIP: u32 = 3;
/// Ticks between placing two nest cells (0.5 s).
const PLACE_TICKS: u64 = 30;
/// How long a placed cell may take to commit before it is placed again (3 s).
const COMMIT_TICKS: u64 = 180;
/// Circling between building trips: this plus up to as much again (4-8 s).
const TRIP_GAP_TICKS: u64 = 240;
/// A crow with no nest site looks again this often (30 s).
const SEARCH_TICKS: u64 = 1_800;
/// A tree top is a wood cell at least this many cells (2 m) above the ground
/// 2 m to one side of it.
const TREE_RISE_CELLS: i64 = 8;
/// The site search looks this far above and below the circle, cells.
const SEARCH_ABOVE_CELLS: i64 = 16;
const SEARCH_BELOW_CELLS: i64 = 48;
/// The material nests are built of.
const NEST_MATERIAL: MaterialId = crate::game::materials::WOOD;

/// Bird-only states.
#[derive(Debug, Clone, Copy)]
pub(super) enum BirdGait {
    /// Fly a circle around `home` at angle `theta`, rate `omega` rad/s.
    Orbit { theta: f64, omega: f64, radius: f64 },
    /// A circling bird descending to `target` on the ground. `orbit` is the
    /// circle it left, `[theta, omega, radius]`, to return to.
    Land { target: [f64; 3], orbit: [f64; 3] },
    /// On the ground until `until`: hopping to `hop` (x, z) if set, else
    /// pecking where it stands, until `act_until`.
    Perch {
        until: u64,
        act_until: u64,
        hop: Option<[f64; 2]>,
        orbit: [f64; 3],
    },
    /// Climbing back to its circle.
    TakeOff { orbit: [f64; 3] },
    /// Flying to the point above its nest, to build or (`roost`) to settle.
    ToNest { orbit: [f64; 3], roost: bool },
    /// Hovering over the nest, placing a cell every [`PLACE_TICKS`] from
    /// `next`; `placed` this visit.
    Build {
        orbit: [f64; 3],
        placed: u32,
        next: u64,
    },
    /// Dropping into the nest cup.
    Settle { orbit: [f64; 3] },
    /// Sitting in the nest for the night.
    Roost { orbit: [f64; 3] },
}

/// What a crow keeps between gaits.
#[derive(Debug, Default)]
pub(super) struct Crow {
    /// The tree-top cell its nest stands on.
    nest: Option<GlobalCell>,
    /// A cell placed and not yet seen committed, and until when to wait.
    pending: Option<(GlobalCell, u64)>,
    /// No building trip before this tick.
    next_trip: u64,
    /// No site search before this tick.
    next_search: u64,
}

/// The nest on tree top `top`, ground up: the floor cell on the top, the four
/// cells round it, then the rim over those four. Each cell's [`support`]
/// comes earlier in the list.
fn nest_cells(top: GlobalCell) -> [GlobalCell; 9] {
    let at = |dx: i64, dy: i64, dz: i64| GlobalCell::new(top.x + dx, top.y + dy, top.z + dz);
    [
        at(0, 1, 0),
        at(1, 1, 0),
        at(-1, 1, 0),
        at(0, 1, 1),
        at(0, 1, -1),
        at(1, 2, 0),
        at(-1, 2, 0),
        at(0, 2, 1),
        at(0, 2, -1),
    ]
}

/// The solid cell nest cell `cell` of the nest on `top` is joined to: the top
/// for the floor, the floor for the four round it, the cell below for the rim.
fn support(top: GlobalCell, cell: GlobalCell) -> GlobalCell {
    if cell.y == top.y + 1 {
        if cell.x == top.x && cell.z == top.z {
            top
        } else {
            GlobalCell::new(top.x, top.y + 1, top.z)
        }
    } else {
        GlobalCell::new(cell.x, cell.y - 1, cell.z)
    }
}

fn filled(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(
        sim.world().terrain().volume.sample(cell),
        Ok(Sample::Filled(_))
    )
}

fn empty(sim: &Simulation, cell: GlobalCell) -> bool {
    matches!(
        sim.world().terrain().volume.sample(cell),
        Ok(Sample::Empty { .. })
    )
}

/// Whether crow `id` roosts at `tick`: the sun is down at a time shifted back
/// by a per-crow lag, so a flock does not move as one.
fn roosting_time(clock: DayClock, tick: u64, id: u64) -> bool {
    let mut mix = id.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    mix ^= mix >> 31;
    let lag = mix % clock.hour_ticks();
    tick >= lag && clock.state_at(tick - lag).sun_height() < 0.0
}

/// Centre of a terrain cell, metres.
fn centre_m(cell: GlobalCell) -> [f64; 3] {
    [
        (cell.x as f64 + 0.5) * 0.25,
        (cell.y as f64 + 0.5) * 0.25,
        (cell.z as f64 + 0.5) * 0.25,
    ]
}

/// Where a crow hovers to build the nest on `top`: a metre over the rim.
fn hover_point(top: GlobalCell) -> [f64; 3] {
    let c = centre_m(top);
    [c[0], (top.y + 3) as f64 * 0.25 + 1.0, c[2]]
}

/// A step from `from` toward `to` of at most `max`, and whether it gets there.
fn toward(from: [f64; 3], to: [f64; 3], max: f64) -> ([f64; 3], bool) {
    let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let length = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if length <= max {
        (to, true)
    } else {
        let k = max / length;
        (
            [from[0] + d[0] * k, from[1] + d[1] * k, from[2] + d[2] * k],
            false,
        )
    }
}

/// The first gait and home of a bird placed with its root at `root`: a circle
/// centred so it starts where it was placed, above the highest ground under
/// the whole circle.
pub(super) fn start(
    dir: &mut AnimalDirector,
    sim: &Simulation,
    root: [f64; 3],
) -> (Gait, [f64; 3]) {
    let theta = dir.unit() * std::f64::consts::TAU;
    let omega = 0.35 + dir.unit() * 0.3;
    let radius = 6.0 + dir.unit() * 8.0;
    let centre = [
        root[0] - radius * theta.cos(),
        root[1],
        root[2] - radius * theta.sin(),
    ];
    let mut ceiling = root[1];
    for step in 0..16 {
        let angle = f64::from(step) * std::f64::consts::TAU / 16.0;
        let x = ((centre[0] + radius * angle.cos()) / 0.25).floor() as i64;
        let z = ((centre[2] + radius * angle.sin()) / 0.25).floor() as i64;
        if let Some(ground) = AnimalDirector::ground_y_m(sim, x, z) {
            ceiling = ceiling.max(ground + 4.0);
        }
    }
    (
        Gait::Bird(BirdGait::Orbit {
            theta,
            omega,
            radius,
        }),
        [centre[0], ceiling, centre[2]],
    )
}

/// One update of a bird.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let here = cx.here;
    let perches = dir.species[walker.species].perches;
    if perches {
        spook(walker, cx.threat);
    }
    match walker.gait {
        Gait::Bird(BirdGait::Orbit {
            theta,
            omega,
            radius,
        }) => {
            let theta = theta + omega * MOVE_DT_S;
            walker.gait = Gait::Bird(BirdGait::Orbit {
                theta,
                omega,
                radius,
            });
            let step = Step {
                fly_target: [
                    walker.home[0] + radius * theta.cos(),
                    walker.home[1],
                    walker.home[2] + radius * theta.sin(),
                ],
                // Tangent of the circle, nose along the direction of travel.
                yaw: f64::atan2(-theta.cos() * omega.signum(), -theta.sin() * omega.signum()),
                tag: LOCOMOTION_MOVE,
                desired: [0.0; 2],
                fly: Some(false),
            };
            // Now and then a perching bird goes down, unless a player is near.
            let calm = cx.threat.is_none_or(|t| t.distance_m > 2.0 * SPOOK_M);
            if perches
                && calm
                && let Some(roost) = dir.nest_trip(walker, cx, radius)
            {
                walker.stuck = 0;
                walker.detours = 0;
                walker.gait = Gait::Bird(BirdGait::ToNest {
                    orbit: [theta, omega, radius],
                    roost,
                });
                return step;
            }
            if perches
                && calm
                && dir.unit() < LAND_CHANCE
                && let Some(target) = dir.landing_spot(cx.sim, walker.species, here)
            {
                walker.gait = Gait::Bird(BirdGait::Land {
                    target,
                    orbit: [theta, omega, radius],
                });
            }
            step
        }
        Gait::Bird(
            BirdGait::ToNest { .. }
            | BirdGait::Build { .. }
            | BirdGait::Settle { .. }
            | BirdGait::Roost { .. },
        ) => dir.nest_step(walker, cx, actions),
        Gait::Bird(_) => dir.bird_step(walker, here, cx.tick),
        _ => Step::still(walker, here),
    }
}

/// Sends a landing or grounded bird back up when a player comes close.
fn spook(walker: &mut Walker, threat: Option<players::Threat>) {
    let near = threat
        .is_some_and(|t| t.distance_m < SPOOK_M || (t.sprinting && t.distance_m < 2.0 * SPOOK_M));
    if let (
        true,
        Gait::Bird(
            BirdGait::Land { orbit, .. }
            | BirdGait::Perch { orbit, .. }
            | BirdGait::ToNest { orbit, .. }
            | BirdGait::Build { orbit, .. }
            | BirdGait::Settle { orbit }
            | BirdGait::Roost { orbit },
        ),
    ) = (near, walker.gait)
    {
        walker.stuck = 0;
        walker.gait = Gait::Bird(BirdGait::TakeOff { orbit });
    }
}

/// A flight step was blocked.
pub(super) fn on_fly_blocked(walker: &mut Walker) {
    match walker.gait {
        // A bird whose circle hits something solid (a tree) flies higher, up
        // to a limit; past it, it circles the other way.
        Gait::Bird(BirdGait::Orbit {
            theta,
            omega,
            radius,
        }) => {
            if walker.home[1] + 1.5 <= walker.max_home_y {
                walker.home[1] += 1.5;
            } else {
                walker.gait = Gait::Bird(BirdGait::Orbit {
                    theta,
                    omega: -omega,
                    radius,
                });
            }
        }
        // Something in the way down: give up and go back up.
        Gait::Bird(BirdGait::Land { orbit, .. } | BirdGait::Settle { orbit }) => {
            walker.stuck = 0;
            walker.gait = Gait::Bird(BirdGait::TakeOff { orbit });
        }
        Gait::Bird(BirdGait::TakeOff { .. }) => walker.stuck += 1,
        // On the way to the nest: climb, then try again; `detours` counts
        // every block on this flight.
        Gait::Bird(BirdGait::ToNest { .. } | BirdGait::Build { .. }) => {
            walker.stuck += 1;
            walker.detours += 1;
        }
        _ => {}
    }
}

/// A flight step was applied.
pub(super) fn on_fly_moved(walker: &mut Walker) {
    if matches!(
        walker.gait,
        Gait::Bird(BirdGait::TakeOff { .. } | BirdGait::ToNest { .. } | BirdGait::Build { .. })
    ) {
        walker.stuck = 0;
    }
}

impl AnimalDirector {
    /// One update of a perching bird's landing, time on the ground, or climb
    /// back to its circle.
    fn bird_step(&mut self, walker: &mut Walker, here: [f64; 3], tick: u64) -> Step {
        let mut step = Step {
            tag: LOCOMOTION_PECK,
            yaw: walker.yaw,
            desired: [0.0; 2],
            fly_target: here,
            fly: None,
        };
        let Gait::Bird(gait) = walker.gait else {
            return step;
        };
        match gait {
            BirdGait::Land { target, orbit } => {
                let (next, arrived) = toward(here, target, DESCEND_M_S * MOVE_DT_S);
                if arrived {
                    walker.gait = Gait::Bird(BirdGait::Perch {
                        until: tick + 600 + self.next() % 600,
                        act_until: tick + 60,
                        hop: None,
                        orbit,
                    });
                }
                step.tag = LOCOMOTION_LAND;
                step.fly_target = next;
                step.fly = Some(true);
            }
            BirdGait::Perch {
                until,
                act_until,
                hop,
                orbit,
            } => {
                if tick >= until {
                    walker.stuck = 0;
                    walker.gait = Gait::Bird(BirdGait::TakeOff { orbit });
                    step.tag = LOCOMOTION_TAKEOFF;
                } else if let Some(goal) = hop {
                    let (dx, dz) = (goal[0] - here[0], goal[1] - here[2]);
                    let distance = dx.hypot(dz);
                    if distance < 0.1 || walker.stuck >= 2 || tick >= act_until {
                        // Arrived (or cannot get there): peck for a while.
                        walker.gait = Gait::Bird(BirdGait::Perch {
                            until,
                            act_until: tick + 60 + self.next() % 90,
                            hop: None,
                            orbit,
                        });
                    } else {
                        let want = f64::atan2(-dz, dx);
                        step.yaw = walker.yaw + wrap_angle(want - walker.yaw).clamp(-0.8, 0.8);
                        let pace = self.species[walker.species].walk_speed_m_s * MOVE_DT_S;
                        let reach = pace.min(distance);
                        step.desired = [dx / distance * reach, dz / distance * reach];
                        step.tag = LOCOMOTION_HOP;
                    }
                } else if tick >= act_until {
                    // Pecked enough here: hop a short way, or keep pecking.
                    let hop = if self.unit() < 0.55 {
                        let angle = self.unit() * std::f64::consts::TAU;
                        let distance = 0.3 + self.unit() * 0.7;
                        walker.stuck = 0;
                        Some([
                            here[0] + distance * angle.cos(),
                            here[2] - distance * angle.sin(),
                        ])
                    } else {
                        None
                    };
                    walker.gait = Gait::Bird(BirdGait::Perch {
                        until,
                        act_until: tick
                            + if hop.is_some() {
                                180
                            } else {
                                60 + self.next() % 90
                            },
                        hop,
                        orbit,
                    });
                }
            }
            BirdGait::TakeOff {
                orbit: [theta, omega, radius],
            } => {
                let circle = [
                    walker.home[0] + radius * theta.cos(),
                    walker.home[1],
                    walker.home[2] + radius * theta.sin(),
                ];
                let climb = CLIMB_M_S * MOVE_DT_S;
                step.tag = LOCOMOTION_TAKEOFF;
                step.fly = Some(true);
                if walker.stuck > 8 {
                    // Boxed in: stay on the ground a while longer.
                    walker.stuck = 0;
                    walker.gait = Gait::Bird(BirdGait::Perch {
                        until: tick + 300,
                        act_until: tick + 60,
                        hop: None,
                        orbit: [theta, omega, radius],
                    });
                    step.fly = None;
                    step.tag = LOCOMOTION_PECK;
                } else if walker.stuck > 0 {
                    // Blocked on the way: go straight up first.
                    step.fly_target = [here[0], here[1] + climb, here[2]];
                } else {
                    let (next, arrived) = toward(here, circle, climb);
                    step.fly_target = next;
                    let (dx, dz) = (circle[0] - here[0], circle[2] - here[2]);
                    if dx.hypot(dz) > 0.5 {
                        step.yaw = f64::atan2(-dz, dx);
                    }
                    if arrived {
                        walker.gait = Gait::Bird(BirdGait::Orbit {
                            theta,
                            omega,
                            radius,
                        });
                    }
                }
            }
            _ => {}
        }
        step
    }

    /// Where a circling bird at `here` would land: the ground straight below,
    /// if it is near enough and dry.
    fn landing_spot(&self, sim: &Simulation, species: usize, here: [f64; 3]) -> Option<[f64; 3]> {
        let (x, z) = (
            (here[0] / 0.25).floor() as i64,
            (here[2] / 0.25).floor() as i64,
        );
        let ground = Self::ground_y_m(sim, x, z)?;
        if here[1] - ground > MAX_LANDING_DROP_M {
            return None;
        }
        let wet = sim.column_water().is_some_and(|water| {
            water
                .water_sample_at_m(here[0], ground + 0.1, here[2])
                .is_some_and(|w| w.surface_y > ground + 0.05)
        });
        if wet {
            return None;
        }
        let root_y = ground - f64::from(self.perch_feet[species]) + GROUND_CLEARANCE_M;
        Some([here[0], root_y, here[2]])
    }
}

/// Terrain materials that count as a tree: wood and its appearance variants.
fn wood_materials() -> Vec<MaterialId> {
    let wood = crate::game::materials::WOOD;
    std::iter::once(wood)
        .chain(
            crate::appearance::variants()
                .into_iter()
                .filter(|variant| variant.base == wood)
                .map(|variant| variant.id),
        )
        .collect()
}

impl AnimalDirector {
    /// Whether a circling crow sets off for its nest now: `Some(true)` to roost
    /// in a finished nest at night, `Some(false)` to build on an unfinished
    /// one. Finds a site first if it has none.
    fn nest_trip(&mut self, walker: &Walker, cx: &Cx, radius: f64) -> Option<bool> {
        let tick = cx.tick;
        let search = {
            let crow = self.crows.entry(cx.id).or_default();
            if let Some(top) = crow.nest
                && !filled(cx.sim, top)
            {
                tracing::info!(creature = cx.id, ?top, "crow lost its nest site");
                crow.nest = None;
                crow.pending = None;
                crow.next_search = tick;
            }
            let search = crow.nest.is_none() && tick >= crow.next_search;
            if search {
                crow.next_search = tick + SEARCH_TICKS;
            }
            search
        };
        if search && let Some(top) = self.nest_site(cx.sim, cx.id, walker.home, radius) {
            tracing::info!(creature = cx.id, ?top, "crow picked a nest site");
            let crow = self.crows.entry(cx.id).or_default();
            crow.nest = Some(top);
            crow.next_trip = tick;
        }
        let crow = self.crows.get(&cx.id)?;
        let top = crow.nest?;
        if nest_cells(top).iter().all(|&cell| filled(cx.sim, cell)) {
            roosting_time(cx.clock, tick, cx.id).then_some(true)
        } else {
            (tick >= crow.next_trip).then_some(false)
        }
    }

    /// The highest tree top within reach of a circle of `radius` round `home`
    /// with room for a nest, not next to another crow's.
    fn nest_site(
        &self,
        sim: &Simulation,
        id: u64,
        home: [f64; 3],
        radius: f64,
    ) -> Option<GlobalCell> {
        let wood = wood_materials();
        let others: Vec<GlobalCell> = self
            .crows
            .iter()
            .filter(|&(&other, _)| other != id)
            .filter_map(|(_, crow)| crow.nest)
            .collect();
        let reach = ((radius + 4.0) / 0.25).ceil() as i64;
        let (hx, hz) = (
            (home[0] / 0.25).floor() as i64,
            (home[2] / 0.25).floor() as i64,
        );
        let y_hi = (home[1] / 0.25).floor() as i64 + SEARCH_ABOVE_CELLS;
        let y_lo = y_hi - SEARCH_ABOVE_CELLS - SEARCH_BELOW_CELLS;
        let world = sim.world();
        let volume = &world.terrain().volume;
        let sample = |x: i64, y: i64, z: i64| volume.sample(GlobalCell::new(x, y, z));
        let mut tops = Vec::new();
        for x in hx - reach..=hx + reach {
            for z in hz - reach..=hz + reach {
                if (x - hx).pow(2) + (z - hz).pow(2) > reach * reach {
                    continue;
                }
                // The column's top within the band; one solid at the band's
                // top reaches above it and is skipped.
                for y in (y_lo..=y_hi).rev() {
                    match sample(x, y, z) {
                        Ok(Sample::Empty { .. }) => continue,
                        Ok(Sample::Filled(m)) if y < y_hi && wood.contains(&m) => {
                            tops.push(GlobalCell::new(x, y, z));
                        }
                        _ => {}
                    }
                    break;
                }
            }
        }
        let away = |c: &GlobalCell| (c.x - hx).pow(2) + (c.z - hz).pow(2);
        tops.sort_by(|a, b| b.y.cmp(&a.y).then(away(a).cmp(&away(b))));
        let clear = |cell: GlobalCell| matches!(volume.sample(cell), Ok(Sample::Empty { .. }));
        tops.into_iter().find(|&top| {
            let apart = others
                .iter()
                .all(|o| (o.x - top.x).abs().max((o.z - top.z).abs()) > TREE_RISE_CELLS);
            // Room for the nest and for the crow to drop into it.
            let room = nest_cells(top).into_iter().all(clear)
                && (top.y + 2..=top.y + 6).all(|y| clear(GlobalCell::new(top.x, y, top.z)));
            // A tree, not a wooden floor: 2 m to one side, nothing but air or
            // wood (branches) for 2 m below the top.
            let rises = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|&(dx, dz)| {
                let (x, z) = (top.x + dx * TREE_RISE_CELLS, top.z + dz * TREE_RISE_CELLS);
                (top.y - TREE_RISE_CELLS + 1..=top.y).all(|y| match sample(x, y, z) {
                    Ok(Sample::Empty { .. }) => true,
                    Ok(Sample::Filled(m)) => wood.contains(&m),
                    _ => false,
                })
            });
            apart && room && rises
        })
    }

    /// One update of a crow flying to, building, settling into or roosting in
    /// its nest.
    fn nest_step(
        &mut self,
        walker: &mut Walker,
        cx: &Cx,
        actions: &mut Vec<CreatureAction>,
    ) -> Step {
        let (tick, here) = (cx.tick, cx.here);
        let mut step = Step {
            tag: LOCOMOTION_MOVE,
            yaw: walker.yaw,
            desired: [0.0; 2],
            fly_target: here,
            fly: Some(false),
        };
        let Gait::Bird(
            gait @ (BirdGait::ToNest { orbit, .. }
            | BirdGait::Build { orbit, .. }
            | BirdGait::Settle { orbit }
            | BirdGait::Roost { orbit }),
        ) = walker.gait
        else {
            return step;
        };
        let gap = TRIP_GAP_TICKS + self.next() % TRIP_GAP_TICKS;
        let feet = f64::from(self.perch_feet[walker.species]);
        let crow = self.crows.entry(cx.id).or_default();
        let leave = |walker: &mut Walker, mut step: Step| {
            walker.stuck = 0;
            walker.gait = Gait::Bird(BirdGait::TakeOff { orbit });
            step.tag = LOCOMOTION_TAKEOFF;
            step.fly = Some(true);
            step.fly_target = here;
            step
        };
        let Some(top) = crow.nest.filter(|&top| filled(cx.sim, top)) else {
            return leave(walker, step);
        };
        let complete = nest_cells(top).iter().all(|&cell| filled(cx.sim, cell));
        let hover = hover_point(top);
        let pace = NEST_FLY_M_S * MOVE_DT_S;
        match gait {
            BirdGait::ToNest { roost, .. } => {
                if (roost && !complete) || walker.detours > 12 {
                    crow.next_trip = tick + gap;
                    return leave(walker, step);
                }
                if walker.stuck > 0 {
                    // Blocked: climb, then try again.
                    step.fly_target = [here[0], here[1] + pace, here[2]];
                    return step;
                }
                let (dx, dz) = (hover[0] - here[0], hover[2] - here[2]);
                let flat = dx.hypot(dz);
                // Cross over at the higher of its height and the hover point,
                // then drop to it.
                let goal = if flat > 1.0 {
                    [hover[0], here[1].max(hover[1]), hover[2]]
                } else {
                    hover
                };
                let (next, arrived) = toward(here, goal, pace);
                step.fly_target = next;
                if flat > 0.5 {
                    step.yaw = f64::atan2(-dz, dx);
                }
                if arrived && flat <= 1.0 {
                    walker.gait = Gait::Bird(if roost {
                        BirdGait::Settle { orbit }
                    } else {
                        BirdGait::Build {
                            orbit,
                            placed: 0,
                            next: tick,
                        }
                    });
                }
            }
            BirdGait::Build { placed, next, .. } => {
                step.fly_target = toward(here, hover, pace).0;
                if placed >= CELLS_PER_TRIP || complete {
                    crow.next_trip = tick + gap;
                    return leave(walker, step);
                }
                if tick < next {
                    return step;
                }
                if let Some((cell, until)) = crow.pending
                    && (filled(cx.sim, cell) || tick >= until)
                {
                    crow.pending = None;
                }
                if crow.pending.is_some() {
                    return step;
                }
                // The lowest missing cell: its support comes earlier in the
                // list, so it is solid already.
                let Some(cell) = nest_cells(top)
                    .into_iter()
                    .find(|&cell| !filled(cx.sim, cell))
                else {
                    return step;
                };
                if !empty(cx.sim, cell) || !filled(cx.sim, support(top, cell)) {
                    // Not resident, or its support went: start again elsewhere.
                    crow.nest = None;
                    crow.next_search = tick;
                    return leave(walker, step);
                }
                actions.push(CreatureAction::EditCell {
                    creature_id: cx.id,
                    cell,
                    place: Some(NEST_MATERIAL),
                });
                crow.pending = Some((cell, tick + COMMIT_TICKS));
                walker.gait = Gait::Bird(BirdGait::Build {
                    orbit,
                    placed: placed + 1,
                    next: tick + PLACE_TICKS,
                });
            }
            BirdGait::Settle { .. } => {
                if !complete {
                    return leave(walker, step);
                }
                // Feet on the nest floor, inside the rim.
                let c = centre_m(top);
                let rest = [
                    c[0],
                    (top.y + 2) as f64 * 0.25 - feet + GROUND_CLEARANCE_M,
                    c[2],
                ];
                let (next, arrived) = toward(here, rest, DESCEND_M_S * MOVE_DT_S);
                step.tag = LOCOMOTION_LAND;
                step.fly = Some(true);
                step.fly_target = next;
                if arrived {
                    walker.gait = Gait::Bird(BirdGait::Roost { orbit });
                }
            }
            BirdGait::Roost { .. } => {
                if !complete || !roosting_time(cx.clock, tick, cx.id) {
                    return leave(walker, step);
                }
                step.tag = LOCOMOTION_ROOST;
                step.fly = None;
            }
            _ => {}
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::time_of_day::DAY_CYCLE_TICKS;

    /// Every nest cell's support is the tree top or a cell placed before it,
    /// and touches it face to face, so placing them in order never leaves one
    /// hanging.
    #[test]
    fn nest_cells_are_supported_in_build_order() {
        let top = GlobalCell::new(5, 9, -3);
        let cells = nest_cells(top);
        for (i, &cell) in cells.iter().enumerate() {
            let under = support(top, cell);
            assert!(
                under == top || cells[..i].contains(&under),
                "cell {i} rests on {under:?}"
            );
            let gap =
                (under.x - cell.x).abs() + (under.y - cell.y).abs() + (under.z - cell.z).abs();
            assert_eq!(gap, 1, "cell {i} does not touch its support");
        }
    }

    #[test]
    fn crows_roost_at_night_and_not_by_day() {
        let hour = DAY_CYCLE_TICKS / 24;
        // Tick 0 is 06:00.
        let at = |h: u64| DAY_CYCLE_TICKS * 2 + ((h + 18) % 24) * hour;
        for id in 0..100 {
            assert!(
                !roosting_time(DayClock::default(), at(12), id),
                "crow {id} roosting at noon"
            );
            assert!(
                roosting_time(DayClock::default(), at(0), id),
                "crow {id} up at midnight"
            );
        }
    }
}
