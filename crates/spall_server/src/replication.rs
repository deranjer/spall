//! T20 replication scheduling: per-client interest relevance, motion-frequency
//! tiers, and a per-client egress byte budget.
//!
//! `docs/architecture.md`: "Schedule bounded dirty work and update replication
//! interest sets." `docs/protocol.md` ("Network budget and overload behavior"):
//! "Prioritize players, imminent contacts, nearby moving bodies, and recently
//! changed structures; send distant/sleeping bodies less frequently with
//! periodic keyframes." … "Drop superseded unsent motion snapshots" … "Never
//! discard committed topology to meet the motion budget."
//!
//! This module owns only the **motion** side of that contract — deciding, for
//! one client and one 20 Hz batch, which [`MotionSnapshot`]s to send. Committed
//! [`spall_protocol::TopologyTransaction`]s are never filtered or dropped here;
//! the reliable backlog bound (ENG-48) is the only thing that ever sheds them,
//! and only by disconnecting the client so it re-baselines.
//!
//! It is deliberately free of `tokio` / `spall_net` so the decision logic is
//! unit-tested in isolation; [`crate::serve`] adapts the authoritative world
//! into [`BodyDigest`]s and routes the result.

use std::collections::HashMap;

use spall_protocol::MotionSnapshot;

/// Provisional encoded size of one [`MotionSnapshot`] on the wire, in bytes.
///
/// `docs/protocol.md` budget example: "200 relevant bodies x 48 encoded bytes x
/// 20 Hz". The real postcard encoding varies a little with pose/velocity
/// magnitude; this fixed figure is what the per-client budget is accounted in,
/// so the budget is a deterministic snapshot count, not a wire-exact measure
/// (`ServeSummary::transport_egress_bytes` carries the wire-exact total).
pub const MOTION_SNAPSHOT_WIRE_BYTES: usize = 48;

/// Outward hysteresis on the interest boundary: a non-player body that has been
/// relevant stays in the `Far` tier until it is this factor past `far_radius_m`,
/// so a body hovering on the edge does not flap in and out of the replica.
const INTEREST_HYSTERESIS: f64 = 1.25;

/// One client's spatial interest.
#[derive(Debug, Clone, PartialEq)]
pub enum InterestSet {
    /// No spatial filter — every body is relevant (the pre-T20 behaviour, and
    /// the fallback when a scene gives a client no anchor position).
    Global,
    /// A sphere around a point (usually the client's player capsule). A body is
    /// `Near` when its bounds are within `near_radius_m`, `Far` out to
    /// `far_radius_m` (plus hysteresis), and `Excluded` beyond.
    Anchored {
        center_m: [f64; 3],
        near_radius_m: f64,
        far_radius_m: f64,
    },
}

/// How relevant one body is to one client this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    /// Send every batch (20 Hz).
    Near,
    /// Send on the reduced keyframe cadence only.
    Far,
    /// Do not replicate motion for this body to this client.
    Excluded,
}

/// The minimum a client needs to know about one body (or another player) to
/// place it in an interest tier. Built by [`crate::serve`] from the
/// authoritative `SimWorld` each motion batch.
#[derive(Debug, Clone, Copy)]
pub struct BodyDigest {
    /// `EntityId::get()` of the body / player this describes.
    pub key: u64,
    /// World-space centre of the body's collider bounds, metres.
    pub center_m: [f64; 3],
    /// Half-diagonal of the body's collider bounds, metres. A large body is
    /// relevant when its *bounds* reach interest even if its centre is far
    /// (`docs/protocol.md`: "Large bodies are relevant if their bounds intersect
    /// interest, even if their centres are far away").
    pub radius_m: f64,
    /// A player capsule rather than a voxel body. Players are prioritised and
    /// never `Excluded` — a distant player only drops to the `Far` cadence.
    pub is_player: bool,
    /// The body is asleep (settled). Sleeping bodies sort after awake ones when
    /// the byte budget forces a choice.
    pub sleeping: bool,
}

