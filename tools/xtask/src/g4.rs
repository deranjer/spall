//! T23 / G4: fail-closed evaluation of the server's measured telemetry
//! (`spall_server::ServeSummary` v10) against `docs/validation.md`'s G4 targets.
//!
//! Every check states what it measured and the target it was held to. A check
//! whose input is absent (no samples, a missing client, no blast recorded)
//! **fails**; nothing here can pass because a measurement was never taken.
//! Warmup is excluded from every steady-state statistic.

use serde::{Deserialize, Serialize};

/// The 60 Hz tick period in milliseconds.
const TICK_MS: f64 = 1000.0 / 60.0;

/// One per-60-ticks server observation (mirrors `spall_server::TelemetrySample`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Sample {
    pub tick: u64,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub process_bytes: Option<u64>,
    #[serde(default)]
    pub backlog_max_bytes: u64,
    #[serde(default)]
    pub backlog_max_age_ms: u64,
    #[serde(default)]
    pub clients: Vec<ClientSample>,
    #[serde(default)]
    pub bodies_total: u64,
    #[serde(default)]
    pub bodies_dormant: u64,
    #[serde(default)]
    pub bodies_awake: u64,
    #[serde(default)]
    pub near_observer_awake: u64,
    #[serde(default)]
    pub giant_origin_y_m: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientSample {
    pub slot: u32,
    pub transport_bytes: u64,
}

/// Mirrors `spall_server::BaselineSendRecord`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct BaselineSend {
    pub session_slot: u32,
    pub payload_bytes: u64,
    pub duration_ms: u64,
}

/// The G4 fields of the server summary, flattened into `ServerSummary`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct G4ServerFacts {
    #[serde(default)]
    pub telemetry_samples: Vec<Sample>,
    #[serde(default)]
    pub blast_commit_ticks: Vec<u64>,
    /// Wall-clock commit time of each blast, parallel to `blast_commit_ticks`.
    #[serde(default)]
    pub blast_commit_elapsed_ms: Vec<u64>,
    /// Server admission accounting for the whole run (from `ServeSummary`).
    #[serde(default)]
    pub actions_requested: u64,
    #[serde(default)]
    pub actions_staged: u64,
    #[serde(default)]
    pub actions_rejected: u64,
    #[serde(default)]
    pub actions_queued_unresolved: u64,
    #[serde(default)]
    pub transactions_committed: u64,
    #[serde(default)]
    pub reliable_backlog_peak_bytes: u64,
    #[serde(default)]
    pub reliable_backlog_peak_age_ms: u64,
    #[serde(default)]
    pub reliable_delivery_age_max_ms: u64,
    #[serde(default)]
    pub reliable_backlog_cap_bytes: u64,
    #[serde(default)]
    pub reliable_backlog_overflows: u64,
    #[serde(default)]
    pub baseline_sends_started: u64,
    #[serde(default)]
    pub baseline_sends_failed: u64,
    #[serde(default)]
    pub baseline_sends_active_max: u64,
    #[serde(default)]
    pub baseline_send_records: Vec<BaselineSend>,
    #[serde(default)]
    pub baseline_rate_limit_bytes_per_sec: Option<u64>,
    #[serde(default)]
    pub capture_pool_workers: u64,
    #[serde(default)]
    pub capture_pool_submitted: u64,
    #[serde(default)]
    pub capture_pool_active_max: u64,
    #[serde(default)]
    pub capture_pool_queued_max: u64,
    #[serde(default)]
    pub process_end_memory_bytes: Option<u64>,
    #[serde(default)]
    pub admission_refused_at_capacity: u64,
}

/// Scenario opt-in: the measured window and the thresholds it is held to.
/// Defaults are `docs/validation.md`'s G4 numbers; a scenario overrides one only
/// where the validation doc names no number (stated in the scenario text).
#[derive(Debug, Clone, Deserialize)]
pub struct G4Telemetry {
    /// Server ticks excluded from every steady statistic.
    pub warmup_ticks: u64,
    /// Server ticks measured after warmup.
    pub measured_ticks: u64,
    /// Ordinary edits and blasts the scenario drives inside the measured
    /// window; the giant collapse is counted separately.
    #[serde(default)]
    pub expected_ordinary_edits: u64,
    #[serde(default)]
    pub expected_blasts: u64,
    /// `docs/validation.md`: `<= 256 KiB/s` server egress per client.
    #[serde(default = "d_egress")]
    pub egress_cap_bytes_per_sec: u64,
    /// "Returns to normal within 5 s after named blast".
    #[serde(default = "d_recovery")]
    pub blast_recovery_sec: u64,
    /// What "normal" is for the unsent reliable backlog once recovered.
    #[serde(default = "d_normal_bytes")]
    pub backlog_normal_bytes: u64,
    #[serde(default = "d_normal_age")]
    pub backlog_normal_age_ms: u64,
    /// "Capped bytes/age at all times": the oldest unsent message may never be
    /// older than this. The bytes cap is the server's own hard limit.
    #[serde(default = "d_cap_age")]
    pub backlog_cap_age_ms: u64,
    /// `docs/validation.md`: server resident memory `<= 8 GiB`.
    #[serde(default = "d_mem")]
    pub server_memory_cap_bytes: u64,
    /// Working-set growth over the window above which memory is called runaway.
    #[serde(default = "d_growth")]
    pub memory_growth_cap_mib_per_min: f64,
    /// "256 active bodies, at least 64 near one observer, throughout".
    #[serde(default = "d_awake")]
    pub min_awake_bodies: u64,
    #[serde(default = "d_near")]
    pub min_near_observer_awake: u64,
    /// "An accumulated population of 4,096 sleeping persistent bodies".
    #[serde(default = "d_dormant")]
    pub min_dormant_bodies: u64,
    /// Fraction of expected edits that must have added a rubble body.
    #[serde(default = "d_rubble")]
    pub min_rubble_fraction: f64,
    /// The 64-brick collapse must have committed inside the run.
    #[serde(default = "d_true")]
    pub require_giant_collapse: bool,
    /// `docs/validation.md`: baselines have a separate `1 MiB/s/client` cap.
    #[serde(default)]
    pub baseline_rate_cap_bytes_per_sec: Option<u64>,
    /// Clients that must have completed their late-join baseline.
    #[serde(default)]
    pub expected_baseline_sends: u64,
}

