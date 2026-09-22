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
