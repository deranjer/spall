//! Work lanes, priorities, and per-lane admission budgets.
//!
//! The scheduler keeps a separate queue and budget for each [`Lane`] so that
//! background work — world generation, visual meshing — cannot starve or
//! outspend the lanes that carry player-visible edits, collision rebuilds, and
//! structural analysis. Each lane bounds both the number of queued jobs and
//! their total declared cost in bytes, plus how many jobs of that lane may run
//! concurrently.

/// A category of background work with its own queue and budget.
///
/// The five lanes mirror the cost centres called out in `docs/architecture.md`:
/// "Separate priority and byte budgets for generation, edits, collision,
/// topology, and visuals prevent background work starving player actions."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lane {
    /// World generation / procedural fill.
    Generation,
    /// Preparing accepted authoritative edits.
    Edit,
    /// Collider builds and updates.
    Collision,
    /// Structural connectivity / support analysis.
    Topology,
    /// Meshing, lighting, and other render-only work.
    Visual,
}

impl Lane {
    /// Every lane, in a fixed order.
    pub const ALL: [Lane; 5] = [
        Lane::Generation,
        Lane::Edit,
        Lane::Collision,
        Lane::Topology,
        Lane::Visual,
    ];

    /// Dense index into a `[_; 5]` keyed by lane.
    pub const fn index(self) -> usize {
        match self {
            Lane::Generation => 0,
            Lane::Edit => 1,
            Lane::Collision => 2,
            Lane::Topology => 3,
            Lane::Visual => 4,
        }
    }
}

/// Relative urgency of a job within its lane. Higher runs first; ties break by
/// submission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Priority(pub u8);

impl Priority {
    pub const IDLE: Priority = Priority(0);
    pub const LOW: Priority = Priority(64);
    pub const NORMAL: Priority = Priority(128);
    pub const HIGH: Priority = Priority(192);
    pub const CRITICAL: Priority = Priority(255);
}

/// The admission limits for one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneBudget {
    /// Maximum jobs waiting in the lane's queue (not yet dispatched).
    pub max_queued_jobs: u32,
    /// Maximum total `cost_bytes` retained by queued, running, and completed
    /// jobs in this lane. The historical field name is retained for source
    /// compatibility with the T04 configuration API.
    pub max_queued_bytes: u64,
    /// Maximum jobs of this lane running concurrently.
    pub max_in_flight: u32,
    /// Maximum results held pending validation/installation. This gives callers
    /// that stop draining completions explicit backpressure instead of allowing
    /// completed outputs to grow without bound.
    pub max_completed_jobs: u32,
}

impl LaneBudget {
    pub const fn new(max_queued_jobs: u32, max_queued_bytes: u64, max_in_flight: u32) -> Self {
        Self {
            max_queued_jobs,
            max_queued_bytes,
            max_in_flight,
            max_completed_jobs: max_queued_jobs,
        }
    }
}

/// Per-lane budgets for a [`crate::Scheduler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    lanes: [LaneBudget; 5],
}

impl SchedulerConfig {
    /// A config with the same `budget` on every lane.
    pub const fn uniform(budget: LaneBudget) -> Self {
        Self { lanes: [budget; 5] }
    }

    /// Replaces one lane's budget (builder style).
    #[must_use]
    pub const fn with_lane(mut self, lane: Lane, budget: LaneBudget) -> Self {
        self.lanes[lane.index()] = budget;
        self
    }

    pub const fn lane(&self, lane: Lane) -> LaneBudget {
        self.lanes[lane.index()]
    }
}

impl Default for SchedulerConfig {
    /// Small bounded defaults suitable for the G1 all-resident fixture and for
    /// CPU CI. Real deployments override per lane from measured budgets.
    fn default() -> Self {
        Self::uniform(LaneBudget::new(256, 64 * 1024 * 1024, 8))
            .with_lane(Lane::Edit, LaneBudget::new(512, 32 * 1024 * 1024, 16))
            .with_lane(Lane::Collision, LaneBudget::new(256, 64 * 1024 * 1024, 8))
            .with_lane(Lane::Topology, LaneBudget::new(256, 32 * 1024 * 1024, 8))
            .with_lane(Lane::Visual, LaneBudget::new(1024, 128 * 1024 * 1024, 8))
    }
}

/// A snapshot of one lane's live load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LanePressure {
    pub queued_jobs: u32,
    pub queued_bytes: u64,
    pub in_flight_jobs: u32,
    pub in_flight_bytes: u64,
    pub completed_waiting_install: u32,
    pub completed_bytes: u64,
    /// Submissions rejected on this lane since the scheduler was created.
    pub rejected: u64,
}

/// Live load across all lanes, for overload reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pressure {
    lanes: [LanePressure; 5],
}

impl Pressure {
    pub fn lane(&self, lane: Lane) -> LanePressure {
        self.lanes[lane.index()]
    }

    pub(crate) fn lane_mut(&mut self, lane: Lane) -> &mut LanePressure {
        &mut self.lanes[lane.index()]
    }

    /// Total jobs neither installed nor cancelled: queued + running + waiting to
    /// install, across every lane.
    pub fn outstanding_jobs(&self) -> u64 {
        Lane::ALL
            .into_iter()
            .map(|lane| {
                let p = self.lane(lane);
                u64::from(p.queued_jobs)
                    + u64::from(p.in_flight_jobs)
                    + u64::from(p.completed_waiting_install)
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_indices_are_dense_and_distinct() {
        let mut seen = [false; 5];
        for lane in Lane::ALL {
            seen[lane.index()] = true;
        }
        assert_eq!(seen, [true; 5]);
    }

    #[test]
    fn config_builder_overrides_only_the_named_lane() {
        let cfg = SchedulerConfig::uniform(LaneBudget::new(10, 100, 2))
            .with_lane(Lane::Edit, LaneBudget::new(1, 2, 3));
        assert_eq!(cfg.lane(Lane::Edit), LaneBudget::new(1, 2, 3));
        assert_eq!(cfg.lane(Lane::Visual), LaneBudget::new(10, 100, 2));
    }

    #[test]
    fn priority_constants_order_as_expected() {
        assert!(Priority::IDLE < Priority::NORMAL);
        assert!(Priority::NORMAL < Priority::CRITICAL);
    }
}
