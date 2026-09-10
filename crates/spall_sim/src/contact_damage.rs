//! T21 — contact-to-damage intent conversion (increment 1: terrain targets).
//!
//! `docs/architecture.md`: *"Convert qualifying contacts/damage into intents for
//! a later tick; no mutation inside physics callbacks."* A resting body pushes a
//! near-constant `m·g·dt` support impulse through its contacts every step, so a
//! naive "any contact damages the floor" rule would chew a hole under anything
//! standing still. [`ContactDamagePolicy`] is the pure filter between the
//! solver's per-step contact impulses ([`spall_physics::ContactImpulse`],
//! resolved by the integrator into [`ContactEvent`]s) and the bounded
//! [`EditIntent`] stream:
//!
//! * **Threshold** — a contact qualifies only when its normal impulse is several
//!   times the striking body's own resting weight *and* clears an absolute
//!   floor. A body at rest never qualifies; a body that fell a few metres does.
//! * **Per-region cooldown** — after a region takes damage it is immune for
//!   [`ContactDamageConfig::cooldown_ticks`], so a bouncing / rattling body
//!   cannot fracture the same floor spot every tick.
//! * **Bounded admission** — at most [`ContactDamageConfig::max_intents_per_tick`]
//!   damage intents are emitted per tick, world-wide; the rest are dropped and
//!   counted, never queued. Fragment counts and pending jobs stay bounded.
//! * **Recursion guard** — a body created by a topology commit *this* tick is at
//!   its split instant, not a genuine impact; its contacts are ignored this
//!   tick, so a cut cannot trigger a same-tick cascade of further cuts.
//!
//! This module is pure: no `SimWorld`, no physics handles, deterministic given
//! the same `(tick, events)`. [`crate::sim::Simulation::apply_contact_damage`]
//! wires it to the live world.

use std::collections::{HashMap, HashSet};

use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{BrickCoord, GlobalCell, SphereBrush, VolumeId};

use crate::intent::{EditTarget, ExplosionImpulse};

/// Tunables for turning solver contact impulses into terrain-damage intents.
/// Impulses are the accumulated normal impulse over one 60 Hz step, in
/// newton-seconds — the unit [`spall_physics::ContactImpulse`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactDamageConfig {
    /// A contact qualifies only when its normal impulse is at least this
    /// multiple of the striking body's resting support impulse `m·g·dt`. A body
    /// standing on the floor sits near `1`; an impact from a real fall is well
    /// above it. Keeps the rule scale-free across body masses.
    pub impact_ratio: f32,
    /// A qualifying impulse must also clear this absolute floor, newton-seconds,
    /// so a very light body cannot trip the ratio test with a trivial tap.
    pub min_impulse_n_s: f32,
    /// Ticks a damaged region is immune to further contact damage. At 60 ticks/s
    /// the default is a third of a second — long enough that a bouncing body
    /// settles or moves on before it can cut the same spot again.
    pub cooldown_ticks: u64,
    /// Maximum damage intents emitted per tick, world-wide. Excess qualifying
    /// contacts are dropped and counted (`dropped_over_cap`), never queued.
    pub max_intents_per_tick: usize,
    /// Radius, in terrain cells, of the cut a damage intent carves at the
    /// contact point. Clamped to `>= 1`.
    pub brush_radius_cells: i64,
    /// At or above this multiple of the resting support impulse the damage cut
    /// also carries a one-shot detachment impulse along the contact normal
    /// (a hard enough hit knocks material loose, not just craters it).
    pub explosion_ratio: f32,
    /// Fraction of the contact impulse handed to that detachment impulse.
    pub explosion_scale: f64,
}

impl ContactDamageConfig {
    /// Defaults tuned on the T21 fixtures: a stone body resting on terrain sits
    /// at `impact_ratio ~= 1`, a metre-plus fall lands above `6`, and a
    /// several-metre drop clears `explosion_ratio = 20`.
    pub const DEFAULT: Self = Self {
        impact_ratio: 6.0,
        min_impulse_n_s: 40.0,
        cooldown_ticks: 20,
        max_intents_per_tick: 4,
        brush_radius_cells: 2,
        explosion_ratio: 20.0,
        explosion_scale: 0.2,
    };
}

impl Default for ContactDamageConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One solver contact the integrator has resolved to "a dynamic body struck
/// terrain": which terrain volume, where, how hard, and how that compares to the
/// striking body's own weight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactEvent {
    /// The terrain volume taking the damage.
    pub target_volume: VolumeId,
    /// World contact point, metres.
    pub point_m: [f64; 3],
    /// World contact normal (unit), pointing terrain → striking body. Used as
    /// the detachment-impulse direction for a hard enough hit.
    pub normal: [f64; 3],
    /// Accumulated normal impulse over the pair this step, newton-seconds.
    pub impulse_n_s: f32,
    /// The striking body's resting support impulse `m·g·dt`, newton-seconds —
    /// the reference the [`ContactDamageConfig::impact_ratio`] test uses.
    pub resting_impulse_n_s: f32,
    /// `true` if the striking body was created by a topology commit on the
    /// current tick. Such a contact is the split instant, not an impact, and is
    /// ignored this tick (recursion guard).
    pub striker_born_this_tick: bool,
}

