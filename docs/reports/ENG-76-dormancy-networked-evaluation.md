# ENG-76 — safe dormancy evaluation seam

Status: the default dormancy policy is unchanged; this increment adds an
explicit, opt-in settle-window override so the measured short-window policy
can be evaluated in a real server run without silently changing existing gate
fixtures.

## Change

`sandbox-server --dormancy-settle-ticks N` now constructs the normal
`DormancyConfig::DEFAULT` with only `settle_ticks` replaced. The flag requires
`--dormancy`, accepts `1..=1_000_000`, and remains absent from all existing
scenarios. The xtask scenario schema exposes the same value as
`dormancy_settle_ticks`.

No re-sleep is forced, no workload is reduced, and no geometry is compacted.
The existing server-authoritative support-removal, body-edit hard wake,
proximity wake, and moving-body guard remain the paths used by the pass.

## Behavioral evidence

The new `spall_sim` policy test models a still body disturbed by a hard terrain
edit every 28 ticks for 112 ticks, with proximity hysteresis set to 10,000
ticks. The measured policy outcomes are:

| settle window | deactivations | hard reactivations | final state |
| ---: | ---: | ---: | --- |
| 120 ticks | 0 | 0 | awake |
| 20 ticks | 4 | 4 | awake |

The existing moving-body regression remains in the same test module and still
rejects deactivation for a body moving at 1 m/s.

This is policy-level evidence, not a claim that the G4 networked physics p95
gate passes. The new `sleep-wake-settle-20` fixture provides a real networked
server/client check of the override and wake paths: 900 ticks, 2/2 committed
transactions, `dormancy_deactivations_total: 1`,
`dormancy_reactivations_total: 1`, and matching server/client/replay hash
`50302b2e3e426711d651b3c3873c93a963828957a98b49aca2f567df35f2d9c0`.
A sustained alternating G4 run with physics timing telemetry is still needed
before changing the default; the default remains 120.

## Bounded eight-client comparison after integration

Two matched 60-second measured windows used eight clients, 10 ordinary edits/s,
the same two structural cuts, 30 seconds of warmup, and 3,600 retained timing
samples each. They are exploratory runs, not the 30-minute G4 soak.

| Settle window | Committed / requested | Deactivations / reactivations | Tick p95 / p99 | Physics p95 |
| ---: | ---: | ---: | ---: | ---: |
| 120 ticks | 608 / 608 | 0 / 0 | 14.077 / 16.809 ms | 0.507 ms |
| 20 ticks | 608 / 608 | 0 / 0 | 15.293 / 23.539 ms | 0.574 ms |

Both timing windows completed, and replay, restart recovery, and client hashes
agreed within each run. The configured timing verdict failed: tick p95 exceeded
the 12 ms target in both runs, and tick p99 exceeded 16.7 ms. The shorter
settle window did not improve cost in this lane. Detached bodies were not
asleep at the end (maximum reported speed was 400 m/s), so neither run
exercised the dormancy transition whose effect ENG-76 needs to measure. A
representative sustained run with genuinely settled rubble remains required.

## Checks

- `cargo test -p spall_sim dormancy --lib` — expected to cover the new cadence
  test, hard-wake behavior, moving-body guard, and deterministic caps.
- `cargo test -p spall_sim --test dormancy` — existing authoritative
  deactivation, body-edit wake, support-removal wake, proximity wake, and
  world-hash/conservation tests.
- `cargo xtask scenario --name sleep-wake-settle-20 --timeout-ms 120000` —
  networked override/wake check passed; see the measured result above.
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Measured networked G4 physics p95/p99 and the `<= 6 ms` target remain open
under ENG-76; ENG-80 should remain experimental/default-off pending that
alternating run.
