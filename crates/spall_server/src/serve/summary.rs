use super::*;

/// Machine-readable result of a [`serve`] run.
#[derive(Debug, Clone, Serialize)]
pub struct ServeSummary {
    pub version: u32,
    pub result: String,
    pub scene: String,
    pub bound_addr: String,
    pub ticks_run: u64,
    pub clients_connected: usize,
    pub transactions_committed: u64,
    pub actions_rejected: u64,
    /// Per-run intent admission and pipeline pressure, including queue-full rejections.
    pub intent_stats: IntentStats,
    pub final_world_hash: String,
    pub total_solid_cells: u64,
    pub body_count: usize,
    /// Largest number of distinct bricks any single detached (non-terrain) body
    /// occupied at end of run. `>= 2` means a body's cells were owned across a
    /// brick boundary — the cross-brick ownership-transfer signal for T11.
    pub max_detached_body_brick_span: u64,
    /// ENG-61: largest linear speed (m/s) of any detached body at end of run.
    /// `0.0` when there is no detached body.
    pub detached_body_max_final_speed_m_s: f64,
    /// ENG-61: every detached body was asleep — at rest — at end of run. `true`
    /// when there is no detached body.
    pub detached_bodies_all_asleep: bool,
    /// ENG-61: consecutive final ticks over which no detached body's origin
    /// moved more than 1 mm on any axis — "vertical position stable for N ticks".
    pub detached_body_stable_ticks: u64,
    /// ENG-61: deepest contact penetration (m) anywhere in the physics world at
    /// end of run. A body that came to rest *clipped through* the remaining
    /// floor rather than on top of it shows up here.
    pub max_contact_penetration_m: f64,
    /// ENG-61: lowest detached-body origin Y (m) at end of run. A body that
    /// free-fell instead of settling leaves this far below the floor. `0.0` when
    /// there is no detached body.
    pub detached_body_min_origin_y_m: f64,
    /// Engine checkpoints published this run (T16). `0` when `--save` is unset.
    pub checkpoints_published: u64,
    /// Topology journal records written this run.
    pub journal_records_written: u64,
    /// Measured mean journal payload bytes per durable journal commit.
    pub persist_bytes_per_write: f64,
    /// Measured durable payload bytes per second of `COMMIT` time.
    pub persist_commit_bytes_per_sec: f64,
    /// T17: late-join baselines that reached `BaselineAck` and went live.
    pub late_joins_completed: u64,
    /// T17: late-join transfers cancelled + re-captured on catch-up overflow.
    pub late_join_retries: u64,
    /// T17: joining clients dropped after exhausting the retry budget.
    pub late_joins_failed: u64,
    /// T17: `ActionRequest`s rejected because their session generation was
    /// superseded by a reconnect.
    pub expired_actions_rejected: u64,
    /// T17: total baseline bulk payload bytes pushed this run.
    pub baseline_bytes_sent: u64,
    /// Repair requests left unanswered because the client's patch queue was full (see
    /// `MAX_QUEUED_REPAIRS_PER_CLIENT`); the client asks again after its backoff.
    pub repairs_deferred_queue_full: u64,
    /// ENG-48: `ActionRequest`s bounced with a throttled rejection because their
    /// session exceeded [`MAX_ACTIONS_PER_CLIENT_PER_TICK`] this tick.
    pub inbound_actions_throttled: u64,
    /// ENG-48: `RepairRequest`s dropped because their session exceeded
    /// [`MAX_REPAIRS_PER_CLIENT_PER_TICK`] this tick.
    pub inbound_repairs_throttled: u64,
    /// Records dropped because the reader-to-sim bridge was full (see `InboundDrops`).
    pub inbound_dropped_actions: u64,
    pub inbound_dropped_repairs: u64,
    pub inbound_dropped_progression: u64,
    pub inbound_dropped_admin: u64,
    /// Water update batches queued (historical field name); a batch may contain
    /// full-frame chunks or changed-brick deltas.
    pub water_keyframes_sent: u64,
    pub water_bytes_queued: u64,
    /// Admin world resets performed.
    pub world_resets: u64,
    /// Edits accepted but not yet resolved when a world reset replaced the world; each was
    /// answered with a retryable "overloaded: world reset" rejection.
    pub actions_discarded_by_reset: u64,
    /// Ticks run past `max_ticks` to let regional clients finish receiving what the server owed
    /// them, and whether the drain limit ended the run with one still receiving.
    pub drain_ticks: u64,
    pub drain_timed_out: bool,
    /// T20: motion snapshots actually sent this run, summed over every client
    /// and batch. With `motion_interest` unset this is `batches * bodies *
    /// clients`; with it set, interest-culled and cadence-deferred snapshots are
    /// not counted here.
    pub motion_snapshots_sent: u64,
    /// T20: motion snapshots withheld because the body was outside a client's
    /// interest set (`Relevance::Excluded`). `0` when `motion_interest` is unset.
    pub motion_snapshots_interest_culled: u64,
    /// T20: motion snapshots deferred to a later batch by the `Far` cadence or
    /// the per-client byte ceiling — motion is superseded, so this is a
    /// bandwidth measure, not lost geometry. `0` when `motion_interest` is unset.
    pub motion_snapshots_budget_deferred: u64,
    /// T20: largest single-batch motion payload sent to one client, accounted
    /// bytes ([`crate::MOTION_SNAPSHOT_WIRE_BYTES`] each).
    pub max_client_motion_batch_bytes: u64,
    /// T20: total **application** bytes this host sent across every client
    /// connection (control records + datagrams + bulk parts, excluding QUIC
    /// framing) — `spall_net`'s own `app_bytes_sent` counter.
    pub app_egress_bytes: u64,
    /// T20: total **transport** bytes this host sent across every client
    /// connection — Quinn's `udp_tx.bytes`, i.e. actual UDP payload on the wire
    /// including QUIC framing and retransmission.
    pub transport_egress_bytes: u64,
    /// T11a / ENG-62: inbound `ActionRequest`s received this run, retries
    /// included ("requested" in the gate's requested / rejected / queued /
    /// committed breakdown). "rejected" is [`Self::actions_rejected`];
    /// "committed" is [`Self::transactions_committed`].
    pub actions_requested: u64,
    /// T11a / ENG-62: `ActionRequest`s the simulation accepted for staging.
    pub actions_staged: u64,
    /// T11a / ENG-62: staged requests that had not committed when the run ended
    /// ("queued" in the gate breakdown).
    pub actions_queued_unresolved: u64,
    /// T11a / ENG-62: server commit latency — admission to commit — as a
    /// nearest-rank p95 (ms) per commit shape, with the sample count behind each
    /// figure. Gate targets (`docs/validation.md` "G1"): single-brick commit
    /// p95 `<= 100 ms`, ordinary structure split `<= 500 ms`, designated
    /// large-collapse `<= 2000 ms`. A bucket with `0` samples was not exercised.
    pub single_brick_commit_p95_ms: f64,
    pub single_brick_commit_samples: u64,
    pub structure_split_p95_ms: f64,
    pub structure_split_samples: u64,
    pub large_collapse_p95_ms: f64,
    pub large_collapse_samples: u64,
    /// T23 / G3 row 7, slice D: residency pass activity. All `0` when
    /// `ServeConfig.residency` is `None` (the default).
    pub residency_evictions_total: u64,
    pub residency_reloads_total: u64,
    /// Fewest / most / final resident terrain bricks the pass observed.
    pub resident_terrain_bricks_min: u64,
    pub resident_terrain_bricks_max: u64,
    pub resident_terrain_bricks_final: u64,
    /// Ticks the resident-brick count exceeded `budget_bricks` (a player's
    /// interest set is larger than the declared budget).
    pub residency_budget_miss_ticks: u64,
    /// T23 / G3 row 7 item 3: fewest / most / final total resident
    /// dense-material bytes across the whole world (terrain + every body
    /// volume), sampled every tick the residency pass runs
    /// ([`spall_server::residency_pass::total_resident_dense_bytes`]). All `0`
    /// when `ServeConfig.residency` is `None`, matching every other residency
    /// counter's convention.
    pub resident_dense_bytes_min: u64,
    pub resident_dense_bytes_max: u64,
    pub resident_dense_bytes_final: u64,
    /// T23 / G3 row 7 item 3: the residency backing's on-disk footprint in
    /// bytes -- the durable-side counterpart to `resident_dense_bytes_*`.
    /// `Some(_)` only when `ServeConfig.residency_disk_path` installed a real
    /// disk-backed store; `None` for the in-process `MemoryBacking` default
    /// and when residency is off.
    pub residency_backing_disk_bytes: Option<u64>,
    /// T23 / G3 row 14: **per-connection** application/transport egress — the
    /// aggregate `app_egress_bytes` / `transport_egress_bytes` above prove
    /// total bandwidth is bounded, but not that `motion_interest` actually
    /// separates load *between* clients. One row per connection that was ever
    /// accepted this run (order not meaningful; sorted by slot).
    pub per_client_egress: Vec<PerClientEgress>,
    /// T21 / ENG-28 increment 4: contact-damage cuts admitted into the edit
    /// pipeline this run. `0` when `ServeConfig.contact_damage` is `None`.
    pub contact_damage_cuts_submitted: u64,
    /// T21 / ENG-28 increment 4: contact-damage cuts the pipeline refused
    /// (queue full — bounded backpressure, not an error). `0` when
    /// `ServeConfig.contact_damage` is `None`.
    pub contact_damage_cuts_rejected: u64,
    /// T21 / ENG-28 increment 4: bodies the dormancy pass deactivated this
    /// run. `0` when `ServeConfig.dormancy` is `None`.
    pub dormancy_deactivations_total: u64,
    /// T21 / ENG-28 increment 4: bodies the dormancy pass reactivated this
    /// run (proximity or a hard-wake edit). `0` when `ServeConfig.dormancy` is
    /// `None`.
    pub dormancy_reactivations_total: u64,
    /// ENG-30 row 7 increment 13: largest pin-set size the residency pass
    /// observed in one tick (pending-edit dependencies, swept-collision
    /// footprints, and pipeline reload grace, unioned). `0` when residency is
    /// off.
    pub residency_pinned_bricks_max: u64,
    /// ENG-30 row 7 increment 13: interest-driven (non-required) reloads
    /// skipped because admitting them would have exceeded `budget_bricks` or
    /// the new dense-byte cap — the "loads happen before an `over_budget`
    /// count" gap the coordinator review named is now a real, counted
    /// deferral. `0` when residency is off.
    pub residency_admission_deferred_total: u64,
    /// ENG-30 row 7 increment 13: ticks where the *required* (interest ∪
    /// pinned) set alone exceeded `budget_bricks` — genuine capacity
    /// pressure the pass could not resolve by evicting only optional
    /// geometry. `0` when residency is off.
    pub residency_required_over_budget_ticks: u64,
    /// ENG-30 row 7 increment 13: approximate bytes of retained evicted-brick
    /// digest metadata at end of run — the frozen contract's "digest-metadata
    /// bytes" retained-memory line item, distinct from resident dense payload
    /// and disk footprint. `0` when residency is off.
    pub residency_digest_bytes_final: u64,
    /// ENG-30 row 7 increment 13: the durable backing's own retained
    /// in-process bytes at end of run (`spall_sim::BrickBackingWriter::resident_bytes`)
    /// — `None` when residency is off or the installed backing keeps no
    /// resident cache (a pure disk store).
    pub residency_backing_resident_bytes: Option<u64>,
    /// ENG-30 row 7 increment 13: this process's peak resident/working-set
    /// memory in bytes since start, independent of residency being on —
    /// `docs/reports/G3-residency-hash.md`'s "process peak memory" retained-
    /// memory line item. `None` on a platform `spall_server::mem_stats` does
    /// not support.
    pub process_peak_memory_bytes: Option<u64>,
    /// ENG-30 row 7 increment 15: cumulative, across every periodic +
    /// shutdown checkpoint this run, of terrain bricks
    /// `ResidencyPass::capture_checkpoint` actually re-snapshotted / reloaded
    /// / re-encoded (its revision differed from the last checkpoint that
    /// captured it). `0` when residency is off (that path always uses
    /// `persist::capture`'s unconditional full walk).
    pub residency_checkpoint_bricks_captured_total: u64,
    /// ENG-30 row 7 increment 15: cumulative count of terrain bricks each of
    /// those `capture_checkpoint` calls' complete logical (resident ∪
    /// evicted) set contained. `captured_total < logical_total` is the
    /// direct evidence that checkpoint capture is incremental rather than a
    /// full walk with a cache wrapped around it. `0` when residency is off.
    pub residency_checkpoint_bricks_logical_total: u64,
    /// T23 / G4: owning server tick busy-time percentiles, excluding the
    /// configured warmup window and pacing sleep. Zero samples means no timing
    /// window was configured or the run ended before it completed.
    pub tick_busy_p95_ms: f64,
    pub tick_busy_p99_ms: f64,
    pub tick_busy_max_ms: f64,
    pub tick_busy_samples: u64,
    pub tick_busy_window_complete: bool,
    /// T23 / G4: physics-step duration percentiles. This measures
    /// `world.step_physics` and body-pose extraction; player sweep/advance is
    /// reported in the owning tick busy time, not this physics field.
    pub physics_p95_ms: f64,
    pub physics_p99_ms: f64,
    pub physics_max_ms: f64,
    pub physics_samples: u64,
    pub physics_window_complete: bool,
}

