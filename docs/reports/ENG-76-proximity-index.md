# ENG-76 takeover: exact dormancy proximity indexing

2026-09-22. ENG-76 / T23 remains in progress pending the integrated evidence
below and the sustained gate. No dormancy tuning or physics policy is changed.

## Implementation

`DormancyPolicy::plan` previously tested each body against a linear list of
active-region spheres. With thousands of moving bodies, this becomes millions
of distance tests every tick, including ticks with no transitions. Shortening
`settle_ticks` does not remove that work.

A private per-pass spatial tree groups region centres and their maximum radius.
A query skips a group only when its closest possible sphere gap exceeds the
existing wake margin. Leaves run the exact original `sphere_gap <= margin`
predicate. Small and non-finite inputs retain the linear path. The index is
rebuilt from the immutable inputs on every pass; no cached world state needs
invalidation. Body processing order, caps, counters, hard wakes, proximity
hysteresis, sleeping/speed requirements, and the default 120-tick settle window
are unchanged. No dependencies, schemas, workload geometry, or collider policy
changed.

Tests compare indexed and exhaustive queries across random positions and radii,
large bodies, surface equality and either side of it, reversed input order,
empty/small sets, coincident centres, out-of-world coordinates, and non-finite
inputs. Existing simulation tests protect body edits, terrain support removal,
player proximity, conservation, and hash invariance.

## Historical ENG-76 diagnostic lane (base 84ec295)

One control followed by one indexed run, sequentially on this Windows host
(AMD Ryzen 7 9700X). Release builds, 8 local QUIC clients, 1,800 warmup ticks,
3,600 measured ticks, 5,700 total, default dormancy settings. Both runs used
`G:/Programming/voxel_engine/.local/eng76-g4-60s-rubble-default.json` unchanged.

| Measured result | Linear control | Indexed |
| --- | ---: | ---: |
| Tick p95 (target <=12 ms) | 13.8619 ms | 11.2659 ms |
| Tick p99 (target <=16.7 ms) | 16.9548 ms | 16.5258 ms |
| Physics p95 (target <=6 ms) | 0.5140 ms | 0.4901 ms |
| Committed edits | 608 | 608 |
| Dormancy deactivations/reactivations | 0 / 0 | 0 / 0 |
| Client hash agreement, exact replay, restart, reconnect | pass | pass |
| Timing verdict | fail | pass |

The old harness folds timing into its misleadingly named `all_hashes_match`;
the control's false aggregate is a timing failure, not hash disagreement.
Topology hashes can differ between runs because these are wall-clock scheduled
cuts against a non-lockstep physics simulation; each run independently passed
all replica/replay/recovery comparisons.

**Evidence limit:** this old `g4-workload` lane has bodies falling outside the
scene (final minimum body y around -30 km, maximum speed 400 m/s). It cannot
establish the real G4 sustained physics target. The prior claim of about 0.5 ms
physics p95 was from this narrower lane. Passing it must not close ENG-76 or T23.
No fixture was changed to obtain the improvement.

The synthetic 4,352-body / 4,352-region release microbenchmark, 60 samples with
index construction included, measured linear p95 **13.2542 ms** versus indexed
**0.7763 ms**, with exactly equal answers. This isolates proximity cost; it is
not a solver or G4 gate measurement.

Temporary owning-thread phase instrumentation sampled three ticks per 60.
In the indexed measured window (180 samples), dormancy mean/p95 was
0.654/1.238 ms; simulation 1.998/7.482 ms; replication 1.402/7.260 ms;
persistence 0.470/1.696 ms. These phase samples are diagnostic and phase-aligned,
not independent gate percentiles. Instrumentation was removed from the final
implementation; its patch is retained in `.local/eng76-profile.patch`.

Local evidence under `G:/Programming/voxel_engine/.local/`:

- `runs/eng76-takeover-profile/{server.summary.json,summary.json}` (control)
- `runs/eng76-takeover-indexed-profile/{server.summary.json,summary.json}`
- `eng76-indexed-profile.log` (sampled phases)

Exact diagnostic commands (from the isolated checkout at base 84ec295):

```powershell
$env:CARGO_TARGET_DIR='G:/Programming/voxel_engine/.local/target-eng76'
cargo xtask session --scenario G:/Programming/voxel_engine/.local/eng76-g4-60s-rubble-default.json --output G:/Programming/voxel_engine/.local/runs/eng76-takeover-profile --timeout-ms 240000
$env:RUST_LOG='eng76_profile=info'
cargo xtask session --scenario G:/Programming/voxel_engine/.local/eng76-g4-60s-rubble-default.json --output G:/Programming/voxel_engine/.local/runs/eng76-takeover-indexed-profile --timeout-ms 240000
cargo test -p spall_sim dormancy --lib
cargo test -p spall_sim --test dormancy
cargo test -p spall_sim --release measure_4352_body_proximity_against_linear -- --ignored --nocapture
```

The two dormancy test commands passed (12 unit + 6 integration; the explicit
measurement command ran the normally ignored benchmark).

## Integrated validation (base 07600f8)

The same code is applied on the existing integrated G4 branch; no historical
workload or policy changes are transplanted from the old worker. The unchanged
`t23-g4-integrated-2min-separated` fixture includes the yard, fresh body/terrain
edits, and the 64-brick giant collapse, 1,800 warmup + 7,200 measured ticks.

The first indexed run completed 9,420 ticks and all 1,213 edits with zero
rejections/unresolved edits. Client hashes, exact replay, cold recovery and
reconnect pass. Tick p95 **11.9050 ms** meets 12 ms in this run, but tick p99
**30.3015 ms** misses 16.7 ms and physics p95 **6.3859 ms** misses 6 ms. The
independent verdict reports only timing as failing. Dormancy is real here:
367 deactivations / 462 reactivations (the latter includes pre-existing dormant
bodies), unlike the old diagnostic lane's zero transitions.

The remaining p99 is attributable to terrain edits: all 116 terrain-only dig
ticks exceed 16.7 ms, with a 30.85 ms median. Mean nested components are
12.68 ms commit (4.96 occupancy extraction, 3.54 collider planning, 2.94 collider
publication), 8.09 ms structure index build, 3.86 ms dry run/reclassification,
and 3.08 ms Rapier stepping. Do not add parent and child spans together.
Ordinary ticks have 10.66 ms p95; body-cut ticks 12.21 ms. The solver itself
has 6.09 ms p95; pose extraction adds to the reported physics-phase time.
This change does not claim to solve those separate costs.

A same-revision linear control is being measured sequentially for comparison.

