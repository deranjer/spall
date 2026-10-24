//! Deer: the shared land walk, fleeing players, cropping the grass under
//! their mouths now and then while resting, and sleeping through the night.

use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;

use super::{
    AnimalDirector, Cx, Gait, LOCOMOTION_GRAZE, LOCOMOTION_SLEEP, Step, Walker, land, players,
};

/// The worldgen grass material; its appearance variants count as grass too.
const GRASS: spall_core::MaterialId = spall_core::MaterialId(10);
/// Chance a grazing species crops the grass when a rest ends on grass.
const GRAZE_CHANCE: f64 = 0.5;
/// Ticks between bites while grazing (1.5 s).
const BITE_TICKS: u64 = 90;
/// A sleeping deer wakes for a walking player this close...
const WAKE_M: f64 = 10.0;
/// ...or a sprinting one this close. Both are inside the awake notice
/// distance in `players.rs`, so a player can creep up on a sleeping herd.
const WAKE_SPRINT_M: f64 = 20.0;
/// Each deer's bedtime and wake time trail sunset and sunrise by up to this
/// many ticks (one in-game hour), so a herd does not switch in one tick.
const SLEEP_LAG_TICKS: u64 = 3_000;

/// Deer-only states.
#[derive(Debug, Clone, Copy)]
pub(super) enum DeerGait {
    /// Stand cropping the grass under the mouth, a bite every [`BITE_TICKS`],
    /// until `until` or the grass in reach is gone.
    Graze { until: u64, next_bite: u64 },
    /// Lie still with the `sleep` clip until morning or a player comes close.
    Sleep,
}

/// Whether deer `id` sleeps at `tick`: the sun is down
/// ([`spall_core::time_of_day`]) at a time shifted back by a per-deer lag, so
/// bedtime and waking are staggered across a herd.
fn sleepy(tick: u64, id: u64) -> bool {
    let mut mix = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mix ^= mix >> 29;
    let lag = mix % SLEEP_LAG_TICKS;
    tick >= lag && spall_core::time_of_day::state_at_tick(tick - lag).sun_height() < 0.0
}

/// Whether the nearest player is close enough to wake a sleeping deer.
fn wakes(threat: Option<players::Threat>) -> bool {
    threat.is_some_and(|t| t.distance_m < if t.sprinting { WAKE_SPRINT_M } else { WAKE_M })
}

/// Terrain materials that count as grass.
pub(super) fn grass_materials() -> Vec<spall_core::MaterialId> {
    std::iter::once(GRASS)
        .chain(
            crate::appearance::variants()
                .into_iter()
                .filter(|variant| variant.base == GRASS)
                .map(|variant| variant.id),
        )
        .collect()
}

/// One update of a deer.
pub(super) fn step(
    dir: &mut AnimalDirector,
    walker: &mut Walker,
    cx: &Cx,
    actions: &mut Vec<CreatureAction>,
) -> Step {
    let (sim, here, tick) = (cx.sim, cx.here, cx.tick);
    let flee_speed = dir.species[walker.species].flee_speed_m_s;
    // Morning or a close player ends sleep; a player farther off goes
    // unnoticed. A woken deer then reacts to players like a resting one.
    if matches!(walker.gait, Gait::Deer(DeerGait::Sleep))
        && (wakes(cx.threat) || !sleepy(tick, cx.id))
    {
        walker.gait = Gait::Rest {
            until: tick + 60 + dir.next() % 240,
        };
    }
    if !matches!(walker.gait, Gait::Deer(DeerGait::Sleep)) && flee_speed.is_some() {
        dir.react_to_players(walker, cx.threat, tick);
    }
    // A resting or wandering deer lies down where it is at bedtime.
    if matches!(walker.gait, Gait::Rest { .. } | Gait::Walk { .. }) && sleepy(tick, cx.id) {
        walker.gait = Gait::Deer(DeerGait::Sleep);
    }
    if matches!(walker.gait, Gait::Deer(DeerGait::Sleep)) {
        let mut step = Step::still(walker, here);
        step.tag = LOCOMOTION_SLEEP;
        return step;
    }
    let mut step = match walker.gait {
        Gait::Rest { until } => {
            let grazes = dir.species[walker.species].grazes
                && tick >= until
                && dir.uncropped_grass(sim, dir.mouth_column(walker.species, here, walker.yaw, 0))
                && dir.unit() < GRAZE_CHANCE;
            if grazes {
                walker.gait = Gait::Deer(DeerGait::Graze {
                    until: tick + 300 + dir.next() % 420,
                    next_bite: tick,
                });
                Step::still(walker, here)
            } else {
                land::step(dir, walker, cx, land::wander, |_| true)
            }
        }
        Gait::Walk { .. } => land::step(dir, walker, cx, land::wander, |_| true),
        Gait::Alert { .. } => players::alert(walker, cx),
        Gait::Flee { .. } => players::flee(walker, cx, flee_speed.unwrap_or(0.0)),
        _ => Step::still(walker, here),
    };
    // Grazing is checked after the match so a rest that just turned into
    // grazing takes its first bite this update.
    if let Gait::Deer(DeerGait::Graze { until, next_bite }) = walker.gait {
        step.tag = LOCOMOTION_GRAZE;
        if tick >= until {
            // Had enough here: walk on after this update.
            walker.gait = Gait::Rest { until: tick };
        } else if tick >= next_bite {
            let bite = [0, 1, -1]
                .into_iter()
                .map(|side| dir.mouth_column(walker.species, here, step.yaw, side))
                .find(|&column| dir.uncropped_grass(sim, column));
            match bite {
                Some(column) => {
                    walker.gait = Gait::Deer(DeerGait::Graze {
                        until,
                        next_bite: tick + BITE_TICKS,
                    });
                    actions.push(CreatureAction::Graze {
                        creature_id: cx.id,
                        column,
                    });
                }
                // Nothing left in reach.
                None => walker.gait = Gait::Rest { until: tick },
            }
        }
    }
    step
}