/// Admission and per-tick pressure observed by the authoritative edit pipeline.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct IntentStats {
    pub queue_full_rejections: u64,
    pub pending_max: u64,
    pub retried_conflicts: u64,
    pub stale_discards: u64,
    pub serialized_regions: u64,
    pub ticks: u64,
    pub pending_ticks: u64,
    pub commits_in_pending_ticks: u64,
    pub pending_depth_sum: u64,
}

impl IntentStats {
    pub(super) fn record_tick(
        &mut self,
        pending_after: usize,
        committed: usize,
        retried: usize,
        stale_discards: usize,
        serialized_regions: usize,
    ) {
        self.ticks += 1;
        self.pending_max = self.pending_max.max(pending_after as u64);
        self.pending_depth_sum += pending_after as u64;
        self.retried_conflicts += retried as u64;
        self.stale_discards += stale_discards as u64;
        self.serialized_regions += serialized_regions as u64;
        if pending_after > 0 {
            self.pending_ticks += 1;
            self.commits_in_pending_ticks += committed as u64;
        }
    }
}

pub(super) fn intent_rejection_reason(error: &spall_sim::IntentError) -> String {
    if matches!(error, spall_sim::IntentError::QueueFull { .. }) {
        format!("overloaded: {error}; retry with backoff")
    } else {
        error.to_string()
    }
}

