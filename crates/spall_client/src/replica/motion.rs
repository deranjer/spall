//! Per-body interpolation history and the render clock (moved out of `replica.rs`).
use super::*;

/// One observed motion state for a body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct MotionState {
    pub(super) server_tick: u64,
    pub(super) topology_revision: Revision,
    pub(super) pose: Pose,
    pub(super) linear_velocity: [f32; 3],
    pub(super) angular_velocity: [f32; 3],
    pub(super) sleeping: bool,
}

impl From<&MotionSnapshot> for MotionState {
    fn from(s: &MotionSnapshot) -> Self {
        Self {
            server_tick: s.server_tick.get(),
            topology_revision: s.topology_revision,
            pose: s.pose,
            linear_velocity: s.linear_velocity,
            angular_velocity: s.angular_velocity,
            sleeping: s.sleeping,
        }
    }
}

/// States retained per body for interpolation. At the 20 Hz publish rate
/// (3 server ticks apart) this covers well over the 100 ms render delay plus
/// jitter, so the delayed target time always has a bracketing pair.
pub(super) const MOTION_HISTORY: usize = 8;

/// Interpolation history for one replicated body: the most recent accepted
/// states (oldest first), plus the first state ever accepted so a caller can
/// measure how far the body has actually travelled.
#[derive(Debug, Clone, Default)]
pub struct MotionTrack {
    first: Option<MotionState>,
    history: VecDeque<MotionState>,
    pub(super) latest: Option<MotionState>,
}

impl MotionTrack {
    pub(super) fn accept(&mut self, state: MotionState) {
        match self.latest {
            Some(cur) if state.server_tick <= cur.server_tick => {}
            _ => {
                if self.first.is_none() {
                    self.first = Some(state);
                }
                if self.history.len() == MOTION_HISTORY {
                    self.history.pop_front();
                }
                self.history.push_back(state);
                self.latest = Some(state);
            }
        }
    }

    /// Pose at fractional server tick `target`: interpolated between the two
    /// accepted states bracketing it, held at the oldest retained state if
    /// `target` precedes them all, and extrapolated up to `max_extra_ticks`
    /// past the newest.
    pub(super) fn sample(&self, target: f64, max_extra_ticks: f64) -> Option<Pose> {
        self.sample_about(target, max_extra_ticks, [0.0; 3])
    }

    /// [`Self::sample`], interpolating the point `pivot_m` (body-local metres,
    /// see [`pivot_world`]) instead of the body origin.
    pub(super) fn sample_about(
        &self,
        target: f64,
        max_extra_ticks: f64,
        pivot_m: [f64; 3],
    ) -> Option<Pose> {
        let latest = self.latest?;
        let idx = self
            .history
            .iter()
            .position(|s| s.server_tick as f64 > target);
        let (a, b) = match idx {
            // Every state is at or before `target`: extrapolate off the newest pair.
            None => match self
                .history
                .len()
                .checked_sub(2)
                .and_then(|i| self.history.get(i))
            {
                Some(&prev) => (prev, latest),
                None => return Some(latest.pose),
            },
            Some(0) => return Some(self.history[0].pose),
            Some(i) => (self.history[i - 1], self.history[i]),
        };
        let span = (b.server_tick - a.server_tick) as f64;
        let t = ((target - a.server_tick as f64) / span).clamp(0.0, 1.0 + max_extra_ticks / span);
        Some(lerp_pose_about(&a.pose, &b.pose, t, pivot_m))
    }

    /// The newest accepted server tick, if any.
    pub fn latest_tick(&self) -> Option<u64> {
        self.latest.map(|s| s.server_tick)
    }

    /// Straight-line distance (metres) between the first and newest accepted
    /// body positions. `0.0` until at least one state has been seen — a body
    /// that only ever reports one stationary pose therefore measures `0.0`.
    pub(super) fn displacement_from_start_m(&self) -> f64 {
        match (self.first, self.latest) {
            (Some(a), Some(b)) => {
                let (p, q) = (a.pose.translation_m, b.pose.translation_m);
                let dx = q[0] - p[0];
                let dy = q[1] - p[1];
                let dz = q[2] - p[2];
                (dx * dx + dy * dy + dz * dz).sqrt()
            }
            _ => 0.0,
        }
    }
}

/// Owned copy of one body's motion history plus the sampling parameters, see
/// [`ReplicaWorld::body_sampler`].
#[derive(Debug, Clone)]
pub struct BodySampler {
    pub(super) track: MotionTrack,
    pub(super) delay_ticks: f64,
    pub(super) max_extra_ticks: f64,
    pub(super) hz: f64,
}