/// A terrain-damage cut the policy decided to emit. The integrator wraps it in
/// an [`crate::intent::EditIntent`] with a server-authored request id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlannedDamage {
    /// Always [`EditTarget::Terrain`] in increment 1.
    pub target: EditTarget,
    /// Cut brush at the contact cell, in the terrain volume's cell space.
    pub brush: SphereBrush,
    /// One-shot detachment impulse for a hard hit, else `None`.
    pub explosion: Option<ExplosionImpulse>,
}

/// The outcome of one [`ContactDamagePolicy::plan`] pass — the cuts to emit plus
/// a full accounting of every contact that did not become one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContactDamagePlan {
    /// Cuts to emit this tick, in deterministic region order.
    pub damage: Vec<PlannedDamage>,
    /// Contacts below the impulse threshold (a resting or gentle contact).
    pub suppressed_below_threshold: usize,
    /// Contacts in a region still on cooldown, or a second contact in a region
    /// already damaged this same tick.
    pub suppressed_cooldown: usize,
    /// Contacts whose striking body was born this tick (recursion guard).
    pub suppressed_recursion: usize,
    /// Qualifying contacts dropped because the per-tick cap was reached.
    pub dropped_over_cap: usize,
    /// Qualifying contacts whose brush could not be built (non-finite or
    /// out-of-range contact point).
    pub malformed: usize,
}

impl ContactDamagePlan {
    /// Number of cuts emitted.
    pub fn admitted(&self) -> usize {
        self.damage.len()
    }
}

/// The stateful part: per-region cooldown timers and a one-pass-per-tick guard.
pub struct ContactDamagePolicy {
    config: ContactDamageConfig,
    /// `(volume id, brick) -> first tick the region is eligible again`.
    cooldown_until: HashMap<(u64, BrickCoord), u64>,
    last_tick: Option<u64>,
}

impl ContactDamagePolicy {
    pub fn new(config: ContactDamageConfig) -> Self {
        Self {
            config,
            cooldown_until: HashMap::new(),
            last_tick: None,
        }
    }

    pub fn config(&self) -> &ContactDamageConfig {
        &self.config
    }

    /// Regions currently on cooldown (for tests / diagnostics).
    pub fn cooldown_region_count(&self) -> usize {
        self.cooldown_until.len()
    }