/// One connection's total egress this run, alongside where its interest
/// anchor (player spawn) was, so a reviewer can read bandwidth separation
/// directly off the numbers without any client-side cooperation.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PerClientEgress {
    pub slot: u32,
    /// `None` on a scene with no player spawns (a `motion_static_anchor`
    /// scenario) or if the connection closed before `Joined` was processed.
    pub spawn_m: Option<[f64; 3]>,
    pub app_bytes: u64,
    pub transport_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TimingReport {
    pub(super) tick_busy_p95_ms: f64,
    pub(super) tick_busy_p99_ms: f64,
    pub(super) tick_busy_max_ms: f64,
    pub(super) tick_busy_samples: u64,
    pub(super) physics_p95_ms: f64,
    pub(super) physics_p99_ms: f64,
    pub(super) physics_max_ms: f64,
    pub(super) physics_samples: u64,
    pub(super) window_complete: bool,
}

pub(super) struct TimingCollector {
    pub(super) window: TimingWindow,
    pub(super) tick_busy_ms: Vec<f64>,
    pub(super) physics_ms: Vec<f64>,
}

impl TimingCollector {
    pub(super) fn new(window: TimingWindow) -> Self {
        Self {
            window,
            tick_busy_ms: Vec::with_capacity(window.max_samples),
            physics_ms: Vec::with_capacity(window.max_samples),
        }
    }