impl InterestSet {
    /// Places `digest` in a tier, given its tier on the previous batch (for the
    /// outward hysteresis on the `Excluded` boundary).
    pub fn classify(&self, digest: &BodyDigest, previous: Option<Relevance>) -> Relevance {
        let InterestSet::Anchored {
            center_m,
            near_radius_m,
            far_radius_m,
        } = self
        else {
            return Relevance::Near;
        };
        let dx = digest.center_m[0] - center_m[0];
        let dy = digest.center_m[1] - center_m[1];
        let dz = digest.center_m[2] - center_m[2];
        // Distance from the interest centre to the *near face* of the body's
        // bounding sphere: a big body counts as close once its shell reaches in.
        let edge_dist = (dx * dx + dy * dy + dz * dz).sqrt() - digest.radius_m.max(0.0);

        if edge_dist <= *near_radius_m {
            Relevance::Near
        } else if edge_dist <= *far_radius_m {
            Relevance::Far
        } else if digest.is_player {
            // A player is never dropped entirely, only slowed to the keyframe
            // cadence.
            Relevance::Far
        } else if matches!(previous, Some(Relevance::Near | Relevance::Far))
            && edge_dist <= *far_radius_m * INTEREST_HYSTERESIS
        {
            Relevance::Far
        } else {
            Relevance::Excluded
        }
    }
}

/// Per-client motion bandwidth policy.
#[derive(Debug, Clone, Copy)]
pub struct MotionBudget {
    /// Send `Far`-tier snapshots only on batches whose index is a multiple of
    /// this. `1` (or `0`) means every batch — no cadence reduction.
    pub far_interval: u64,
    /// Byte ceiling for one client's motion in one batch, accounted at
    /// [`MOTION_SNAPSHOT_WIRE_BYTES`] each. `0` disables the ceiling.
    pub per_batch_bytes: usize,
}

impl MotionBudget {
    /// No cadence reduction and no byte ceiling — the pre-T20 behaviour.
    pub const UNLIMITED: MotionBudget = MotionBudget {
        far_interval: 1,
        per_batch_bytes: 0,
    };

    fn far_due(&self, batch_index: u64) -> bool {
        self.far_interval <= 1 || batch_index.is_multiple_of(self.far_interval)
    }
}

/// What one [`ClientReplication::select`] pass decided.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SelectOutcome {
    /// Snapshots kept for this client this batch.
    pub kept: u64,
    /// Snapshots for bodies outside this client's interest (`Excluded`).
    pub interest_culled: u64,
    /// Snapshots withheld this batch by the byte ceiling or the `Far` cadence —
    /// motion is superseded by the next batch, so this is a bandwidth measure,
    /// not lost geometry.
    pub budget_deferred: u64,
    /// Accounted bytes of the kept set (`kept * MOTION_SNAPSHOT_WIRE_BYTES`).
    pub bytes: usize,
}

/// One connected client's replication state: its interest set plus the per-body
/// tier from the previous batch (for hysteresis) and running counters.
#[derive(Debug, Clone)]
pub struct ClientReplication {
    interest: InterestSet,
    last_tier: HashMap<u64, Relevance>,
    last: SelectOutcome,
    totals: SelectOutcome,
    max_batch_bytes: usize,
}

impl ClientReplication {
    /// A client with no spatial filter yet.
    pub fn new() -> Self {
        Self {
            interest: InterestSet::Global,
            last_tier: HashMap::new(),
            last: SelectOutcome::default(),
            totals: SelectOutcome::default(),
            max_batch_bytes: 0,
        }
    }

    /// Replaces the interest set (called each batch with the client's current
    /// anchor). Passing [`InterestSet::Global`] restores unfiltered replication.
    pub fn set_interest(&mut self, interest: InterestSet) {
        if interest != self.interest {
            self.interest = interest;
        }
    }