fn d_egress() -> u64 {
    256 * 1024
}
fn d_recovery() -> u64 {
    5
}
fn d_normal_bytes() -> u64 {
    64 * 1024
}
fn d_normal_age() -> u64 {
    1_000
}
fn d_cap_age() -> u64 {
    5_000
}
fn d_mem() -> u64 {
    8 * 1024 * 1024 * 1024
}
fn d_growth() -> f64 {
    256.0
}
fn d_awake() -> u64 {
    256
}
fn d_near() -> u64 {
    64
}
fn d_dormant() -> u64 {
    4096
}
fn d_rubble() -> f64 {
    0.9
}
fn d_true() -> bool {
    true
}

/// One evaluated requirement.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub measured: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClientEgressRow {
    pub slot: u32,
    /// One-second intervals inside the measured window.
    pub intervals: u64,
    pub mean_bytes_per_sec: f64,
    pub p95_bytes_per_sec: f64,
    pub max_bytes_per_sec: f64,
    pub window_bytes: u64,
}

/// Whether the unsent-reliable backlog is *measured* to have recovered after a blast (cluster),
/// measured to have stayed excessive, or was never measured well enough to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlastStatus {
    /// Every sample interval after the recovery window, up to the next blast cluster, was normal.
    Recovered,
    /// A sample interval wholly after the recovery window measured a backlog above normal.
    MeasuredExcess,
    /// No verdict is possible: see `evidence_gap`. Acceptance treats this as a failure
    /// (fail-closed), but it is **not** a measured excessive backlog.
    NoEvidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlastRow {
    /// Commit ticks of the blasts in this cluster. Blasts whose commits land within one recovery
    /// window (wall time) of each other overlap; they are judged together, from the last commit.
    pub commit_ticks: Vec<u64>,
    pub first_commit_ms: u64,
    pub last_commit_ms: u64,
    /// The recovery window, wall-clock milliseconds after `last_commit_ms`.
    pub recovery_window_ms: u64,
    /// Worst unsent reliable backlog / oldest-message age, per client, over samples whose
    /// intervals overlap the cluster and its recovery window.
    pub peak_bytes_first_window: u64,
    pub peak_age_ms_first_window: u64,
    /// Samples whose whole interval lies after the recovery window and before the next cluster.
    pub tail_samples: u64,
    pub tail_max_bytes: u64,
    pub tail_max_age_ms: u64,
    /// Wall time from the last commit to the start of the first sample interval from which the
    /// backlog stayed normal to the end of the cluster's span; `None` if it never did / unknown.
    pub recovery_wall_ms: Option<u64>,
    pub status: BlastStatus,
    pub evidence_gap: Option<String>,
    /// Kept for readers of older summaries: `status == Recovered`.
    pub recovered: bool,
}

/// Maps a server tick to wall-clock milliseconds by linear interpolation between telemetry
/// samples (used only when the server did not record the commit's wall time).
fn tick_to_ms(samples: &[Sample], tick: u64) -> Option<u64> {
    let first = samples.first()?;
    if tick <= first.tick {
        return Some(first.elapsed_ms * tick / first.tick.max(1));
    }
    for w in samples.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if tick <= b.tick {
            let span = (b.tick - a.tick).max(1) as f64;
            let f = (tick - a.tick) as f64 / span;
            return Some(a.elapsed_ms + ((b.elapsed_ms - a.elapsed_ms) as f64 * f) as u64);
        }
    }
    let last = samples.last()?;
    // Past the last sample: extrapolate at the last interval's rate.
    let prev = samples.iter().rev().nth(1).unwrap_or(last);
    let ms_per_tick =
        (last.elapsed_ms - prev.elapsed_ms) as f64 / (last.tick - prev.tick).max(1) as f64;
    Some(last.elapsed_ms + ((tick - last.tick) as f64 * ms_per_tick) as u64)
}

