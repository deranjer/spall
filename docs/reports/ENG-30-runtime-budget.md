# ENG-30 / T23 row 11 — runtime and shaped-join investigation

Date: 2026-09-17. Base: `b0931ff` (PR #127 merged).

## Runtime hypothesis: not supported by the controlled diagnostic

The original lead was that the eight sandbox-client processes and one server
each construct Tokio's multi-thread runtime without an explicit worker count.
On this host `available_parallelism()` reports 16, so source inspection alone
suggested up to 144 runtime workers. That was a plausible lead, but an idle
worker count does not establish runnable CPU contention.

Baseline on the merged tree:

```text
cargo xtask scenario --name t23-g4-join-budget --timeout-ms 300000
```

failed before client 7 received its baseline. Clients 0–6 and the server
converged at `e2aa791e…544f66de`; client 7 reported `connection closed before
the baseline arrived`. The server sent 34,894,744 motion snapshots, and client
7 received only 2,594,276 transport bytes versus roughly 157–177 MiB for each
of the live peers.

As a no-source-change control, the same scenario was run with
`TOKIO_WORKER_THREADS=1`. It failed with the same client-7 pre-baseline
disconnect; clients 0–6 installed in about 21.3 s and were ready in about
23.3 s. The lightweight process sampler confirmed nine sandbox processes, but
its thread-count expression was invalid, so it did **not** capture live Tokio
worker counts. The run therefore disproves neither all scheduler effects nor
the source-inspection estimate; it does show that forcing one Tokio worker per
runtime does not fix this gate and is not a warranted production change.

## Demonstrated cause and fix

The server's default (`motion_interest: None`) motion path broadcast every
20 Hz batch to every connected session. That included a late joiner whose
baseline barrier was still in progress. The interest-aware path already sends
motion only to `LateJoin::live_sessions()`, as required by the late-join
protocol: the joiner receives a current keyframe after it confirms the
baseline.

With 4,352 bodies, sending the supersedable live feed to the shaped client
could compete with the reliable baseline and control streams on its 1 MiB/s
link. Changing only the default path to the same live-session filter turns the
failing run into a pass, supporting this traffic-interference diagnosis. The
egress totals alone do not identify the exact internal queue or scheduling
mechanism. The change does not alter the world, body population, cuts, shaping,
or join budget.

Focused unit coverage verifies that a joining session receives no ordinary
motion batch, then receives it once promoted to `Live`. The existing
`LateJoin::on_baseline_ack` path continues to send the required current motion
keyframe at that promotion barrier.

## Measured passing evidence

```text
cargo xtask scenario --name t23-g4-join-budget --timeout-ms 300000
```

passed with all eight clients, the server, and replay converging at
`9c826894…463fbee9`; all 20 scripted transactions committed. The shaped
client measured 48,122 compressed baseline bytes and 23,838 ms connect-to-ready
with `ready_confirmed: true`, satisfying the unchanged limits of 16 MiB and
30 s. It received 46,956 motion snapshots over the run. Ordinary motion is
withheld during `Phase::Joining`; the existing initial live phase before a
baseline-request sentinel arrives is unchanged, so that total is not proof
that every received snapshot was sent after baseline completion.

## Coordinator integration verification — 2026-09-18

`cargo xtask check` passed on the combined ENG-30/ENG-69 integration: formatting,
workspace/all-target/all-feature Clippy with warnings denied, and all-feature
workspace tests. The new motion-phase and focus-loss tests both ran and passed.
An earlier run failed the existing `separate_process_transport` test; integration
includes the user's existing `9b1e190` datagram-test correction, after which
both that test and the complete suite passed. No additional test exclusions
were introduced to obtain the successful result.

An independent repeat used:

```text
cargo xtask scenario --name t23-g4-join-budget --timeout-ms 300000 --output .local/runs/eng-30-integrated-join-budget
```

It passed with 48,122 compressed bytes and **23,792 ms** confirmed readiness;
all eight clients, the server, and exact replay agreed after all 20 edits.
The full hash was
`9c826894322bc0644b30d3f73aff62461423c348a1e78c55d02707a6463fbee9`.
Raw JSON summaries and process logs are in that output directory of the
`eng-30-69-integration` worktree; check logs are under `.local/validation/`.
The default Tokio worker count was used; there was no concurrent build or
benchmark during the scenario.

The row-11 join-budget evidence is now passing. The overall T23 gate remains
open for the [row-7 residency contract gaps](ENG-30-row7-remaining.md).