impl BodySampler {
    /// Server ticks per second, for advancing a sampled `render_tick`.
    pub fn hz(&self) -> f64 {
        self.hz
    }

    /// Whether the newest snapshot says the body is awake and moving.
    pub fn is_moving(&self) -> bool {
        self.track.latest.is_some_and(|s| {
            !s.sleeping && s.linear_velocity.iter().map(|v| v * v).sum::<f32>() > 0.0025
        })
    }

    /// Age (server ticks) of the newest snapshot at `render_tick`.
    pub fn latest_age_ticks(&self, render_tick: f64) -> Option<f64> {
        Some(render_tick - self.track.latest?.server_tick as f64)
    }

    /// Same result as [`ReplicaWorld::presented_pose`] at `render_tick`.
    pub fn presented(&self, render_tick: f64, focus_m: Option<[f64; 3]>) -> Option<Pose> {
        self.presented_about(render_tick, focus_m, [0.0; 3])
    }

    /// [`Self::presented`] for a body whose geometry sits far from its pose
    /// origin. A body's local frame is the world cell grid, so a felled tree's
    /// origin can be 100 m from the tree. Interpolating or extrapolating the
    /// *origin* then swings the tree by `radius * angle^2 / 8` per snapshot
    /// interval (and extrapolation with the centre-of-mass velocity is wrong by
    /// `angular_velocity x lever_arm`). `pivot_m` is a body-local point near
    /// the geometry's centre of mass; the motion is applied to that point.
    pub fn presented_about(
        &self,
        render_tick: f64,
        focus_m: Option<[f64; 3]>,
        pivot_m: [f64; 3],
    ) -> Option<Pose> {
        let delayed = self.track.sample_about(
            render_tick - self.delay_ticks,
            self.max_extra_ticks,
            pivot_m,
        )?;
        let Some(focus) = focus_m else {
            return Some(delayed);
        };
        let latest = self.track.latest?;
        let present = advance_pose_about(
            &latest.pose,
            latest.linear_velocity,
            latest.angular_velocity,
            latest.sleeping,
            (render_tick - latest.server_tick as f64).clamp(0.0, self.max_extra_ticks),
            self.hz,
            pivot_m,
        );
        let d = pivot_world(&present, pivot_m)
            .iter()
            .zip(focus)
            .map(|(p, f)| (p - f) * (p - f))
            .sum::<f64>()
            .sqrt();
        let x = ((d - ReplicaWorld::PRESENT_FULL_WITHIN_M)
            / (ReplicaWorld::PRESENT_NONE_BEYOND_M - ReplicaWorld::PRESENT_FULL_WITHIN_M))
            .clamp(0.0, 1.0);
        let smooth = x * x * (3.0 - 2.0 * x);
        Some(lerp_pose_about(&present, &delayed, smooth, pivot_m))
    }
}

/// A body's newest snapshot, see [`ReplicaWorld::latest_motion`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct LatestMotion {
    pub pose: Pose,
    pub server_tick: u64,
    pub linear_velocity_m_s: [f32; 3],
    pub angular_velocity_rad_s: [f32; 3],
    pub sleeping: bool,
}

/// Free-running render clock in fractional server ticks. It advances with wall
/// time and is slewed (a few percent of real time) toward "newest received tick
/// plus half a snapshot interval", so per-packet arrival jitter averages out
/// instead of showing up as motion stutter. It is never allowed to run backwards
/// and only snaps when it is wildly off (join, stall, or reconnect).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RenderClock {
    last: Option<Instant>,
    ticks: f64,
}

impl RenderClock {
    /// Half of the default 3-tick publish interval: the average age of the
    /// newest snapshot at an arbitrary instant.
    pub(super) const TARGET_LEAD_TICKS: f64 = 1.5;
    pub(super) const MAX_SLEW: f64 = 0.05;
    pub(super) const SNAP_ERROR_TICKS: f64 = 12.0;

    pub(super) fn advance(&mut self, now: Instant, newest_tick: u64, hz: f64) -> f64 {
        let target = newest_tick as f64 + Self::TARGET_LEAD_TICKS;
        let Some(last) = self.last else {
            self.last = Some(now);
            self.ticks = target;
            return self.ticks;
        };
        let dt_ticks = now.saturating_duration_since(last).as_secs_f64() * hz;
        self.last = Some(last.max(now));
        let error = target - (self.ticks + dt_ticks);
        if error.abs() > Self::SNAP_ERROR_TICKS {
            self.ticks = target;
        } else {
            let slew = (error * 0.1).clamp(-Self::MAX_SLEW, Self::MAX_SLEW) * dt_ticks;
            self.ticks += dt_ticks + slew;
        }
        self.ticks
    }
}
