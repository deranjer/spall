//! Birds: circling flight, and for perching species landing, hopping and
//! pecking on the ground, then climbing back to the circle. A player who comes
//! close spooks a landing or grounded bird back up.

use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;

use super::{
    AnimalDirector, Cx, GROUND_CLEARANCE_M, Gait, LOCOMOTION_HOP, LOCOMOTION_LAND, LOCOMOTION_MOVE,
    LOCOMOTION_PECK, LOCOMOTION_TAKEOFF, MOVE_DT_S, Step, Walker, players, wrap_angle,
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
    _actions: &mut Vec<CreatureAction>,
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
        Gait::Bird(_) => dir.bird_step(walker, here, cx.tick),
        _ => Step::still(walker, here),
    }
}

/// Sends a landing or grounded bird back up when a player comes close.
fn spook(walker: &mut Walker, threat: Option<players::Threat>) {
    let near = threat
        .is_some_and(|t| t.distance_m < SPOOK_M || (t.sprinting && t.distance_m < 2.0 * SPOOK_M));
    if let (true, Gait::Bird(BirdGait::Land { orbit, .. } | BirdGait::Perch { orbit, .. })) =
        (near, walker.gait)
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
        Gait::Bird(BirdGait::Land { orbit, .. }) => {
            walker.stuck = 0;
            walker.gait = Gait::Bird(BirdGait::TakeOff { orbit });
        }
        Gait::Bird(BirdGait::TakeOff { .. }) => walker.stuck += 1,
        _ => {}
    }
}

/// A flight step was applied.
pub(super) fn on_fly_moved(walker: &mut Walker) {
    if matches!(walker.gait, Gait::Bird(BirdGait::TakeOff { .. })) {
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
        let toward = |from: [f64; 3], to: [f64; 3], max: f64| -> ([f64; 3], bool) {
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
            BirdGait::Orbit { .. } => {}
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
