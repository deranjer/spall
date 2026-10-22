//! Deer: the shared land walk, fleeing players, and cropping the grass under
//! their mouths now and then while resting.

use spall_server::creature_service::CreatureAction;
use spall_sim::Simulation;

use super::{AnimalDirector, Cx, Gait, LOCOMOTION_GRAZE, Step, Walker, land, players};

/// The worldgen grass material; its appearance variants count as grass too.
const GRASS: spall_core::MaterialId = spall_core::MaterialId(10);
/// Chance a grazing species crops the grass when a rest ends on grass.
const GRAZE_CHANCE: f64 = 0.5;
/// Ticks between bites while grazing (1.5 s).
const BITE_TICKS: u64 = 90;

/// Deer-only states.
#[derive(Debug, Clone, Copy)]
pub(super) enum DeerGait {
    /// Stand cropping the grass under the mouth, a bite every [`BITE_TICKS`],
    /// until `until` or the grass in reach is gone.
    Graze { until: u64, next_bite: u64 },
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
    if flee_speed.is_some() {
        dir.react_to_players(walker, cx.threat, tick);
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