/// Judges the backlog's recovery after each named blast **in wall-clock time** (a 5 s recovery
/// window must not stretch to 20 s because the server was running slow ticks), handling
/// overlapping blasts explicitly and never turning a missing measurement into either a pass or a
/// claim of measured excess.
///
/// `blast_ticks[i]` is the server tick of blast `i`'s commit; `blast_ms[i]`, when present, is its
/// wall-clock commit time (otherwise it is interpolated from the samples). Only blasts with
/// `win_lo < tick <= win_hi` are judged. A sample's interval is `(previous sample's elapsed,
/// its elapsed]`; its backlog maxima describe that whole interval.
pub fn assess_blast_recovery(
    cfg: &G4Telemetry,
    samples: &[Sample],
    blast_ticks: &[u64],
    blast_ms: &[u64],
    win_lo: u64,
    win_hi: u64,
) -> Vec<BlastRow> {
    let recovery_ms = cfg.blast_recovery_sec * 1000;
    let mut blasts: Vec<(u64, u64)> = blast_ticks
        .iter()
        .enumerate()
        .filter(|(_, t)| **t > win_lo && **t <= win_hi)
        .filter_map(|(i, &t)| {
            let ms = blast_ms
                .get(i)
                .copied()
                .or_else(|| tick_to_ms(samples, t))?;
            Some((t, ms))
        })
        .collect();
    blasts.sort_by_key(|b| b.1);
    // Blasts whose commits are within one recovery window of the previous one overlap: one cluster.
    let mut clusters: Vec<Vec<(u64, u64)>> = Vec::new();
    for b in blasts {
        match clusters.last_mut() {
            Some(c) if b.1.saturating_sub(c.last().expect("non-empty").1) <= recovery_ms => {
                c.push(b)
            }
            _ => clusters.push(vec![b]),
        }
    }
    let window_end_ms = tick_to_ms(samples, win_hi).unwrap_or(u64::MAX);
    // (interval start, interval end, sample) for every sample.
    let intervals: Vec<(u64, u64, &Sample)> = samples
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let start = if i == 0 { 0 } else { samples[i - 1].elapsed_ms };
            (start, s.elapsed_ms, s)
        })
        .collect();
    let normal = |s: &Sample| {
        s.backlog_max_bytes <= cfg.backlog_normal_bytes
            && s.backlog_max_age_ms <= cfg.backlog_normal_age_ms
    };
    let mut rows = Vec::new();
    for (ci, c) in clusters.iter().enumerate() {
        let first_ms = c[0].1;
        let last_ms = c.last().expect("non-empty").1;
        let recovered_by = last_ms + recovery_ms;
        let next_first = clusters.get(ci + 1).map_or(window_end_ms, |n| n[0].1);
        let last_cluster = ci + 1 == clusters.len();
        // Intervals overlapping [first commit, recovered_by].
        let first_window: Vec<&Sample> = intervals
            .iter()
            .filter(|(st, en, _)| *en > first_ms && *st < recovered_by)
            .map(|(_, _, s)| *s)
            .collect();
        // Intervals wholly after the recovery window and ending before the next cluster.
        let tail: Vec<&Sample> = intervals
            .iter()
            .filter(|(st, en, _)| *st >= recovered_by && *en <= next_first)
            .map(|(_, _, s)| *s)
            .collect();
        // The interval that straddles the end of the recovery window (cannot be attributed to
        // either side of it).
        let straddle = intervals
            .iter()
            .find(|(st, en, _)| *st < recovered_by && *en > recovered_by && *st < next_first)
            .map(|(_, _, s)| *s);
        let tail_bytes = tail.iter().map(|s| s.backlog_max_bytes).max().unwrap_or(0);
        let tail_age = tail.iter().map(|s| s.backlog_max_age_ms).max().unwrap_or(0);
        let tail_excess = tail.iter().any(|s| !normal(s));
        let straddle_excess = straddle.is_some_and(|s| !normal(s));
        let (status, gap) = if tail_excess {
            (BlastStatus::MeasuredExcess, None)
        } else if tail.is_empty() {
            (
                BlastStatus::NoEvidence,
                Some(format!(
                    "no sample interval lies wholly between {recovery_ms} ms after the last commit and the {}",
                    if last_cluster {
                        "end of the measured window"
                    } else {
                        "next blast cluster"
                    }
                )),
            )
        } else if straddle_excess {
            (
                BlastStatus::NoEvidence,
                Some(
                    "the sample interval straddling the end of the recovery window measured excess \
                     and is too coarse to say whether it fell before or after it"
                        .to_string(),
                ),
            )
        } else {
            (BlastStatus::Recovered, None)
        };
        // Wall time from the last commit until the backlog stayed normal for the rest of the span.
        let span: Vec<&(u64, u64, &Sample)> = intervals
            .iter()
            .filter(|(st, en, _)| *en > last_ms && *st < next_first)
            .collect();
        let mut quiet_from: Option<u64> = None;
        for (st, _, s) in span.iter().map(|t| (t.0, t.1, t.2)).rev() {
            if normal(s) {
                quiet_from = Some(st);
            } else {
                break;
            }
        }
        rows.push(BlastRow {
            commit_ticks: c.iter().map(|b| b.0).collect(),
            first_commit_ms: first_ms,
            last_commit_ms: last_ms,
            recovery_window_ms: recovery_ms,
            peak_bytes_first_window: first_window
                .iter()
                .map(|s| s.backlog_max_bytes)
                .max()
                .unwrap_or(0),
            peak_age_ms_first_window: first_window
                .iter()
                .map(|s| s.backlog_max_age_ms)
                .max()
                .unwrap_or(0),
            tail_samples: tail.len() as u64,
            tail_max_bytes: tail_bytes,
            tail_max_age_ms: tail_age,
            recovery_wall_ms: quiet_from.map(|q| q.saturating_sub(last_ms)),
            status,
            evidence_gap: gap,
            recovered: status == BlastStatus::Recovered,
        });
    }
    rows
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryRow {
    pub process_peak_bytes: Option<u64>,
    pub process_end_bytes: Option<u64>,
    pub window_first_bytes: Option<u64>,
    pub window_last_bytes: Option<u64>,
    pub window_max_bytes: Option<u64>,
    pub growth_mib_per_min: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BodyRow {
    pub min_awake: Option<u64>,
    pub min_near_observer_awake: Option<u64>,
    pub min_dormant: Option<u64>,
    pub first_total: Option<u64>,
    pub last_total: Option<u64>,
    pub rubble_gained: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinRow {
    pub baseline_sends_started: u64,
    pub baseline_sends_failed: u64,
    pub baseline_sends_active_max: u64,
    pub baseline_rate_limit_bytes_per_sec: Option<u64>,
    pub max_baseline_rate_bytes_per_sec: Option<f64>,
    pub capture_pool_workers: u64,
    pub capture_pool_submitted: u64,
    pub capture_pool_active_max: u64,
    pub capture_pool_queued_max: u64,
    pub admission_refused_at_capacity: u64,
}

/// The G4 evidence and its verdict, written into `summary.json`.
#[derive(Debug, Clone, Serialize)]
pub struct G4Row {
    pub configured: bool,
    pub warmup_ticks: u64,
    pub measured_ticks: u64,
    pub window_samples: u64,
    pub window_wall_seconds: f64,
    pub checks: Vec<Check>,
    pub per_client_egress: Vec<ClientEgressRow>,
    pub blasts: Vec<BlastRow>,
    pub memory: Option<MemoryRow>,
    pub bodies: Option<BodyRow>,
    pub joins: Option<JoinRow>,
    /// Client frame time is a G2 hardware measurement; the networked harness
    /// has no windowed GPU client, so it is reported unavailable rather than
    /// approximated from CPU submission time.
    pub client_frame_time: &'static str,
    pub requirements_met: bool,
}

impl G4Row {
    pub fn unconfigured() -> Self {
        Self {
            configured: false,
            warmup_ticks: 0,
            measured_ticks: 0,
            window_samples: 0,
            window_wall_seconds: 0.0,
            checks: Vec::new(),
            per_client_egress: Vec::new(),
            blasts: Vec::new(),
            memory: None,
            bodies: None,
            joins: None,
            client_frame_time: FRAME_TIME_UNAVAILABLE,
            requirements_met: true,
        }
    }
}

const FRAME_TIME_UNAVAILABLE: &str = "unavailable: the networked scenario harness runs headless clients; client frame time (G2) needs the windowed GPU-timestamp path and is not approximated from CPU submission time";

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// One client's measured end-to-end topology lag (server tick of each
/// transaction versus the freshest tick that client had seen on the wire).
#[derive(Debug, Clone, Copy)]
pub struct ClientLag {
    pub p95_ms: u64,
    pub max_ms: u64,
}

/// Evaluates `facts` against `cfg`. `process_peak_bytes` is the server's OS peak
/// working set; `committed` its committed-transaction count; `clients` how many
/// clients the scenario runs; `large_collapse_samples` how many commits the
/// server classified as a large structural collapse.
pub fn evaluate(
    cfg: &G4Telemetry,
    facts: &G4ServerFacts,
    process_peak_bytes: Option<u64>,
    clients: u64,
    large_collapse_samples: u64,
    client_lags: &[ClientLag],
) -> G4Row {
    let mut checks: Vec<Check> = Vec::new();
    let mut add = |name: &str, passed: bool, measured: String, target: String| {
        checks.push(Check {
            name: name.to_string(),
            passed,
            measured,
            target,
        });
    };

    let win_lo = cfg.warmup_ticks;
    let win_hi = cfg.warmup_ticks + cfg.measured_ticks;
    let all = &facts.telemetry_samples;
    // Samples whose whole interval (previous sample .. this one) lies inside the
    // measured window.
    let in_window: Vec<&Sample> = all
        .iter()
        .filter(|s| s.tick > win_lo + 60 && s.tick <= win_hi)
        .collect();
    let expected_samples = cfg.measured_ticks / 60;
    let last_tick = all.last().map(|s| s.tick).unwrap_or(0);
    let wall_seconds = match (in_window.first(), in_window.last()) {
        (Some(a), Some(b)) => (b.elapsed_ms.saturating_sub(a.elapsed_ms)) as f64 / 1000.0,
        _ => 0.0,
    };
    add(
        "measured window completed with samples",
        !in_window.is_empty()
            && last_tick >= win_hi
            && in_window.len() as u64 + 2 >= expected_samples,
        format!(
            "{} samples over {:.1} s wall (last tick {last_tick})",
            in_window.len(),
            wall_seconds
        ),
        format!(
            ">= {} samples, run reached tick {win_hi}",
            expected_samples.saturating_sub(2)
        ),
    );

    // --- per-client steady egress -------------------------------------------
    let mut slots: Vec<u32> = in_window
        .iter()
        .flat_map(|s| s.clients.iter().map(|c| c.slot))
        .collect();
    slots.sort_unstable();
    slots.dedup();
    let mut egress_rows = Vec::new();
    for slot in &slots {
        let mut series: Vec<(u64, u64)> = Vec::new(); // (elapsed_ms, transport_bytes)
        for s in all.iter().filter(|s| s.tick >= win_lo && s.tick <= win_hi) {
            if let Some(c) = s.clients.iter().find(|c| c.slot == *slot) {
                series.push((s.elapsed_ms, c.transport_bytes));
            }
        }
        let mut rates: Vec<f64> = series
            .windows(2)
            .filter(|w| w[1].0 > w[0].0)
            .map(|w| (w[1].1.saturating_sub(w[0].1)) as f64 / ((w[1].0 - w[0].0) as f64 / 1000.0))
            .collect();
        let window_bytes = match (series.first(), series.last()) {
            (Some(a), Some(b)) => b.1.saturating_sub(a.1),
            _ => 0,
        };
        let mean = if rates.is_empty() {
            0.0
        } else {
            rates.iter().sum::<f64>() / rates.len() as f64
        };
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        egress_rows.push(ClientEgressRow {
            slot: *slot,
            intervals: rates.len() as u64,
            mean_bytes_per_sec: mean,
            p95_bytes_per_sec: percentile(&rates, 0.95),
            max_bytes_per_sec: rates.last().copied().unwrap_or(0.0),
            window_bytes,
        });
    }
    let cap = cfg.egress_cap_bytes_per_sec as f64;
    let worst_p95 = egress_rows
        .iter()
        .map(|r| r.p95_bytes_per_sec)
        .fold(0.0, f64::max);
    let worst_mean = egress_rows
        .iter()
        .map(|r| r.mean_bytes_per_sec)
        .fold(0.0, f64::max);
    add(
        "every client observed for the whole window",
        egress_rows.len() as u64 >= clients
            && egress_rows
                .iter()
                .all(|r| r.intervals + 2 >= expected_samples),
        format!(
            "{} of {clients} clients, min intervals {}",
            egress_rows.len(),
            egress_rows.iter().map(|r| r.intervals).min().unwrap_or(0)
        ),
        format!(
            "{clients} clients x >= {} intervals",
            expected_samples.saturating_sub(2)
        ),
    );
    add(
        "steady per-client egress p95 <= cap",
        !egress_rows.is_empty() && worst_p95 <= cap,
        format!(
            "worst p95 {:.1} KiB/s (worst mean {:.1} KiB/s, worst max {:.1} KiB/s; transport bytes incl. overhead)",
            worst_p95 / 1024.0,
            worst_mean / 1024.0,
            egress_rows
                .iter()
                .map(|r| r.max_bytes_per_sec)
                .fold(0.0, f64::max)
                / 1024.0
        ),
        format!("<= {:.0} KiB/s per client", cap / 1024.0),
    );

    // --- reliable backlog: caps and per-blast recovery ----------------------
    add(
        "application send-queue never exceeded its hard cap",
        facts.reliable_backlog_overflows == 0
            && facts.reliable_backlog_cap_bytes > 0
            && facts.reliable_backlog_peak_bytes <= facts.reliable_backlog_cap_bytes,
        format!(
            "peak {} of cap {}, overflow disconnects {}",
            mib(facts.reliable_backlog_peak_bytes),
            mib(facts.reliable_backlog_cap_bytes),
            facts.reliable_backlog_overflows
        ),
        "peak <= cap, 0 overflow disconnects".to_string(),
    );
    add(
        "application send-queue age capped at all times",
        !all.is_empty() && facts.reliable_backlog_peak_age_ms <= cfg.backlog_cap_age_ms,
        format!(
            "peak sampled age {} ms (worst enqueue-to-transport wait {} ms)",
            facts.reliable_backlog_peak_age_ms, facts.reliable_delivery_age_max_ms
        ),
        format!("<= {} ms", cfg.backlog_cap_age_ms),
    );

    // The application-level queue above cannot see data already handed to the
    // transport (QUIC buffers unsent reliable bytes when the congestion window
    // is small), so it read "recovered" while replicas were 76 s behind. The
    // client-observed lag is the end-to-end measure.
    let worst_lag_p95 = client_lags.iter().map(|l| l.p95_ms).max();
    let worst_lag_max = client_lags.iter().map(|l| l.max_ms).max();
    add(
        "end-to-end topology lag (client-observed) within cap for every client",
        client_lags.len() as u64 >= clients
            && worst_lag_max.is_some_and(|m| m <= cfg.backlog_cap_age_ms),
        format!(
            "{} clients reporting; worst p95 {:?} ms, worst max {:?} ms",
            client_lags.len(),
            worst_lag_p95,
            worst_lag_max
        ),
        format!(
            "{clients} clients, max lag <= {} ms",
            cfg.backlog_cap_age_ms
        ),
    );

    let blast_rows = assess_blast_recovery(
        cfg,
        all,
        &facts.blast_commit_ticks,
        &facts.blast_commit_elapsed_ms,
        win_lo,
        win_hi,
    );
    let blasts_in_window = facts
        .blast_commit_ticks
        .iter()
        .filter(|t| **t > win_lo && **t <= win_hi)
        .count();
    let recovered = blast_rows
        .iter()
        .filter(|b| b.status == BlastStatus::Recovered)
        .count();
    let excess = blast_rows
        .iter()
        .filter(|b| b.status == BlastStatus::MeasuredExcess)
        .count();
    let no_evidence = blast_rows
        .iter()
        .filter(|b| b.status == BlastStatus::NoEvidence)
        .count();
    add(
        "every named blast committed",
        cfg.expected_blasts > 0 && blasts_in_window as u64 >= cfg.expected_blasts,
        format!("{blasts_in_window} blast commits in the measured window"),
        format!(">= {}", cfg.expected_blasts),
    );
    add(
        "application send-queue back to normal within the recovery window after every blast",
        !blast_rows.is_empty() && recovered == blast_rows.len(),
        format!(
            "{recovered} of {} blast clusters recovered, {excess} measured excessive, {no_evidence} with no usable evidence (not measured); {blasts_in_window} blast commits (worst first-window peak {} KiB / {} ms; worst tail {} KiB / {} ms)",
            blast_rows.len(),
            blast_rows
                .iter()
                .map(|b| b.peak_bytes_first_window)
                .max()
                .unwrap_or(0)
                / 1024,
            blast_rows
                .iter()
                .map(|b| b.peak_age_ms_first_window)
                .max()
                .unwrap_or(0),
            blast_rows
                .iter()
                .map(|b| b.tail_max_bytes)
                .max()
                .unwrap_or(0)
                / 1024,
            blast_rows
                .iter()
                .map(|b| b.tail_max_age_ms)
                .max()
                .unwrap_or(0),
        ),
        format!(
            "<= {} KiB and <= {} ms within {} s (wall clock) of each blast cluster",
            cfg.backlog_normal_bytes / 1024,
            cfg.backlog_normal_age_ms,
            cfg.blast_recovery_sec
        ),
    );

    // --- memory ---------------------------------------------------------------
    let mem_pts: Vec<(f64, u64)> = in_window
        .iter()
        .filter_map(|s| s.process_bytes.map(|b| (s.elapsed_ms as f64 / 60_000.0, b)))
        .collect();
    let growth = if mem_pts.len() >= 4 {
        let n = mem_pts.len() as f64;
        let (sx, sy) = mem_pts
            .iter()
            .fold((0.0, 0.0), |(x, y), (t, b)| (x + t, y + *b as f64));
        let (mx, my) = (sx / n, sy / n);
        let num: f64 = mem_pts
            .iter()
            .map(|(t, b)| (t - mx) * (*b as f64 - my))
            .sum();
        let den: f64 = mem_pts.iter().map(|(t, _)| (t - mx).powi(2)).sum();
        (den > 0.0).then(|| num / den / (1024.0 * 1024.0))
    } else {
        None
    };
    let memory = MemoryRow {
        process_peak_bytes,
        process_end_bytes: facts.process_end_memory_bytes,
        window_first_bytes: mem_pts.first().map(|p| p.1),
        window_last_bytes: mem_pts.last().map(|p| p.1),
        window_max_bytes: mem_pts.iter().map(|p| p.1).max(),
        growth_mib_per_min: growth,
    };
    add(
        "server process peak memory within cap",
        process_peak_bytes.is_some_and(|b| b <= cfg.server_memory_cap_bytes),
        process_peak_bytes.map_or("unavailable".into(), mib),
        format!("<= {}", mib(cfg.server_memory_cap_bytes)),
    );
    add(
        "no runaway memory growth over the window",
        growth.is_some_and(|g| g <= cfg.memory_growth_cap_mib_per_min),
        growth.map_or("unavailable (too few samples)".into(), |g| {
            format!("{g:.1} MiB/min (least-squares over the window)")
        }),
        format!("<= {:.0} MiB/min", cfg.memory_growth_cap_mib_per_min),
    );

    // --- body populations throughout the window -------------------------------
    let bodies = BodyRow {
        min_awake: in_window.iter().map(|s| s.bodies_awake).min(),
        min_near_observer_awake: in_window.iter().map(|s| s.near_observer_awake).min(),
        min_dormant: in_window.iter().map(|s| s.bodies_dormant).min(),
        first_total: in_window.first().map(|s| s.bodies_total),
        last_total: in_window.last().map(|s| s.bodies_total),
        rubble_gained: match (in_window.first(), in_window.last()) {
            (Some(a), Some(b)) => Some(b.bodies_total.saturating_sub(a.bodies_total)),
            _ => None,
        },
    };
    add(
        "active (solver-awake) bodies throughout the window",
        bodies.min_awake.is_some_and(|m| m >= cfg.min_awake_bodies),
        format!("min {:?}", bodies.min_awake),
        format!(">= {}", cfg.min_awake_bodies),
    );
    add(
        "active bodies near the observer throughout the window",
        bodies
            .min_near_observer_awake
            .is_some_and(|m| m >= cfg.min_near_observer_awake),
        format!("min {:?}", bodies.min_near_observer_awake),
        format!(">= {} within 12 m", cfg.min_near_observer_awake),
    );
    add(
        "sleeping persistent bodies throughout the window",
        bodies
            .min_dormant
            .is_some_and(|m| m >= cfg.min_dormant_bodies),
        format!("min {:?}", bodies.min_dormant),
        format!(">= {}", cfg.min_dormant_bodies),
    );
    let want_rubble = (cfg.expected_ordinary_edits as f64 * cfg.min_rubble_fraction) as u64;
    // Not a growth *failure* when short: rubble accumulates one body per committed edit, so a run
    // that committed fewer edits than the workload scripted cannot reach the required amount.
    add(
        "required rubble accumulation reached",
        cfg.expected_ordinary_edits > 0 && bodies.rubble_gained.is_some_and(|g| g >= want_rubble),
        format!(
            "{:?} bodies gained ({:?} -> {:?}); {} of {} requested edits committed",
            bodies.rubble_gained,
            bodies.first_total,
            bodies.last_total,
            facts.transactions_committed,
            facts.actions_requested
        ),
        format!(
            "required >= {want_rubble} bodies gained ({} x expected ordinary edits {})",
            cfg.min_rubble_fraction, cfg.expected_ordinary_edits
        ),
    );
    add(
        "workload completed: every requested edit committed, none rejected or left unresolved",
        facts.actions_requested > 0
            && facts.actions_rejected == 0
            && facts.actions_queued_unresolved == 0
            && facts.transactions_committed == facts.actions_requested,
        format!(
            "{} requested, {} staged, {} committed, {} rejected, {} unresolved (convergence of the committed work is checked separately)",
            facts.actions_requested,
            facts.actions_staged,
            facts.transactions_committed,
            facts.actions_rejected,
            facts.actions_queued_unresolved
        ),
        "committed == requested, 0 rejected, 0 unresolved".to_string(),
    );
    if cfg.require_giant_collapse {
        let ys: Vec<f64> = all.iter().filter_map(|s| s.giant_origin_y_m).collect();
        let standing = ys.first().copied();
        let lowest = ys.iter().copied().fold(f64::INFINITY, f64::min);
        add(
            "64-brick connected collapse: the block came down inside the run",
            standing.is_some_and(|y| y >= 0.0) && lowest <= -3.0,
            format!(
                "giant origin y {:?} m -> lowest {:.1} m; {large_collapse_samples} commit(s) classed large (the block stays the parent, so the detached plate is small)",
                standing, lowest
            ),
            "starts >= 0 m, falls to <= -3 m".to_string(),
        );
    }

    // --- join / baseline concurrency and backpressure -------------------------
    let max_rate = facts
        .baseline_send_records
        .iter()
        .filter(|r| r.duration_ms >= 20)
        .map(|r| r.payload_bytes as f64 / (r.duration_ms as f64 / 1000.0))
        .fold(None, |m: Option<f64>, r| Some(m.map_or(r, |m| m.max(r))));
    let joins = JoinRow {
        baseline_sends_started: facts.baseline_sends_started,
        baseline_sends_failed: facts.baseline_sends_failed,
        baseline_sends_active_max: facts.baseline_sends_active_max,
        baseline_rate_limit_bytes_per_sec: facts.baseline_rate_limit_bytes_per_sec,
        max_baseline_rate_bytes_per_sec: max_rate,
        capture_pool_workers: facts.capture_pool_workers,
        capture_pool_submitted: facts.capture_pool_submitted,
        capture_pool_active_max: facts.capture_pool_active_max,
        capture_pool_queued_max: facts.capture_pool_queued_max,
        admission_refused_at_capacity: facts.admission_refused_at_capacity,
    };
    add(
        "every expected baseline transfer completed, none failed",
        facts.baseline_sends_failed == 0
            && facts.baseline_send_records.len() as u64 >= cfg.expected_baseline_sends,
        format!(
            "{} completed, {} failed",
            facts.baseline_send_records.len(),
            facts.baseline_sends_failed
        ),
        format!(">= {} completed, 0 failed", cfg.expected_baseline_sends),
    );
    add(
        "baseline concurrency bounded",
        facts.capture_pool_workers > 0
            && facts.capture_pool_active_max <= facts.capture_pool_workers
            && facts.baseline_sends_active_max <= clients.max(1),
        format!(
            "captures active max {} of {} workers (queued max {}); transfers in flight max {}",
            facts.capture_pool_active_max,
            facts.capture_pool_workers,
            facts.capture_pool_queued_max,
            facts.baseline_sends_active_max
        ),
        format!("captures <= workers, transfers <= {clients} clients"),
    );
    if let Some(cap) = cfg.baseline_rate_cap_bytes_per_sec {
        add(
            "per-client baseline rate within its cap",
            facts.baseline_rate_limit_bytes_per_sec.is_some()
                && max_rate.is_none_or(|r| r <= cap as f64 * 1.05),
            max_rate.map_or("no transfer long enough to time".into(), |r| {
                format!("max {:.0} KiB/s", r / 1024.0)
            }),
            format!(
                "<= {:.0} KiB/s and a server-side limiter configured",
                cap as f64 / 1024.0
            ),
        );
    }

    let requirements_met = checks.iter().all(|c| c.passed);
    let _ = TICK_MS;
    G4Row {
        configured: true,
        warmup_ticks: cfg.warmup_ticks,
        measured_ticks: cfg.measured_ticks,
        window_samples: in_window.len() as u64,
        window_wall_seconds: wall_seconds,
        checks,
        per_client_egress: egress_rows,
        blasts: blast_rows,
        memory: Some(memory),
        bodies: Some(bodies),
        joins: Some(joins),
        client_frame_time: FRAME_TIME_UNAVAILABLE,
        requirements_met,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> G4Telemetry {
        serde_json::from_str(r#"{"warmup_ticks":600,"measured_ticks":1200,"expected_blasts":1,"expected_ordinary_edits":10,"expected_baseline_sends":1}"#)
            .unwrap()
    }

    /// A healthy synthetic run: 2 clients at 100 KiB/s, tiny backlog, one blast
    /// at tick 900 that recovers.
    fn healthy() -> G4ServerFacts {
        let mut samples = Vec::new();
        for i in 1..=40u64 {
            let tick = i * 60;
            samples.push(Sample {
                tick,
                elapsed_ms: tick * 1000 / 60,
                process_bytes: Some(1_000_000_000),
                backlog_max_bytes: if (900..960).contains(&tick) {
                    500_000
                } else {
                    100
                },
                backlog_max_age_ms: if (900..960).contains(&tick) { 900 } else { 5 },
                clients: (0..2)
                    .map(|slot| ClientSample {
                        slot,

                        transport_bytes: i * 102_400,
                    })
                    .collect(),
                bodies_total: 5000 + tick / 6,
                bodies_dormant: 4400,
                bodies_awake: 300,
                near_observer_awake: 70,
                giant_origin_y_m: Some(if tick < 1000 { 1.0 } else { -7.0 }),
            });
        }
        G4ServerFacts {
            telemetry_samples: samples,
            blast_commit_ticks: vec![900],
            actions_requested: 10,
            actions_staged: 10,
            transactions_committed: 10,
            reliable_backlog_cap_bytes: 8 << 20,
            reliable_backlog_peak_bytes: 500_000,
            reliable_backlog_peak_age_ms: 900,
            baseline_sends_started: 1,
            baseline_sends_active_max: 1,
            baseline_send_records: vec![BaselineSend {
                session_slot: 0,
                payload_bytes: 50_000,
                duration_ms: 100,
            }],
            capture_pool_workers: 4,
            capture_pool_active_max: 1,
            ..G4ServerFacts::default()
        }
    }

    #[test]
    fn a_healthy_run_passes_every_check() {
        let row = evaluate(
            &cfg(),
            &healthy(),
            Some(2_000_000_000),
            2,
            1,
            &[ClientLag {
                p95_ms: 100,
                max_ms: 400,
            }; 2],
        );
        let failed: Vec<_> = row.checks.iter().filter(|c| !c.passed).collect();
        assert!(failed.is_empty(), "{failed:#?}");
        assert!(row.requirements_met);
    }

    #[test]
    fn missing_measurements_fail_closed() {
        let row = evaluate(&cfg(), &G4ServerFacts::default(), None, 2, 0, &[]);
        assert!(!row.requirements_met);
        for name in [
            "measured window completed with samples",
            "server process peak memory within cap",
            "application send-queue back to normal within the recovery window after every blast",
            "steady per-client egress p95 <= cap",
            "active (solver-awake) bodies throughout the window",
        ] {
            let c = row.checks.iter().find(|c| c.name == name).expect(name);
            assert!(!c.passed, "{name} must not pass on no data");
        }
    }

    // --- blast recovery: wall-clock windows, overlapping blasts, missing evidence -----------

    fn bsample(tick: u64, elapsed_ms: u64, high: bool) -> Sample {
        Sample {
            tick,
            elapsed_ms,
            backlog_max_bytes: if high { 2_000_000 } else { 100 },
            backlog_max_age_ms: if high { 4_000 } else { 5 },
            ..Sample::default()
        }
    }

    /// One sample per second of wall time (60 ticks each), `high` between the given seconds.
    fn per_second(secs: std::ops::RangeInclusive<u64>, high: std::ops::Range<u64>) -> Vec<Sample> {
        secs.map(|k| bsample(k * 60, k * 1000, high.contains(&k)))
            .collect()
    }

    #[test]
    fn a_blast_that_recovers_is_measured_in_wall_time() {
        // Blast committed at 60 s; the backlog is high for the three seconds after it.
        let samples = per_second(50..=90, 61..64);
        let rows = assess_blast_recovery(&cfg(), &samples, &[3_600], &[60_000], 0, 90 * 60);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.status, BlastStatus::Recovered, "{r:#?}");
        assert!(r.recovered);
        assert_eq!(r.recovery_wall_ms, Some(3_000), "{r:#?}");
        assert!(r.tail_samples > 0 && r.evidence_gap.is_none());
    }

    #[test]
    fn slow_ticks_stretch_ticks_not_the_wall_clock_recovery_window() {
        // The server runs at ~15 ticks/s: a sample (60 ticks) covers 4 s. The backlog is high
        // until 68 s; the blast committed at 60 s, so the 5 s window ends at 65 s. A tick-based
        // window (300 ticks) would have ended at ~80 s and called this recovered.
        let samples: Vec<Sample> = (10..=30u64)
            .map(|k| bsample(k * 60, k * 4_000, (15..18).contains(&k))) // high through 68 s
            .collect();
        let rows = assess_blast_recovery(&cfg(), &samples, &[900], &[60_000], 0, 30 * 60);
        let r = &rows[0];
        // The interval (64 s, 68 s] straddles the end of the window and is high: the backlog may
        // have recovered late or on time and this sampling cannot say -- not "recovered", and not
        // a *measured* excess either.
        assert_eq!(r.status, BlastStatus::NoEvidence, "{r:#?}");
        assert!(r.evidence_gap.as_deref().unwrap().contains("straddling"));
        assert!(!r.recovered);
        // The same backlog sampled once a second is unambiguous: excess after the window.
        let fine = per_second(50..=90, 61..70);
        let rows = assess_blast_recovery(&cfg(), &fine, &[3_600], &[60_000], 0, 90 * 60);
        assert_eq!(
            rows[0].status,
            BlastStatus::MeasuredExcess,
            "{:#?}",
            rows[0]
        );
    }

    #[test]
    fn bunched_commits_are_one_cluster_judged_from_the_last_commit() {
        // Three blasts committed within 100 ms of each other (a backed-up intent queue drained
        // in one burst), then a separate blast 15 s later.
        let samples = per_second(50..=100, 61..63);
        let rows = assess_blast_recovery(
            &cfg(),
            &samples,
            &[3_600, 3_603, 3_606, 4_500],
            &[60_000, 60_050, 60_100, 75_000],
            0,
            100 * 60,
        );
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert_eq!(rows[0].commit_ticks, vec![3_600, 3_603, 3_606]);
        assert_eq!(rows[0].last_commit_ms, 60_100);
        assert_eq!(rows[0].status, BlastStatus::Recovered, "{:#?}", rows[0]);
        assert_eq!(rows[1].commit_ticks, vec![4_500]);
        assert_eq!(rows[1].status, BlastStatus::Recovered, "{:#?}", rows[1]);
    }

    #[test]
    fn overlapping_blasts_are_judged_after_the_last_one_not_the_first() {
        // Blasts 3 s apart with a 5 s window overlap: one cluster whose window runs from the last
        // commit (66 s) to 71 s; the backlog is high until 68 s, normal after.
        let samples = per_second(50..=100, 61..69);
        let rows = assess_blast_recovery(
            &cfg(),
            &samples,
            &[3_600, 3_780, 3_960],
            &[60_000, 63_000, 66_000],
            0,
            100 * 60,
        );
        assert_eq!(rows.len(), 1, "{rows:#?}");
        assert_eq!(rows[0].commit_ticks.len(), 3);
        assert_eq!(rows[0].recovery_window_ms, 5_000);
        assert_eq!(rows[0].status, BlastStatus::Recovered, "{:#?}", rows[0]);
        assert_eq!(rows[0].recovery_wall_ms, Some(2_000), "{:#?}", rows[0]);
    }

    #[test]
    fn missing_samples_are_no_evidence_not_measured_excess_and_still_fail_closed() {
        // The run stopped 2 s after the blast: nothing was measured after the window.
        let samples = per_second(50..=62, 61..62);
        let rows = assess_blast_recovery(&cfg(), &samples, &[3_600], &[60_000], 0, 100 * 60);
        assert_eq!(rows[0].status, BlastStatus::NoEvidence, "{:#?}", rows[0]);
        assert_eq!(rows[0].tail_samples, 0);
        assert!(rows[0].evidence_gap.is_some());
        assert!(!rows[0].recovered);
        // A window that never contained samples between blast clusters is also a gap.
        let samples = per_second(50..=100, 100..100);
        let sparse: Vec<Sample> = samples.into_iter().filter(|s| s.tick % 600 == 0).collect();
        let rows = assess_blast_recovery(
            &cfg(),
            &sparse,
            &[3_600, 4_200],
            &[60_000, 70_000],
            0,
            100 * 60,
        );
        assert_eq!(rows[0].status, BlastStatus::NoEvidence, "{:#?}", rows[0]);
        // And through `evaluate`: the acceptance check fails and says why, without claiming excess.
        let mut facts = healthy();
        facts.telemetry_samples.retain(|s| s.tick <= 960);
        let row = evaluate(&cfg(), &facts, Some(1), 2, 1, &[]);
        let c = row
            .checks
            .iter()
            .find(|c| c.name.contains("back to normal"))
            .unwrap();
        assert!(!c.passed, "{c:?}");
        assert!(c.measured.contains("no usable evidence"), "{c:?}");
        assert!(c.measured.contains("0 measured excessive"), "{c:?}");
    }

    /// Diagnostic: re-judge the blast recovery of a finished run with the current logic.
    /// `RUN_DIR=<dir> cargo test -p xtask --release g4::tests::reassess_run -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic: needs RUN_DIR"]
    fn reassess_run() {
        let dir = std::path::PathBuf::from(std::env::var("RUN_DIR").expect("RUN_DIR"));
        let facts: G4ServerFacts =
            serde_json::from_slice(&std::fs::read(dir.join("server.summary.json")).unwrap())
                .unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("summary.json")).unwrap()).unwrap();
        let g4 = &summary["g4"];
        let warmup = g4["warmup_ticks"].as_u64().unwrap();
        let measured = g4["measured_ticks"].as_u64().unwrap();
        let rows = assess_blast_recovery(
            &cfg(),
            &facts.telemetry_samples,
            &facts.blast_commit_ticks,
            &facts.blast_commit_elapsed_ms,
            warmup,
            warmup + measured,
        );
        let count = |s: BlastStatus| rows.iter().filter(|r| r.status == s).count();
        println!(
            "{}: {} blast commits -> {} clusters: {} recovered, {} measured excess, {} no evidence",
            dir.display(),
            facts
                .blast_commit_ticks
                .iter()
                .filter(|t| **t > warmup && **t <= warmup + measured)
                .count(),
            rows.len(),
            count(BlastStatus::Recovered),
            count(BlastStatus::MeasuredExcess),
            count(BlastStatus::NoEvidence),
        );
        let tick_ms = facts
            .telemetry_samples
            .windows(2)
            .map(|w| {
                (w[1].elapsed_ms - w[0].elapsed_ms) as f64 / (w[1].tick - w[0].tick).max(1) as f64
            })
            .fold(0.0, f64::max);
        println!("worst sample-interval wall ms per tick: {tick_ms:.1}");
        for r in rows.iter().take(12) {
            println!(
                "  ticks {:?} commit_ms {}..{} recovery_wall {:?} tail {} ({} KiB / {} ms) {:?} {}",
                r.commit_ticks,
                r.first_commit_ms,
                r.last_commit_ms,
                r.recovery_wall_ms,
                r.tail_samples,
                r.tail_max_bytes / 1024,
                r.tail_max_age_ms,
                r.status,
                r.evidence_gap.as_deref().unwrap_or("")
            );
        }
    }

    #[test]
    fn a_run_that_committed_only_part_of_the_workload_fails_completion_not_convergence() {
        // 18,181 requested, 8,259 committed, 9,922 rejected (a v2-soak-shaped run).
        let mut facts = healthy();
        facts.actions_requested = 18_181;
        facts.actions_staged = 8_259;
        facts.transactions_committed = 8_259;
        facts.actions_rejected = 9_922;
        let row = evaluate(&cfg(), &facts, Some(1), 2, 1, &[]);
        let c = row
            .checks
            .iter()
            .find(|c| c.name.starts_with("workload completed"))
            .unwrap();
        assert!(!c.passed, "{c:?}");
        assert!(
            c.measured.contains("18181 requested") && c.measured.contains("9922 rejected"),
            "{c:?}"
        );
        let r = row
            .checks
            .iter()
            .find(|c| c.name == "required rubble accumulation reached")
            .unwrap();
        assert!(
            r.measured.contains("of 18181 requested edits committed"),
            "{r:?}"
        );
        // Nothing requested is a missing measurement, never a pass.
        let mut none = healthy();
        none.actions_requested = 0;
        let row = evaluate(&cfg(), &none, Some(1), 2, 1, &[]);
        assert!(
            !row.checks
                .iter()
                .find(|c| c.name.starts_with("workload completed"))
                .unwrap()
                .passed
        );
    }
    #[test]
    fn a_backlog_that_never_drains_after_a_blast_fails() {
        let mut facts = healthy();
        for s in &mut facts.telemetry_samples {
            if s.tick >= 900 {
                s.backlog_max_bytes = 2_000_000;
                s.backlog_max_age_ms = 4_000;
            }
        }
        let row = evaluate(&cfg(), &facts, Some(1), 2, 1, &[]);
        let c = row
            .checks
            .iter()
            .find(|c| c.name.contains("back to normal"))
            .unwrap();
        assert!(!c.passed, "{c:?}");
    }

    #[test]
    fn over_cap_egress_and_short_windows_fail() {
        let mut facts = healthy();
        for s in &mut facts.telemetry_samples {
            for c in &mut s.clients {
                c.transport_bytes = s.tick * 8_000; // 480 KB/s
            }
        }
        let row = evaluate(&cfg(), &facts, Some(1), 2, 1, &[]);
        assert!(
            !row.checks
                .iter()
                .find(|c| c.name.starts_with("steady per-client egress"))
                .unwrap()
                .passed
        );
        // Run that stopped before the window ended.
        let mut short = healthy();
        short.telemetry_samples.truncate(15);
        let row = evaluate(&cfg(), &short, Some(1), 2, 1, &[]);
        assert!(
            !row.checks
                .iter()
                .find(|c| c.name.starts_with("measured window"))
                .unwrap()
                .passed
        );
    }
}
