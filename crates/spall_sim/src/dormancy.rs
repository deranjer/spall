//! T21 — region dormancy policy for settled debris (increment 2).
//!
//! `docs/architecture.md`: *"Settled distant bodies may be persisted and
//! deactivated only when their whole interaction region is dormant, and must
//! reactivate before contact or edits."* and *"Sleeping bodies retain geometry,
//! pose, identity, and future destructibility; nearby edits wake affected
//! neighbors."*
//!
//! [`DormancyPolicy`] is the pure decision layer between the per-tick body /
//! player state and [`crate::sim::Simulation`]'s deactivate / reactivate calls:
//!
//! * a detached body that has been **asleep and effectively still** for
//!   `settle_ticks` consecutive ticks, with **no active region** (a player, an
//!   awake body, or a pending edit) within `wake_margin_m` of its bounding
//!   sphere, becomes a candidate for deactivation;
//! * a **dormant** body with an active region inside that margin is reactivated
//!   — immediately for a hard trigger (an edit that targets it), or after
//!   `min_dormant_ticks` of hysteresis for mere proximity, so a body hovering at
//!   the margin does not thrash;
//! * deactivations and reactivations are each capped per tick, so a mass
//!   settle / a player sweeping past a rubble field is bounded work.
//!
//! Dormancy never touches authoritative geometry, ownership, or damage, so it
//! cannot change `world_hash`, conservation, or the checkpoint set — it is a
//! pure runtime-cost optimisation. This module has no `SimWorld` or physics
//! dependency and is deterministic given the same `(tick, bodies, regions)`.

use std::collections::HashMap;

use spall_core::EntityId;

/// Tunables for [`DormancyPolicy`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DormancyConfig {
    /// Consecutive ticks a body must be asleep and slower than
    /// [`Self::still_speed_m_s`], with nothing nearby, before it is eligible for
    /// deactivation. At 60 ticks/s the default is ~2 s.
    pub settle_ticks: u64,
    /// A body is kept awake, or a dormant one is woken, while an active region's
    /// surface is within this distance of the body's bounding sphere, metres.
    pub wake_margin_m: f64,
    /// After deactivation a body stays dormant at least this many ticks before a
    /// *proximity* reactivation. A hard trigger (an edit targeting it) ignores
    /// this. Prevents thrash for a body sitting on the margin.
    pub min_dormant_ticks: u64,
    /// Max bodies deactivated in one [`DormancyPolicy::plan`] pass.
    pub max_deactivations_per_tick: usize,
    /// Max bodies reactivated in one pass (a proximity wake; hard triggers are
    /// never capped — an edit must always reach a live body).
    pub max_reactivations_per_tick: usize,
    /// Linear speed at or below which a body counts as "still" for the settle
    /// counter, m/s.
    pub still_speed_m_s: f64,
}

impl DormancyConfig {
    pub const DEFAULT: Self = Self {
        settle_ticks: 120,
        wake_margin_m: 4.0,
        min_dormant_ticks: 30,
        max_deactivations_per_tick: 8,
        max_reactivations_per_tick: 8,
        still_speed_m_s: 0.05,
    };
}

impl Default for DormancyConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Per-tick state of one detached body, as the integrator observes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyDormancyInput {
    pub entity: EntityId,
    /// World-space centre of the body's bounding sphere, metres.
    pub centre_m: [f64; 3],
    /// Bounding-sphere radius, metres.
    pub radius_m: f64,
    /// Whether the solver has the body asleep (always `true` for a body that is
    /// already dormant).
    pub sleeping: bool,
    /// Current linear speed, m/s (`0` for a dormant body).
    pub speed_m_s: f64,
    /// Whether the body is currently dormant.
    pub dormant: bool,
    /// A hard trigger this tick: an edit targets this body, so it must be live
    /// now regardless of the hysteresis window. Ignored for a non-dormant body.
    pub hard_wake: bool,
}

/// A sphere the policy treats as "someone is interacting here": a player
/// capsule, an awake body, or a pending / in-flight edit's brush.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveRegion {
    pub centre_m: [f64; 3],
    pub radius_m: f64,
}

/// The deactivate / reactivate decisions for one tick, plus accounting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DormancyPlan {
    /// Bodies to deactivate, in ascending entity-id order.
    pub deactivate: Vec<EntityId>,
    /// Bodies to reactivate, in ascending entity-id order.
    pub reactivate: Vec<EntityId>,
    /// Awake bodies held awake this tick by a nearby active region.
    pub kept_awake_nearby: usize,
    /// Awake bodies partway through their settle countdown.
    pub settling: usize,
    /// Deactivations not taken this tick because the per-tick cap was reached.
    pub deferred_deactivations: usize,
    /// Proximity reactivations not taken this tick because the cap was reached
    /// (a hard-trigger wake is never deferred).
    pub deferred_reactivations: usize,
}

