//! Players as threats: where they are, the nearest one to an animal, and how
//! a species that flees players moves between its calm gaits, `Alert` and
//! `Flee`.

use spall_sim::Simulation;

use super::{
    AnimalDirector, Cx, Gait, LOCOMOTION_ALERT, LOCOMOTION_RUN, MOVE_DT_S, Step, Walker, wrap_angle,
};

/// A player this close is noticed by a fleeing species, which stops to watch.
const NOTICE_M: f64 = 28.0;
/// A player this close, or a sprinting one inside the notice distance, makes
/// the animal bolt at once.
const BOLT_M: f64 = 14.0;
/// A fleeing animal calms down once every player is this far away.
const CALM_M: f64 = 45.0;
/// Horizontal speed above which a player counts as sprinting (walk is slower).
const SPRINT_M_S: f32 = 4.5;

/// The nearest player to an animal, horizontally.
#[derive(Debug, Clone, Copy)]
pub(super) struct Threat {
    pub(super) distance_m: f64,
    /// Unit vector from the player toward the animal (x, z).
    pub(super) away: [f64; 2],
    pub(super) sprinting: bool,
}

/// Something a fleeing animal may run from: where it is, whether it counts as
/// sprinting, and how close it must be to be seen at all.
#[derive(Debug, Clone, Copy)]
pub(super) struct ThreatSource {
    pub(super) at: [f64; 3],
    pub(super) sprinting: bool,
    pub(super) seen_within_m: f64,
}

impl ThreatSource {
    /// Seen at any distance.
    pub(super) fn open(at: [f64; 3], sprinting: bool) -> Self {
        Self {
            at,
            sprinting,
            seen_within_m: f64::INFINITY,
        }
    }
}

/// Every player's position and whether it is sprinting, then every bear that
/// is not stalking and every wolf (a charging bear or a wolf running in counts
/// as sprinting; an encircling wolf is seen only close up).
pub(super) fn threat_sources(dir: &AnimalDirector, sim: &Simulation) -> Vec<ThreatSource> {
    sim.world()
        .players()
        .map(|player| {
            let [vx, _, vz] = player.state.velocity_m_s;
            ThreatSource::open(player.state.position_m, vx.hypot(vz) > SPRINT_M_S)
        })
        .chain(super::bears::threats(dir).map(|(at, sprinting)| ThreatSource::open(at, sprinting)))
        .chain(super::wolves::threats(dir))
        .collect()
}

/// The nearest source `here` can see, horizontally.
pub(super) fn nearest_threat(sources: &[ThreatSource], here: [f64; 3]) -> Option<Threat> {
    sources
        .iter()
        .filter_map(|source| {
            let ThreatSource { at, sprinting, .. } = *source;
            let (dx, dz) = (here[0] - at[0], here[2] - at[2]);
            let distance_m = dx.hypot(dz);
            if distance_m > source.seen_within_m {
                return None;
            }
            let away = if distance_m > 1.0e-6 {
                [dx / distance_m, dz / distance_m]
            } else {
                [1.0, 0.0]
            };
            Some(Threat {
                distance_m,
                away,
                sprinting,
            })
        })
        .min_by(|a, b| a.distance_m.total_cmp(&b.distance_m))
}

impl AnimalDirector {
    /// Moves a species that flees players between its calm gaits, `Alert` and
    /// `Flee`, from where the nearest player is.
    pub(super) fn react_to_players(
        &mut self,
        walker: &mut Walker,
        threat: Option<Threat>,
        tick: u64,
    ) {
        let bolts = |t: &Threat| t.distance_m < BOLT_M || (t.sprinting && t.distance_m < NOTICE_M);
        let flee = Gait::Flee {
            until: tick + 60 * 20,
        };
        match walker.gait {
            Gait::Flee { until } => {
                if tick >= until || threat.is_none_or(|t| t.distance_m > CALM_M) {
                    walker.detours = 0;
                    walker.gait = Gait::Rest {
                        until: tick + 120 + (self.next() % 240),
                    };
                }
            }
            Gait::Alert { until } => match threat {
                Some(t) if bolts(&t) => walker.gait = flee,
                // Watched long enough: leave if the player is still around.
                Some(t) if tick >= until && t.distance_m < NOTICE_M => walker.gait = flee,
                _ if tick >= until => {
                    walker.gait = Gait::Rest {
                        until: tick + 60 + (self.next() % 120),
                    };
                }
                _ => {}
            },
            // A player interrupts grazing like any other calm moment.
            Gait::Rest { .. } | Gait::Walk { .. } | Gait::Deer(_) => match threat {
                Some(t) if bolts(&t) => {
                    walker.detours = 0;
                    walker.gait = flee;
                }
                Some(t) if t.distance_m < NOTICE_M => {
                    walker.gait = Gait::Alert {
                        until: tick + 90 + (self.next() % 90),
                    };
                }
                _ => {}
            },
            // Other families' states do not react to players here.
            _ => {}
        }
    }
}

/// One update of `Alert`: turn to watch the player, a bounded angle per step.
pub(super) fn alert(walker: &Walker, cx: &Cx) -> Step {
    let mut step = Step::still(walker, cx.here);
    if let Some(threat) = cx.threat {
        let want = f64::atan2(threat.away[1], -threat.away[0]);
        step.yaw = walker.yaw + wrap_angle(want - walker.yaw).clamp(-0.6, 0.6);
    }
    step.tag = LOCOMOTION_ALERT;
    step
}

/// One update of `Flee` at `speed_m_s`: straight away from the nearest player;
/// a blocked runner veers further off each time until it gets going again.
pub(super) fn flee(walker: &mut Walker, cx: &Cx, speed_m_s: f64) -> Step {
    let mut step = Step::still(walker, cx.here);
    let yaw = walker.yaw;
    let away = cx.threat.map_or([yaw.cos(), -yaw.sin()], |t| t.away);
    const VEERS: [f64; 6] = [0.9, -0.9, 1.6, -1.6, 2.3, -2.3];
    if walker.stuck >= 2 {
        walker.stuck = 0;
        walker.detours += 1;
    }
    let veer = match walker.detours {
        0 => 0.0,
        n => VEERS[(n as usize - 1) % VEERS.len()],
    };
    let want = f64::atan2(-away[1], away[0]) + veer;
    // Turns sharply, but keeps running through the turn.
    step.yaw = walker.yaw + wrap_angle(want - walker.yaw).clamp(-1.2, 1.2);
    let distance = speed_m_s * MOVE_DT_S;
    step.desired = [distance * step.yaw.cos(), -distance * step.yaw.sin()];
    step.tag = LOCOMOTION_RUN;
    step
}
