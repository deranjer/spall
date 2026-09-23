# ENG-75/76/79/80 — G4 representative rubble-lane measurement

Status: evidence-gathering only. No default changed (`DormancyConfig::DEFAULT`
stays at `settle_ticks = 120`; per-brick terrain colliders stay
experimental/default-off). Adds three reusable scenario fixtures and reports
what they measured; does not close any of the four tickets.

## Why the prior short comparisons weren't representative

ENG-76's own bounded eight-client comparison (two 60 s windows, settle=120 vs
settle=20) reported `0 / 0` dormancy deactivations/reactivations in both runs,
with a maximum reported body speed of 400 m/s — the active debris population
never came to rest, so neither run exercised the dormancy transition either
ticket needs to measure.

Reading `spawn_g4_workload_bodies` / `g4_workload_setup`
(`crates/spall_sim/src/fixtures.rs`) directly shows the 256 active debris
bodies *do* land on real floors: `G4_DEBRIS_FLOOR_MIN..MAX` and
`G4_NEAR_FLOOR_MIN..MAX`, added specifically because a 2026-09-18
acceptance-audit finding caught them free-falling into empty space outside
the declared envelope. That fix already existed on `main` before this
report — the 0/0 result was a property of those two *short* (90 s total)
exploratory runs not giving the population time to settle, not a broken
mechanism.

## New fixtures

Three scenario files, all reusing `t23-g4-soak-2min.json`'s real
`g4-workload` scene and gate rate (8 late-join clients, 10 ordinary edits/s +
one 4 m blast/10 s, 30 s warmup + 2-minute measured window) with
`dormancy: true` and a `server_timing` window added:

- `g4-dormancy-settle-120.json` — default 120-tick settle window.
- `g4-dormancy-settle-20.json` — 20-tick override (the window ENG-79's
  in-process policy sweep identified).
- `g4-dormancy-residency.json` — settle=120 plus `--residency-budget-bricks`
  (per-brick terrain colliders), tuned per the exploratory results below.

None carry `dormancy_assertions` or a hard-coded timing pass/fail — tick p99
is exactly the metric in question and is expected to sometimes miss target
at the default settle window (see below), so wiring these into an
automatic CI gate would make the gate itself flaky by design. They are
reproduction fixtures for `cargo xtask scenario`, not acceptance gates.

## ENG-75/76: settle=120 vs settle=20, repeated

One run each, then three more of each (sequential, not parallel — parallel
runs on one machine visibly perturbed timing in an earlier attempt). One
`settle-20` attempt hit a real `persistence: persistence backlog full: 64
jobs queued at capacity 64` event and terminated early before its timing
window completed; excluded as invalid data (most likely transient host
disk/IO contention from six heavy sessions back to back over ~20 minutes,
not a `settle_ticks` effect).

7 valid runs (4× settle=120, 3× settle=20):

| settle_ticks | tick p99 samples (ms) | mean | spread | misses 16.7 ms target |
| ---: | --- | ---: | ---: | :---: |
| 120 (default) | 18.18, 13.17, 13.38, 20.17 | 16.23 | 7.0 ms | 2 / 4 |
| 20 (override) | 14.39, 14.01, 14.10 | 14.17 | 0.38 ms | 0 / 3 |

tick p95 (9.0–9.5 ms settle=120, 8.5–10.1 ms settle=20) and physics p95
(0.39–0.45 ms both configs) stayed comfortably under target (12 ms / 6 ms)
in every valid run for both configs — not a discriminator.