    /// The current interest set.
    pub fn interest(&self) -> &InterestSet {
        &self.interest
    }

    /// Running totals over every batch this client has been sent.
    pub fn totals(&self) -> SelectOutcome {
        self.totals
    }

    /// The outcome of the most recent [`Self::select`] call.
    pub fn last_outcome(&self) -> SelectOutcome {
        self.last
    }

    /// Largest single-batch accounted byte size this client has been sent.
    pub fn max_batch_bytes(&self) -> usize {
        self.max_batch_bytes
    }

    /// Chooses which of `snaps` to send this client for batch `batch_index`.
    ///
    /// `digests` maps `MotionSnapshot::body.get()` to the body/player digest; a
    /// snapshot with no digest is treated as `Global`-relevant (kept) so a
    /// missing digest can never silently drop motion.
    ///
    /// Ordering of the kept set: this client's own player first, then other
    /// players, then awake near bodies, then sleeping near bodies, then far
    /// bodies — so when the byte ceiling bites it sheds the least important
    /// motion first. Within a group the input order (ascending `snapshot_seq`)
    /// is preserved.
    pub fn select(
        &mut self,
        batch_index: u64,
        own_player_key: Option<u64>,
        snaps: &[MotionSnapshot],
        digests: &HashMap<u64, BodyDigest>,
        budget: &MotionBudget,
    ) -> Vec<MotionSnapshot> {
        let far_due = budget.far_due(batch_index);
        let mut outcome = SelectOutcome::default();
        let mut fresh_tiers: HashMap<u64, Relevance> = HashMap::with_capacity(snaps.len());

        // priority: 0 own player, 1 other player, 2 near awake, 3 near sleeping,
        // 4 far. Lower is kept first under the byte ceiling.
        let mut candidates: Vec<(u8, usize)> = Vec::with_capacity(snaps.len());
        for (idx, snap) in snaps.iter().enumerate() {
            let key = snap.body.get();
            let Some(digest) = digests.get(&key) else {
                // Unknown body — keep it (fail open), lowest priority.
                candidates.push((4, idx));
                continue;
            };
            let tier = self
                .interest
                .classify(digest, self.last_tier.get(&key).copied());
            fresh_tiers.insert(key, tier);
            match tier {
                Relevance::Excluded => {
                    outcome.interest_culled += 1;
                    continue;
                }
                Relevance::Far if !far_due => {
                    outcome.budget_deferred += 1;
                    continue;
                }
                _ => {}
            }
            let priority = if Some(key) == own_player_key {
                0
            } else if digest.is_player {
                1
            } else {
                match tier {
                    Relevance::Near if !digest.sleeping => 2,
                    Relevance::Near => 3,
                    _ => 4,
                }
            };
            candidates.push((priority, idx));
        }
        self.last_tier = fresh_tiers;

        // Stable sort by priority; `sort_by_key` keeps equal-priority entries in
        // their original (ascending `snapshot_seq`) order.
        candidates.sort_by_key(|(priority, _)| *priority);

        let mut kept = Vec::with_capacity(candidates.len());
        let mut bytes = 0usize;
        for (_, idx) in candidates {
            if budget.per_batch_bytes > 0
                && bytes + MOTION_SNAPSHOT_WIRE_BYTES > budget.per_batch_bytes
            {
                outcome.budget_deferred += 1;
                continue;
            }
            bytes += MOTION_SNAPSHOT_WIRE_BYTES;
            kept.push(snaps[idx]);
        }
        outcome.kept = kept.len() as u64;
        outcome.bytes = bytes;
        self.last = outcome;

        self.totals.kept += outcome.kept;
        self.totals.interest_culled += outcome.interest_culled;
        self.totals.budget_deferred += outcome.budget_deferred;
        self.totals.bytes += outcome.bytes;
        self.max_batch_bytes = self.max_batch_bytes.max(bytes);

        kept
    }
}

impl Default for ClientReplication {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{EntityId, Revision, Tick};
    use spall_protocol::{InputSeq, SnapshotSeq};
    use spall_sim::BodyPose;

