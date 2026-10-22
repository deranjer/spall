//! The calm walk shared by land animals: rest a while, then walk to a target,
//! detouring round whatever blocks the way.

use super::{AnimalDirector, Cx, Gait, LOCOMOTION_MOVE, MOVE_DT_S, Step, Walker, wrap_angle};

/// The first gait of a land animal: a short rest.
pub(super) fn start(dir: &mut AnimalDirector, tick: u64) -> Gait {
    Gait::Rest {
        until: tick + 60 + (dir.next() % 240),
    }
}

/// A random wander target 4 to 18 m from `here` (x, z).
pub(super) fn wander(dir: &mut AnimalDirector, here: [f64; 3]) -> [f64; 2] {
    let angle = dir.unit() * std::f64::consts::TAU;
    let distance = 4.0 + dir.unit() * 14.0;
    [
        here[0] + distance * angle.cos(),
        here[2] - distance * angle.sin(),
    ]
}

/// One update of `Rest` or `Walk`; any other gait stands still. A rest that
/// ends walks to `target(dir, here)`; a stuck walker tries detours that
/// `detour_ok` accepts.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    target: impl FnOnce(&mut AnimalDirector, [f64; 3]) -> [f64; 2],
    detour_ok: impl FnOnce([f64; 2]) -> bool,
) -> Step {
    let (here, tick) = (cx.here, cx.tick);
    let mut step = Step::still(walker, here);
    match walker.gait {
        Gait::Rest { until } => {
            if tick >= until {
                walker.gait = Gait::Walk {
                    target: target(dir, here),
                    until: tick + 60 * 20,
                };
            }
        }
        Gait::Walk { target, until } => {
            let (dx, dz) = (target[0] - here[0], target[1] - here[2]);
            let distance = dx.hypot(dz);
            if distance < 0.4 || tick >= until {
                walker.gait = Gait::Rest {
                    until: tick + 60 + (dir.next() % 240),
                };
            } else if walker.stuck >= 3 {
                // Not getting anywhere: try another heading, wider each time,
                // and stand for a while if every one fails.
                walker.stuck = 0;
                walker.detours += 1;
                if walker.detours > 6 {
                    walker.detours = 0;
                    walker.gait = Gait::Rest { until: tick + 180 };
                } else {
                    const TURNS: [f64; 6] = [0.8, -0.8, 1.6, -1.6, 2.4, -2.4];
                    let angle = walker.yaw + TURNS[(walker.detours as usize - 1) % 6];
                    let detour = [here[0] + 5.0 * angle.cos(), here[2] - 5.0 * angle.sin()];
                    // A rejected detour leaves the walker stuck, so the next
                    // step tries the next heading.
                    if detour_ok(detour) {
                        walker.gait = Gait::Walk {
                            target: detour,
                            until,
                        };
                    } else {
                        walker.stuck = 3;
                    }
                }
            } else {
                let want = f64::atan2(-dz, dx);
                let turn = wrap_angle(want - walker.yaw);
                step.tag = LOCOMOTION_MOVE;
                if turn.abs() > 0.35 {
                    // Turn toward the target in place, a bounded angle per step.
                    step.yaw = walker.yaw + turn.clamp(-0.35, 0.35);
                } else {
                    step.yaw = walker.yaw + turn;
                    let speed = dir.species[walker.species].walk_speed_m_s;
                    let reach = (speed * MOVE_DT_S).min(distance);
                    step.desired = [dx / distance * reach, dz / distance * reach];
                }
            }
        }
        _ => {}
    }
    step
}