/// Stateful dormancy decisions: per-body settle counters and dormant-since
/// ticks.
pub struct DormancyPolicy {
    config: DormancyConfig,
    /// entity -> consecutive still-and-clear ticks while awake.
    settle: HashMap<u64, u64>,
    /// entity -> tick it became dormant (for the hysteresis window).
    dormant_since: HashMap<u64, u64>,
    last_tick: Option<u64>,
}

impl DormancyPolicy {
    pub fn new(config: DormancyConfig) -> Self {
        Self {
            config,
            settle: HashMap::new(),
            dormant_since: HashMap::new(),
            last_tick: None,
        }
    }

    pub fn config(&self) -> &DormancyConfig {
        &self.config
    }

    /// Bodies with a settle countdown in progress (for tests / diagnostics).
    pub fn settling_body_count(&self) -> usize {
        self.settle.len()
    }

    /// Decides deactivations / reactivations for `tick`.
    ///
    /// Deterministic in `(tick, bodies, regions)` and independent of the order
    /// `bodies` arrives in — inputs are processed in ascending entity-id order,
    /// and the per-tick caps therefore drop the same bodies under reordering.
    /// Calling twice for the same `tick` returns an empty plan (dormancy is a
    /// once-per-tick pass).
    pub fn plan(
        &mut self,
        tick: u64,
        bodies: &[BodyDormancyInput],
        regions: &[ActiveRegion],
    ) -> DormancyPlan {
        let mut plan = DormancyPlan::default();
        if self.last_tick == Some(tick) {
            return plan;
        }
        self.last_tick = Some(tick);

        // Forget bodies that no longer exist (split away, mined out).
        let live: std::collections::HashSet<u64> = bodies.iter().map(|b| b.entity.get()).collect();
        self.settle.retain(|e, _| live.contains(e));
        self.dormant_since.retain(|e, _| live.contains(e));

        let mut ordered: Vec<&BodyDormancyInput> = bodies.iter().collect();
        ordered.sort_by_key(|b| b.entity.get());

        for body in ordered {
            let key = body.entity.get();
            let near = regions.iter().any(|r| {
                sphere_gap(body.centre_m, body.radius_m, r.centre_m, r.radius_m)
                    <= self.config.wake_margin_m
            });

            if body.dormant {
                self.settle.remove(&key);
                let since = *self.dormant_since.entry(key).or_insert(tick);
                if body.hard_wake {
                    plan.reactivate.push(body.entity);
                    self.dormant_since.remove(&key);
                } else if near {
                    let held_long_enough =
                        tick.saturating_sub(since) >= self.config.min_dormant_ticks;
                    if !held_long_enough {
                        // Still in the hysteresis window; leave it dormant.
                    } else if plan.reactivate.len() < self.config.max_reactivations_per_tick {
                        plan.reactivate.push(body.entity);
                        self.dormant_since.remove(&key);
                    } else {
                        plan.deferred_reactivations += 1;
                    }
                }
                continue;
            }

            // Awake body.
            if near {
                self.settle.remove(&key);
                plan.kept_awake_nearby += 1;
                continue;
            }
            let still = body.sleeping && body.speed_m_s <= self.config.still_speed_m_s;
            let count = if still {
                let c = self.settle.entry(key).or_insert(0);
                *c += 1;
                *c
            } else {
                self.settle.remove(&key);
                0
            };
            if count >= self.config.settle_ticks {
                if plan.deactivate.len() < self.config.max_deactivations_per_tick {
                    plan.deactivate.push(body.entity);
                    self.settle.remove(&key);
                    self.dormant_since.insert(key, tick);
                } else {
                    plan.deferred_deactivations += 1;
                }
            } else if count > 0 {
                plan.settling += 1;
            }
        }

        plan
    }
}