    fn digest(
        key: u64,
        center: [f64; 3],
        radius: f64,
        is_player: bool,
        sleeping: bool,
    ) -> BodyDigest {
        BodyDigest {
            key,
            center_m: center,
            radius_m: radius,
            is_player,
            sleeping,
        }
    }

    fn snap(body: u64, seq: u64) -> MotionSnapshot {
        MotionSnapshot {
            server_tick: Tick(1),
            snapshot_seq: SnapshotSeq(seq),
            acked_input: InputSeq(0),
            body: EntityId::new(body).unwrap(),
            topology_revision: Revision::ZERO,
            pose: BodyPose::identity().to_protocol(),
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: false,
        }
    }

    fn digest_map(items: &[BodyDigest]) -> HashMap<u64, BodyDigest> {
        items.iter().map(|d| (d.key, *d)).collect()
    }

    #[test]
    fn global_interest_keeps_every_snapshot() {
        let mut repl = ClientReplication::new();
        let snaps = [snap(1, 0), snap(2, 1), snap(3, 2)];
        let digests = digest_map(&[
            digest(1, [100.0, 0.0, 0.0], 0.5, false, false),
            digest(2, [0.0, 0.0, 0.0], 0.5, false, true),
            digest(3, [0.0, 500.0, 0.0], 0.5, true, false),
        ]);
        let kept = repl.select(0, None, &snaps, &digests, &MotionBudget::UNLIMITED);
        assert_eq!(kept.len(), 3);
        assert_eq!(repl.totals().interest_culled, 0);
        assert_eq!(repl.totals().budget_deferred, 0);
    }

    #[test]
    fn anchored_interest_excludes_a_distant_body_but_never_a_player() {
        let mut repl = ClientReplication::new();
        repl.set_interest(InterestSet::Anchored {
            center_m: [0.0, 0.0, 0.0],
            near_radius_m: 10.0,
            far_radius_m: 30.0,
        });
        let snaps = [snap(1, 0), snap(2, 1), snap(3, 2)];
        let digests = digest_map(&[
            digest(1, [5.0, 0.0, 0.0], 0.5, false, false), // near body
            digest(2, [200.0, 0.0, 0.0], 0.5, false, false), // far body -> excluded
            digest(3, [200.0, 0.0, 0.0], 0.5, true, false), // far player -> Far, kept
        ]);
        let kept = repl.select(0, None, &snaps, &digests, &MotionBudget::UNLIMITED);
        let mut kept_bodies: Vec<u64> = kept.iter().map(|s| s.body.get()).collect();
        kept_bodies.sort_unstable();
        assert_eq!(
            kept_bodies,
            vec![1, 3],
            "near body + far player kept, far body dropped"
        );
        // The player (priority 1) is ordered ahead of the near body (priority 2).
        assert_eq!(kept.first().map(|s| s.body.get()), Some(3));
        assert_eq!(repl.totals().interest_culled, 1);
    }

    #[test]
    fn large_body_bounds_reach_interest_even_with_a_far_centre() {
        let mut repl = ClientReplication::new();
        repl.set_interest(InterestSet::Anchored {
            center_m: [0.0, 0.0, 0.0],
            near_radius_m: 8.0,
            far_radius_m: 12.0,
        });
        // Centre 40 m away but a 35 m half-diagonal: near face is 5 m out -> Near.
        let digests = digest_map(&[digest(1, [40.0, 0.0, 0.0], 35.0, false, false)]);
        repl.select(0, None, &[snap(1, 0)], &digests, &MotionBudget::UNLIMITED);
        assert_eq!(repl.totals().kept, 1);
        assert_eq!(repl.totals().interest_culled, 0);
    }