    /// Plans the damage cuts for one tick from the resolved contact events.
    ///
    /// Deterministic in `(tick, cell_m, events)` and independent of the order
    /// `events` arrives in: candidates are sorted by region then descending
    /// impulse, so the hardest hit in a region claims its single slot and the
    /// per-tick cap keeps the same cuts under reordering. Calling twice for the
    /// same `tick` is a no-op returning an empty plan — contact damage is a
    /// once-per-tick pass.
    pub fn plan(&mut self, tick: u64, cell_m: f64, events: &[ContactEvent]) -> ContactDamagePlan {
        let mut plan = ContactDamagePlan::default();
        if self.last_tick == Some(tick) {
            return plan;
        }
        self.last_tick = Some(tick);
        self.cooldown_until.retain(|_, &mut until| until > tick);

        // Canonical order: region (brick) then hardest-first, then input index
        // as a final tiebreak so equal-impulse contacts are still deterministic.
        let mut candidates: Vec<(BrickCoord, usize, &ContactEvent)> = events
            .iter()
            .enumerate()
            .map(|(i, e)| (brick_of(e.point_m, cell_m), i, e))
            .collect();
        candidates.sort_by(|a, b| {
            a.0.sort_key()
                .cmp(&b.0.sort_key())
                .then_with(|| {
                    b.2.impulse_n_s
                        .partial_cmp(&a.2.impulse_n_s)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then(a.1.cmp(&b.1))
        });

        let mut damaged_this_tick: HashSet<(u64, BrickCoord)> = HashSet::new();
        for (brick, _, event) in candidates {
            let region = (event.target_volume.get(), brick);
            let resting = event.resting_impulse_n_s.max(f32::MIN_POSITIVE);

            if event.impulse_n_s < self.config.min_impulse_n_s
                || event.impulse_n_s < self.config.impact_ratio * resting
            {
                plan.suppressed_below_threshold += 1;
                continue;
            }
            if event.striker_born_this_tick {
                plan.suppressed_recursion += 1;
                continue;
            }
            if self.cooldown_until.contains_key(&region) || damaged_this_tick.contains(&region) {
                plan.suppressed_cooldown += 1;
                continue;
            }
            if plan.damage.len() >= self.config.max_intents_per_tick {
                plan.dropped_over_cap += 1;
                continue;
            }
            let Some(brush) = brush_at(event.point_m, cell_m, self.config.brush_radius_cells)
            else {
                plan.malformed += 1;
                continue;
            };

            let explosion = if event.impulse_n_s >= self.config.explosion_ratio * resting {
                Some(ExplosionImpulse {
                    magnitude_ns: f64::from(event.impulse_n_s) * self.config.explosion_scale,
                    direction: event.normal,
                })
            } else {
                None
            };
            plan.damage.push(PlannedDamage {
                target: EditTarget::Terrain,
                brush,
                explosion,
            });
            damaged_this_tick.insert(region);
            self.cooldown_until
                .insert(region, tick.saturating_add(self.config.cooldown_ticks));
        }
        plan
    }
}

/// The whole-cell index nearest a world-metre coordinate.
fn cell_index(v: f64, cell_m: f64) -> Option<i64> {
    if !v.is_finite() || cell_m <= 0.0 {
        return None;
    }
    let c = (v / cell_m).round();
    if c.abs() >= i64::MAX as f64 {
        return None;
    }
    Some(c as i64)
}

/// The brick a world-space contact point falls in, for cooldown keying. A
/// non-finite point collapses to the origin brick; such events are dropped as
/// `malformed` at brush-build time anyway.
fn brick_of(point_m: [f64; 3], cell_m: f64) -> BrickCoord {
    let cell = GlobalCell::new(
        cell_index(point_m[0], cell_m).unwrap_or(0),
        cell_index(point_m[1], cell_m).unwrap_or(0),
        cell_index(point_m[2], cell_m).unwrap_or(0),
    );
    cell.split().0
}

/// A cut brush of `radius_cells` (clamped `>= 1`) centred on the terrain cell
/// nearest `point_m`. `None` if the point is non-finite or so far out that the
/// fixed-point centre would overflow.
fn brush_at(point_m: [f64; 3], cell_m: f64, radius_cells: i64) -> Option<SphereBrush> {
    let cx = cell_index(point_m[0], cell_m)?;
    let cy = cell_index(point_m[1], cell_m)?;
    let cz = cell_index(point_m[2], cell_m)?;
    let half = BRUSH_UNIT / 2;
    let centre = BrushPoint::from_units(
        cx.checked_mul(BRUSH_UNIT)?.checked_add(half)?,
        cy.checked_mul(BRUSH_UNIT)?.checked_add(half)?,
        cz.checked_mul(BRUSH_UNIT)?.checked_add(half)?,
    );
    SphereBrush::new(centre, radius_cells.max(1) * BRUSH_UNIT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_M: f64 = 0.25;

    fn vol() -> VolumeId {
        VolumeId::new(1).unwrap()
    }

    /// A contact `ratio`× the resting load of a `mass_kg` body at `point_m`.
    fn event(point_m: [f64; 3], mass_kg: f32, ratio: f32) -> ContactEvent {
        let resting = mass_kg * 9.81 * (1.0 / 60.0);
        ContactEvent {
            target_volume: vol(),
            point_m,
            normal: [0.0, 1.0, 0.0],
            impulse_n_s: resting * ratio,
            resting_impulse_n_s: resting,
            striker_born_this_tick: false,
        }
    }

    fn policy() -> ContactDamagePolicy {
        ContactDamagePolicy::new(ContactDamageConfig::DEFAULT)
    }

    #[test]
    fn a_hard_impact_becomes_one_damage_cut() {
        let mut p = policy();
        let plan = p.plan(1, CELL_M, &[event([4.0, 1.0, 4.0], 200.0, 12.0)]);
        assert_eq!(plan.admitted(), 1);
        assert_eq!(plan.suppressed_below_threshold, 0);
        assert!(
            plan.damage[0].explosion.is_none(),
            "12x < explosion_ratio 20x"
        );
        assert_eq!(plan.damage[0].target, EditTarget::Terrain);
    }

    #[test]
    fn a_resting_contact_is_below_threshold() {
        let mut p = policy();
        // A body sitting on the floor: impulse ~= its weight support, ratio ~1.
        let plan = p.plan(1, CELL_M, &[event([4.0, 1.0, 4.0], 200.0, 1.2)]);
        assert_eq!(plan.admitted(), 0);
        assert_eq!(plan.suppressed_below_threshold, 1);
    }

    #[test]
    fn the_absolute_floor_gates_a_light_body() {
        let mut p = policy();
        // Ratio 10x clears impact_ratio, but a 0.2 kg body's 10x impulse is
        // ~0.33 N·s — far below min_impulse_n_s.
        let plan = p.plan(1, CELL_M, &[event([4.0, 1.0, 4.0], 0.2, 10.0)]);
        assert_eq!(plan.admitted(), 0);
        assert_eq!(plan.suppressed_below_threshold, 1);
    }

    #[test]
    fn a_very_hard_hit_also_detaches_material() {
        let mut p = policy();
        let plan = p.plan(1, CELL_M, &[event([4.0, 1.0, 4.0], 200.0, 30.0)]);
        assert_eq!(plan.admitted(), 1);
        let x = plan.damage[0]
            .explosion
            .expect("30x >= explosion_ratio 20x");
        assert!(x.magnitude_ns > 0.0);
        assert_eq!(x.direction, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn a_region_on_cooldown_takes_no_further_damage_until_it_expires() {
        let mut p = policy();
        let hit = event([4.0, 1.0, 4.0], 200.0, 12.0);
        assert_eq!(p.plan(1, CELL_M, &[hit]).admitted(), 1);

        // Same region, still inside cooldown_ticks (20): suppressed.
        for tick in 2..=20 {
            let plan = p.plan(tick, CELL_M, &[hit]);
            assert_eq!(plan.admitted(), 0, "tick {tick} inside cooldown");
            assert_eq!(plan.suppressed_cooldown, 1);
        }
        // Cooldown expires: eligible again.
        assert_eq!(p.plan(21, CELL_M, &[hit]).admitted(), 1);
    }

    #[test]
    fn two_contacts_in_one_region_in_one_tick_emit_once() {
        let mut p = policy();
        let a = event([4.0, 1.0, 4.0], 200.0, 12.0);
        let b = event([4.1, 1.0, 4.05], 200.0, 30.0); // same 0.25 m cell/brick
        let plan = p.plan(1, CELL_M, &[a, b]);
        assert_eq!(plan.admitted(), 1);
        assert_eq!(plan.suppressed_cooldown, 1);
        // The harder hit (b, 30x) won the slot, so the cut carries an explosion.
        assert!(plan.damage[0].explosion.is_some());
    }

    #[test]
    fn the_per_tick_cap_drops_excess_qualifying_contacts() {
        let mut p = policy(); // max_intents_per_tick = 4
        // Six qualifying hits in six well-separated bricks (32 cells = 8 m).
        let events: Vec<ContactEvent> = (0..6)
            .map(|i| event([i as f64 * 8.0 + 1.0, 1.0, 1.0], 200.0, 12.0))
            .collect();
        let plan = p.plan(1, CELL_M, &events);
        assert_eq!(plan.admitted(), 4);
        assert_eq!(plan.dropped_over_cap, 2);
    }

    #[test]
    fn a_body_born_this_tick_cannot_trigger_damage() {
        let mut p = policy();
        let mut e = event([4.0, 1.0, 4.0], 200.0, 30.0);
        e.striker_born_this_tick = true;
        let plan = p.plan(1, CELL_M, &[e]);
        assert_eq!(plan.admitted(), 0);
        assert_eq!(plan.suppressed_recursion, 1);
    }

    #[test]
    fn planning_is_independent_of_event_order() {
        let events: Vec<ContactEvent> = vec![
            event([1.0, 1.0, 1.0], 200.0, 12.0),
            event([20.0, 1.0, 1.0], 200.0, 30.0),
            event([1.0, 1.0, 20.0], 200.0, 8.0),
        ];
        let mut forward = policy();
        let a = forward.plan(1, CELL_M, &events);
        let mut reversed = policy();
        let mut rev = events.clone();
        rev.reverse();
        let b = reversed.plan(1, CELL_M, &rev);
        assert_eq!(a.damage, b.damage);
        assert_eq!(a.admitted(), 3);
    }

    #[test]
    fn replanning_the_same_tick_is_a_no_op() {
        let mut p = policy();
        let hit = event([4.0, 1.0, 4.0], 200.0, 12.0);
        assert_eq!(p.plan(7, CELL_M, &[hit]).admitted(), 1);
        let again = p.plan(7, CELL_M, &[hit]);
        assert_eq!(again, ContactDamagePlan::default());
    }

    #[test]
    fn a_non_finite_contact_point_is_counted_malformed_not_panicked() {
        let mut p = policy();
        let mut e = event([f64::NAN, 1.0, 4.0], 200.0, 30.0);
        e.point_m = [f64::INFINITY, 1.0, 4.0];
        let plan = p.plan(1, CELL_M, &[e]);
        assert_eq!(plan.admitted(), 0);
        assert_eq!(plan.malformed, 1);
    }
}