**Dormancy deactivations/reactivations were `261 / 30` in every single valid
run, both configs, with zero variance.** This workload's disturbance is
dominated by the recurring blast (`sustained_edits`'s blast target sits at
`y=12`, right at the edge of the near-observer debris floor's `z`-range),
repeating every 10 s / 600 ticks — far longer than either settle window, so
the population fully re-settles between disturbances either way. `settle_ticks`
genuinely does not affect dormancy churn on this workload; whatever drives the
tick p99 difference above is not mediated by transition count.

The repeat turned a single-run coincidence into a repeat-confirmed pattern:
settle=120 is bimodal (roughly half its runs comfortably pass, half clearly
miss) for a reason not yet identified; settle=20 is both lower-mean and far
tighter. `n = 3–4` per config is still modest — this is a real lead, not
proof, and the actual mechanism (why settle=120 has high tick p99 variance)
is the open question, not just "which is faster." No default was changed
from this evidence.

## ENG-80: per-brick colliders vs the whole-terrain baseline

`--residency-budget-bricks` had never been run against the `g4-workload`
scene before this report. Two things worth establishing before trusting any
number from it:

1. `Simulation::ensure_terrain_brick_colliders` (`crates/spall_sim/src/world.rs`)
   switches the *entire* terrain to per-brick colliders as soon as the first
   residency-driven eviction is requested anywhere — not just the evicted
   region — so any working residency config exercises this ticket's actual
   question.
2. The "remaining active" debris floor (`G4_DEBRIS_FLOOR_MIN..MAX`, centred
   around `x ≈ 191 m`) sits roughly 170–200 m from every player spawn — far
   outside any reasonable interest radius — so it was a real open question
   whether its terrain gets evicted and what happens to the debris resting
   on it.

### Exploratory tuning

Three short (3000-tick, 4-cut) smoke runs at different configs, all
converged and passed (hash agreement, replay, and restart all matched at
every config — no correctness break found at any point):

| `residency_radius_bricks` | `residency_budget_bricks` | `required_over_budget_ticks` (of 3000) | dormancy deact/react |
| ---: | ---: | ---: | ---: |
| 2 | 12 | 3000 (100%) | 57 / 18 |
| 3 | 28 | 3000 (100%) | 117 / 18 |
| 0 | 16 | 5 (0.2%) | 1 / 0 |

`residency_pass.rs` computes `required_bricks = interest.union(&pinned).count()`;
a loose interest radius (2–3 bricks — a guess made before checking the
existing reference scenarios) inflates that set past any reasonable budget
regardless of size, independent of what the scene actually needs resident.
Retuning to `radius = 0` (matching what `t23-g3-residency.json` and
`t23-g3-traversal.json` already use) dropped `required_over_budget_ticks` to
near zero and dormancy churn by two orders of magnitude in the same short
run. This strongly suggests **residency-radius tuning, not the per-brick
collider mechanism itself, may be what drove ENG-80's original "20-30x
churn" finding** — that finding's own radius isn't recorded in this
project's history, so this is a lead, not a refutation.

### Full comparison

`g4-dormancy-residency.json` (radius=0, budget=16, otherwise identical to
`g4-dormancy-settle-120.json`) against the whole-terrain settle=120 baseline
(n=4 average from the table above):

| | whole-terrain (n=4 avg) | per-brick residency (n=1) |
| --- | ---: | ---: |
| tick p95 | 9.14 ms | 10.65 ms |
| tick p99 | 16.23 ms (13.2–20.2 range) | 15.31 ms |
| physics p95 | 0.42 ms | 0.92 ms |
| physics p99 | ~0.65 ms | 1.39 ms |
| dormancy deact / react | 261 / 30 | 115 / 76 (191 total vs 291) |
| requirements_met | 2/4 runs | true |
| hash convergence | yes | yes |

At this tuned config: a modest tick p95 increase, physics p95/p99 roughly
2–3x higher but still comfortably under the 6 ms target, and a *different*
(not dramatically worse) dormancy pattern — more reactivations, fewer
deactivations, similar total transition volume. This is one run, not yet
repeated with the same rigor as the settle-window comparison above; treat it
as a promising single data point, not adoption evidence.

## Recommendations

- Do not change `DormancyConfig::DEFAULT.settle_ticks` from this evidence.
  Dormancy churn is identical at 120 and 20 ticks on this workload; the
  tick p99 difference is a repeat-confirmed lead but its mechanism is
  unexplained.
- Per-brick terrain colliders remain experimental/default-off. The original
  blocking "20-30x churn" concern did not reproduce at a properly-tuned
  residency radius in this report's testing — investigate whether radius was
  the actual variable in the original finding before treating churn as a
  fixed cost of the representation.
- Next unblocked work: recover the original ENG-80 high-churn run's exact
  residency radius/budget and workload, then reproduce those values at full
  horizon. The current report's tuned configuration now has three repeats,
  and its controlled radius=2 full-horizon run is recorded below. Separately,
  investigate why settle=120's tick p99 is bimodal.

## ENG-80: sequential full-horizon repeat (2026-09-23)

The exact three fixtures above were run sequentially in a dedicated worktree
at `38d426f` plus this report/fixture commit `5acfaea` (the fixture commit was
cherry-picked into the worktree as `c2afb45`). No engine source or default was
changed. The report's `g4-dormancy-residency.json` remained byte-identical;
the lockfile SHA-256 was
`DCC3E3D3F15288A651F8C5548743E1BBFFB5BDAEEEF1DC3EF99F520485188883`.
The sessions were run one at a time after the other worker's targeted checks
finished; the release binaries were built on the baseline invocation and
reused thereafter. Host was Windows NT 10.0.26200, 16 logical processors as
reported by the runtime. Windows denied access to the processor model, so the
CPU model could not be recorded. No competing agent builds or timed runs
overlapped this sequence; other interactive desktop processes were not
controlled, so this is sequential evidence, not an isolated benchmark host.

Commands used (every command exited successfully except tuned run 01, which
returned exit 1 for its configured server-timing target miss):

```text
cargo xtask scenario --name g4-dormancy-settle-120 --timeout-ms 400000 --output .local/runs/eng80-luna/baseline-run-01
cargo xtask scenario --name g4-dormancy-residency --timeout-ms 400000 --output .local/runs/eng80-luna/tuned-run-01
cargo xtask scenario --name g4-dormancy-residency --timeout-ms 400000 --output .local/runs/eng80-luna/tuned-run-02
cargo xtask scenario --name g4-dormancy-residency --timeout-ms 400000 --output .local/runs/eng80-luna/tuned-run-03
cargo xtask scenario --name g4-dormancy-residency-radius2 --timeout-ms 400000 --output .local/runs/eng80-luna/loose-radius2-run-01
```

The baseline is a fresh one-run comparison against the report's earlier
whole-terrain n=4 set. The radius=2 run is a controlled full-horizon fixture
that keeps the tuned fixture's budget=16, settle=120, workload and timing
window unchanged, changing only the residency radius from 0 to 2.
`fixtures/scenarios/g4-dormancy-residency-radius2.json` records that exact
configuration for reproduction.

| Run / configuration | session result | Tick p95 / p99 (ms) | Physics p95 / p99 (ms) | Dormancy deact / react | Residency evictions / reloads | Required-over-budget ticks / budget misses | Topology, replay, restart |
| --- | :---: | ---: | ---: | ---: | ---: | ---: | :---: |
| Fresh whole-terrain baseline, settle=120 | pass | 9.7478 / 15.3598 | 0.4117 / 0.6338 | 261 / 30 | 0 / 0 | 0 / 0 | all match |
| Per-brick, radius=0 budget=16, run 01 | **timing miss** | 12.6228 / 18.3999 | 0.8923 / 1.5258 | 115 / 76 | 3,805 / 2,189 | 530 / 27 | all match |
| Per-brick, radius=0 budget=16, run 02 | pass | 9.9968 / 12.7466 | 0.7906 / 1.2802 | 115 / 76 | 3,961 / 2,228 | 569 / 27 | all match |
| Per-brick, radius=0 budget=16, run 03 | pass | 10.7632 / 15.4773 | 0.9522 / 1.4615 | 115 / 76 | 3,805 / 2,189 | 530 / 27 | all match |
| Per-brick, radius=2 budget=16, run 01 | pass | 10.7887 / 15.8373 | 0.8216 / 1.3901 | 144 / 105 | 1,557 / 1,554 | 9,139 / 8,880 | all match |

Each run completed the configured 7,200-sample measured timing window and
committed 1,212 transactions. The three tuned-radius runs had the same
115/76 dormancy counts despite differing timing: mean tick p95/p99 was
11.1276/15.5413 ms (ranges 9.9968–12.6228 / 12.7466–18.3999 ms); mean
physics p95/p99 was 0.8784/1.4225 ms. Two of three met all configured timing
thresholds; run 01 missed tick p95 <=12 ms and p99 <=16.7 ms. That session's
eight clients each matched the server hash, and replay, cold-restart hash,
and reconnect hash all matched. Its top-level session result was `failed`
solely because `server_timing.requirements_met` was false; the harness's
`all_hashes_match` summary field is also false on that aggregate failure even
though the per-client, replay, and recovery hashes match. Do not count this as
a topology or recovery failure, and do count it as a timing target miss.

The radius=2 control passed hash agreement, replay, cold restart and reconnect,
and its timing targets. It measured 249 total dormancy transitions versus
191 at radius=0 (+30%), while the default whole-terrain baseline measured
291. Meanwhile the radius=2 required set exceeded budget on all 9,139 server
ticks, with 8,880 budget-miss ticks; the tuned radius=0 runs recorded 530–569
required-over-budget ticks and 27 budget misses. This controlled run supports
radius as a contributor to residency admission pressure and a moderate change
in churn. It does **not** reproduce the historical 20–30x churn claim, nor
prove radius alone caused that claim: the original comparison's radius and
matched settings remain unknown, and the short earlier runs used differing
budgets. The old claim needs its original config/logs or a better-matched
reproduction before it can be retired.

All output folders are under `.local/runs/eng80-luna/` in the measurement
worktree and contain `summary.json`, `server.summary.json`, process JSONL
logs, and run databases. The per-run server summary SHA-256 values are:

| Output | SHA-256 of `server.summary.json` |
| --- | --- |
| `baseline-run-01` | `B44AB46B2B3FE454E3AA8789011D4F7873286E2317D5BEF280F63688AFA98722` |
| `tuned-run-01` | `6F2462EB54B641698702442F131B8B368D5A052F635370DDB5452FB95988CC28` |
| `tuned-run-02` | `A9555F93438113D6758FDD57D900EF9C69205825F47F3147283C6A0FB2B5C499` |
| `tuned-run-03` | `AB2F41E07F71ECB660AD5D6FD7734EA331A92A97F78E31973E391FB5E43EABBC` |
| `loose-radius2-run-01` | `D04FF4F45B8E23611A8F7CFCC0B3BD44DCE0738C830B3D477DCC8BDDF6819CBF` |

These results improve the evidence but do not qualify adoption: there are
only three tuned repeats on one desktop host, timing varies materially, and
the matched radius control is only one run. Per-brick colliders remain
experimental/default-off pending stronger repeated evidence and resolution
of the original high-churn configuration.

## Checks

```sh
cargo xtask scenario --name g4-dormancy-settle-120 --timeout-ms 400000 --output .local/runs/g4-dormancy-settle-120
cargo xtask scenario --name g4-dormancy-settle-20 --timeout-ms 400000 --output .local/runs/g4-dormancy-settle-20
cargo xtask scenario --name g4-dormancy-residency --timeout-ms 400000 --output .local/runs/g4-dormancy-residency
```

Each is a real 8-client QUIC session over the full 9300-tick (~2.6-minute
real-time-paced) G4 workload; `release_profile: true` builds release binaries
first. `server_timing` and `dormancy_deactivations_total` /
`dormancy_reactivations_total` are in the run's `summary.json` /
`server.summary.json`.

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