    #[test]
    fn far_cadence_defers_far_bodies_between_keyframes() {
        let mut repl = ClientReplication::new();
        repl.set_interest(InterestSet::Anchored {
            center_m: [0.0, 0.0, 0.0],
            near_radius_m: 5.0,
            far_radius_m: 100.0,
        });
        let digests = digest_map(&[digest(1, [50.0, 0.0, 0.0], 0.5, false, false)]);
        let budget = MotionBudget {
            far_interval: 4,
            per_batch_bytes: 0,
        };
        // Batch 0: keyframe, sent. Batches 1-3: deferred. Batch 4: sent again.
        assert_eq!(
            repl.select(0, None, &[snap(1, 0)], &digests, &budget).len(),
            1
        );
        assert_eq!(
            repl.select(1, None, &[snap(1, 0)], &digests, &budget).len(),
            0
        );
        assert_eq!(
            repl.select(2, None, &[snap(1, 0)], &digests, &budget).len(),
            0
        );
        assert_eq!(
            repl.select(3, None, &[snap(1, 0)], &digests, &budget).len(),
            0
        );
        assert_eq!(
            repl.select(4, None, &[snap(1, 0)], &digests, &budget).len(),
            1
        );
        assert_eq!(repl.totals().budget_deferred, 3);
    }

    #[test]
    fn byte_ceiling_sheds_lowest_priority_first() {
        let mut repl = ClientReplication::new();
        // Budget for exactly 2 snapshots.
        let budget = MotionBudget {
            far_interval: 1,
            per_batch_bytes: 2 * MOTION_SNAPSHOT_WIRE_BYTES,
        };
        let snaps = [snap(10, 0), snap(11, 1), snap(12, 2), snap(13, 3)];
        let digests = digest_map(&[
            digest(10, [0.0, 0.0, 0.0], 0.5, false, true), // near sleeping body (pri 3)
            digest(11, [0.0, 0.0, 0.0], 0.5, false, false), // near awake body  (pri 2)
            digest(12, [0.0, 0.0, 0.0], 0.5, true, false), // other player     (pri 1)
            digest(13, [0.0, 0.0, 0.0], 0.5, true, false), // own player       (pri 0)
        ]);
        let kept = repl.select(0, Some(13), &snaps, &digests, &budget);
        let kept_bodies: Vec<u64> = kept.iter().map(|s| s.body.get()).collect();
        assert_eq!(kept_bodies, vec![13, 12], "own player then other player");
        assert_eq!(repl.totals().budget_deferred, 2);
        assert_eq!(repl.max_batch_bytes(), 2 * MOTION_SNAPSHOT_WIRE_BYTES);
    }

    #[test]
    fn hysteresis_keeps_a_body_that_drifts_just_past_the_far_edge() {
        let mut repl = ClientReplication::new();
        repl.set_interest(InterestSet::Anchored {
            center_m: [0.0, 0.0, 0.0],
            near_radius_m: 5.0,
            far_radius_m: 10.0,
        });
        let digests = digest_map(&[digest(1, [8.0, 0.0, 0.0], 0.0, false, false)]);
        // First batch: inside far_radius -> Far, kept.
        assert_eq!(
            repl.select(0, None, &[snap(1, 0)], &digests, &MotionBudget::UNLIMITED)
                .len(),
            1
        );
        // Drift to 11 m: past far_radius but within the 1.25x hysteresis band and
        // previously relevant -> still Far.
        let drifted = digest_map(&[digest(1, [11.0, 0.0, 0.0], 0.0, false, false)]);
        assert_eq!(
            repl.select(1, None, &[snap(1, 0)], &drifted, &MotionBudget::UNLIMITED)
                .len(),
            1
        );
        // Jump to 20 m: well past the band -> excluded.
        let gone = digest_map(&[digest(1, [20.0, 0.0, 0.0], 0.0, false, false)]);
        assert_eq!(
            repl.select(2, None, &[snap(1, 0)], &gone, &MotionBudget::UNLIMITED)
                .len(),
            0
        );
        assert_eq!(repl.totals().interest_culled, 1);
    }
}