    pub(super) fn record(&mut self, tick: u64, busy: Duration, physics: Duration) {
        let measured_end = self
            .window
            .warmup_ticks
            .saturating_add(self.window.measured_ticks);
        if tick <= self.window.warmup_ticks || tick > measured_end {
            return;
        }
        if self.tick_busy_ms.len() < self.window.max_samples {
            self.tick_busy_ms.push(busy.as_secs_f64() * 1_000.0);
            self.physics_ms.push(physics.as_secs_f64() * 1_000.0);
        }
    }

    pub(super) fn finish(self, ticks_run: u64) -> TimingReport {
        let window_end = self
            .window
            .warmup_ticks
            .saturating_add(self.window.measured_ticks);
        TimingReport {
            tick_busy_p95_ms: percentile(&self.tick_busy_ms, 0.95),
            tick_busy_p99_ms: percentile(&self.tick_busy_ms, 0.99),
            tick_busy_max_ms: self.tick_busy_ms.iter().copied().fold(0.0, f64::max),
            tick_busy_samples: self.tick_busy_ms.len() as u64,
            physics_p95_ms: percentile(&self.physics_ms, 0.95),
            physics_p99_ms: percentile(&self.physics_ms, 0.99),
            physics_max_ms: self.physics_ms.iter().copied().fold(0.0, f64::max),
            physics_samples: self.physics_ms.len() as u64,
            window_complete: self.window.measured_ticks > 0 && ticks_run >= window_end,
        }
    }
}