/// Gap between the surfaces of two spheres, metres. Negative when they overlap.
fn sphere_gap(a_centre: [f64; 3], a_radius: f64, b_centre: [f64; 3], b_radius: f64) -> f64 {
    let dx = a_centre[0] - b_centre[0];
    let dy = a_centre[1] - b_centre[1];
    let dz = a_centre[2] - b_centre[2];
    (dx * dx + dy * dy + dz * dz).sqrt() - a_radius - b_radius
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(n: u64) -> EntityId {
        EntityId::new(n).unwrap()
    }

    fn cfg() -> DormancyConfig {
        DormancyConfig {
            settle_ticks: 5,
            wake_margin_m: 2.0,
            min_dormant_ticks: 3,
            max_deactivations_per_tick: 2,
            max_reactivations_per_tick: 2,
            still_speed_m_s: 0.05,
        }
    }

    fn still_body(n: u64, at: [f64; 3]) -> BodyDormancyInput {
        BodyDormancyInput {
            entity: ent(n),
            centre_m: at,
            radius_m: 0.5,
            sleeping: true,
            speed_m_s: 0.0,
            dormant: false,
            hard_wake: false,
        }
    }

    #[test]
    fn a_body_that_stays_still_and_clear_deactivates_after_settle_ticks() {
        let mut p = DormancyPolicy::new(cfg());
        let bodies = [still_body(1, [50.0, 1.0, 50.0])];
        for tick in 1..5 {
            let plan = p.plan(tick, &bodies, &[]);
            assert!(plan.deactivate.is_empty(), "tick {tick}: not yet");
            assert_eq!(plan.settling, 1);
        }
        let plan = p.plan(5, &bodies, &[]);
        assert_eq!(plan.deactivate, vec![ent(1)]);
    }

    #[test]
    fn a_nearby_active_region_resets_the_settle_countdown() {
        let mut p = DormancyPolicy::new(cfg());
        let bodies = [still_body(1, [50.0, 1.0, 50.0])];
        let player_near = [ActiveRegion {
            centre_m: [51.0, 1.0, 50.0],
            radius_m: 0.4,
        }]; // gap ~0.1 m < wake_margin 2 m
        for tick in 1..20 {
            let plan = p.plan(tick, &bodies, &player_near);
            assert!(plan.deactivate.is_empty());
            assert_eq!(plan.kept_awake_nearby, 1);
        }
        // Player leaves: countdown starts fresh (t20..t23 = 4 ticks) and
        // completes on the 5th.
        for tick in 20..24 {
            assert!(
                p.plan(tick, &bodies, &[]).deactivate.is_empty(),
                "tick {tick}"
            );
        }
        assert_eq!(p.plan(24, &bodies, &[]).deactivate, vec![ent(1)]);
    }

    #[test]
    fn a_moving_body_never_settles() {
        let mut p = DormancyPolicy::new(cfg());
        let mut b = still_body(1, [50.0, 1.0, 50.0]);
        b.sleeping = false;
        b.speed_m_s = 1.0;
        for tick in 1..30 {
            assert!(p.plan(tick, &[b], &[]).deactivate.is_empty(), "tick {tick}");
        }
    }

    #[test]
    fn a_dormant_body_wakes_on_proximity_after_the_hysteresis_window() {
        let mut p = DormancyPolicy::new(cfg());
        let dormant = BodyDormancyInput {
            dormant: true,
            speed_m_s: 0.0,
            ..still_body(1, [50.0, 1.0, 50.0])
        };
        // Went dormant at tick 10.
        p.plan(10, &[dormant], &[]);
        // Player arrives at tick 11 — still inside min_dormant_ticks (3): held.
        let near = [ActiveRegion {
            centre_m: [50.8, 1.0, 50.0],
            radius_m: 0.2,
        }];
        assert!(p.plan(11, &[dormant], &near).reactivate.is_empty());
        assert!(p.plan(12, &[dormant], &near).reactivate.is_empty());
        // tick 13: 13 - 10 >= 3 -> reactivate.
        assert_eq!(p.plan(13, &[dormant], &near).reactivate, vec![ent(1)]);
    }

    #[test]
    fn an_edit_hard_wakes_a_dormant_body_immediately() {
        let mut p = DormancyPolicy::new(cfg());
        let mut dormant = BodyDormancyInput {
            dormant: true,
            speed_m_s: 0.0,
            ..still_body(1, [50.0, 1.0, 50.0])
        };
        p.plan(10, &[dormant], &[]);
        dormant.hard_wake = true;
        // Same tick it went dormant + no nearby region: still reactivates.
        assert_eq!(p.plan(11, &[dormant], &[]).reactivate, vec![ent(1)]);
    }

    #[test]
    fn deactivations_are_capped_and_deterministic_per_tick() {
        let mut p = DormancyPolicy::new(cfg()); // cap = 2
        let bodies: Vec<BodyDormancyInput> = (1..=5)
            .map(|n| still_body(n, [n as f64 * 20.0, 1.0, 1.0]))
            .collect();
        for tick in 1..5 {
            p.plan(tick, &bodies, &[]);
        }
        let a = p.plan(5, &bodies, &[]);
        assert_eq!(a.deactivate, vec![ent(1), ent(2)], "lowest ids first");
        assert_eq!(a.deferred_deactivations, 3);

        // Order independence: a fresh policy fed the reversed list decides the
        // same.
        let mut q = DormancyPolicy::new(cfg());
        let mut rev = bodies.clone();
        rev.reverse();
        for tick in 1..5 {
            q.plan(tick, &rev, &[]);
        }
        let b = q.plan(5, &rev, &[]);
        assert_eq!(a.deactivate, b.deactivate);
    }

    #[test]
    fn replanning_the_same_tick_is_a_no_op() {
        let mut p = DormancyPolicy::new(cfg());
        let bodies = [still_body(1, [50.0, 1.0, 50.0])];
        for tick in 1..=5 {
            p.plan(tick, &bodies, &[]);
        }
        // tick 5 already planned above and returned the deactivate; a second
        // call for tick 5 is empty.
        assert_eq!(p.plan(5, &bodies, &[]), DormancyPlan::default());
    }
}