/// A graze action's outcome: when the grass in reach is gone, stop and move on.
pub(super) fn on_grazed(walker: &mut Walker, ate: bool, last_tick: u64) {
    if !ate && matches!(walker.gait, Gait::Deer(DeerGait::Graze { .. })) {
        walker.gait = Gait::Rest { until: last_tick };
    }
}

impl AnimalDirector {
    /// The terrain column under the mouth of a grazing animal at `here` facing
    /// `yaw`, shifted `side` cells to its left (+) or right (-).
    fn mouth_column(&self, species: usize, here: [f64; 3], yaw: f64, side: i64) -> [i64; 2] {
        let reach = self.mouth_m[species] * 0.9;
        let side_m = side as f64 * 0.25;
        let x = here[0] + reach * yaw.cos() - side_m * yaw.sin();
        let z = here[2] - reach * yaw.sin() - side_m * yaw.cos();
        [(x / 0.25).floor() as i64, (z / 0.25).floor() as i64]
    }

    /// Whether the column carries grass that is not cropped or regrowing. A
    /// world without vegetation has none to eat.
    fn uncropped_grass(&self, sim: &Simulation, column: [i64; 2]) -> bool {
        let Some(vegetation) = sim.vegetation_state() else {
            return false;
        };
        let (Ok(x), Ok(z)) = (u16::try_from(column[0]), u16::try_from(column[1])) else {
            return false;
        };
        let eaten = vegetation
            .cleared
            .binary_search_by_key(&(x, z), |c| (c.x, c.z))
            .is_ok();
        !eaten
            && Self::ground(sim, column[0], column[1])
                .is_some_and(|(_, material)| self.grass.contains(&material))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::time_of_day::DAY_CYCLE_TICKS;

    const HOUR: u64 = DAY_CYCLE_TICKS / 24;

    /// Ticks at a clock hour on day 3 (the server tick starts at 06:00).
    fn at(hour: u64) -> u64 {
        DAY_CYCLE_TICKS * 3 + ((hour + 24 - 6) % 24) * HOUR
    }

    #[test]
    fn deer_sleep_through_the_night_and_not_by_day() {
        for id in 0..200 {
            assert!(!sleepy(at(12), id), "deer {id} asleep at noon");
            assert!(
                !sleepy(at(8), id),
                "deer {id} asleep two hours after sunrise"
            );
            assert!(sleepy(at(0), id), "deer {id} awake at midnight");
            assert!(sleepy(at(20), id), "deer {id} awake two hours after sunset");
        }
    }

    #[test]
    fn bedtime_and_waking_are_staggered_across_a_herd() {
        // Half an hour after sunset (and sunrise) some deer have switched and
        // some have not.
        let half = HOUR / 2;
        let asleep = (0..200).filter(|&id| sleepy(at(18) + half, id)).count();
        assert!(
            (20..180).contains(&asleep),
            "{asleep} of 200 asleep at 18:30"
        );
        let still = (0..200).filter(|&id| sleepy(at(6) + half, id)).count();
        assert!((20..180).contains(&still), "{still} of 200 asleep at 06:30");
        // The first night has its staggered bedtime too: tick 0 is morning.
        assert!(!sleepy(0, 1));
    }

    #[test]
    fn a_close_player_wakes_a_sleeper_and_a_sprinter_sooner() {
        let player = |distance_m, sprinting| {
            Some(players::Threat {
                distance_m,
                away: [1.0, 0.0],
                sprinting,
            })
        };
        assert!(!wakes(None));
        assert!(wakes(player(9.0, false)));
        // Walking up to here would make an awake deer bolt.
        assert!(!wakes(player(12.0, false)));
        assert!(wakes(player(18.0, true)));
        assert!(!wakes(player(22.0, true)));
    }

    #[test]
    fn only_deer_have_a_sleep_clip() {
        assert_eq!(
            super::super::locomotion_clip("deer", LOCOMOTION_SLEEP),
            Some("sleep")
        );
        assert_eq!(
            super::super::locomotion_clip("bear", LOCOMOTION_SLEEP),
            None
        );
    }
}