pub(super) fn percentile(samples: &[f64], fraction: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (fraction * sorted.len() as f64)
        .ceil()
        .clamp(1.0, sorted.len() as f64) as usize;
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_full_intent_rejections_are_marked_retryable() {
        let reason = intent_rejection_reason(&spall_sim::IntentError::QueueFull { limit: 256 });
        assert!(reason.starts_with("overloaded:"));
        assert!(reason.contains("retry with backoff"));
        let reason =
            intent_rejection_reason(&spall_sim::IntentError::DuplicateRequest(RequestId(9)));
        assert!(!reason.starts_with("overloaded:"));
    }

    #[test]
    fn intent_stats_serialize_pipeline_pressure_fields() {
        let stats = IntentStats {
            queue_full_rejections: 2,
            pending_max: 8,
            retried_conflicts: 3,
            stale_discards: 1,
            serialized_regions: 4,
            ticks: 20,
            pending_ticks: 7,
            commits_in_pending_ticks: 9,
            pending_depth_sum: 31,
        };
        let value = serde_json::to_value(stats).unwrap();
        assert_eq!(value["queue_full_rejections"], 2);
        assert_eq!(value["pending_max"], 8);
        assert_eq!(value["pending_depth_sum"], 31);
    }

    #[test]
    fn intent_stats_accumulate_pending_and_rebase_activity_per_tick() {
        let mut stats = IntentStats::default();
        stats.record_tick(2, 1, 3, 1, 2);
        stats.record_tick(0, 1, 0, 0, 0);
        assert_eq!(stats.ticks, 2);
        assert_eq!(stats.pending_max, 2);
        assert_eq!(stats.pending_depth_sum, 2);
        assert_eq!(stats.pending_ticks, 1);
        assert_eq!(stats.commits_in_pending_ticks, 1);
        assert_eq!(stats.retried_conflicts, 3);
        assert_eq!(stats.stale_discards, 1);
        assert_eq!(stats.serialized_regions, 2);
    }

    #[test]
    fn timing_window_excludes_warmup_and_reports_nearest_rank_percentiles() {
        let mut timing = TimingCollector::new(TimingWindow {
            warmup_ticks: 2,
            measured_ticks: 4,
            max_samples: 4,
        });

        // Warmup is deliberately expensive but must not contaminate the gate
        // measurement. The six measured ticks are 1..=4 ms for tick time and
        // 10..=40 ms for physics; the collector should retain only ticks 3..6.
        timing.record(1, Duration::from_millis(100), Duration::from_millis(100));
        timing.record(2, Duration::from_millis(99), Duration::from_millis(99));
        for tick in 3..=6 {
            timing.record(
                tick,
                Duration::from_millis(tick - 2),
                Duration::from_millis((tick - 2) * 10),
            );
        }

        let report = timing.finish(6);
        assert_eq!(report.tick_busy_samples, 4);
        assert_eq!(report.physics_samples, 4);
        assert_eq!(report.tick_busy_p95_ms, 4.0);
        assert_eq!(report.tick_busy_p99_ms, 4.0);
        assert_eq!(report.physics_p95_ms, 40.0);
        assert_eq!(report.physics_p99_ms, 40.0);
        assert_eq!(report.tick_busy_max_ms, 4.0);
        assert_eq!(report.physics_max_ms, 40.0);
        assert!(report.window_complete);
    }

    #[test]
    fn timing_window_is_fail_closed_when_the_requested_window_is_incomplete() {
        let mut timing = TimingCollector::new(TimingWindow {
            warmup_ticks: 1,
            measured_ticks: 3,
            max_samples: 3,
        });
        timing.record(2, Duration::from_millis(1), Duration::from_millis(1));
        timing.record(3, Duration::from_millis(1), Duration::from_millis(1));

        let report = timing.finish(3);
        assert_eq!(report.tick_busy_samples, 2);
        assert_eq!(report.physics_samples, 2);
        assert!(!report.window_complete);
        assert_eq!(report.tick_busy_p95_ms, 1.0);
    }
}
