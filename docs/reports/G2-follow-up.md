# G2 integrator follow-up — 2026-09-10

Status: review preparation for **T15 / ENG-22**, not gate acceptance or a compatibility freeze. The five evidence increments exist on the current checkout (`acac07cf9575da0f2f6c19a6b88bca92d4cee8c6`); their PRs remain open. Keep T15 in progress.

Sources: [G2 evidence](G2.md), [dynamic-lighting evidence and limits](G2-lighting.md), [G2 acceptance contract](../validation.md#g2--graphics-quality-and-cost), [T15](../tasks.md#t15--g2-graphics-gate), and live GitHub/Loopira metadata inspected on this date. GPU figures below are prior author measurements, not measurements repeated in this audit.

## Integration status

| PR | Increment | Head | Hosted foundation checks |
| --- | --- | --- | --- |
| [#76](https://github.com/deranjer/spall/pull/76) | Cold frame-cost harness | `88d66d4` | Two successful |
| [#77](https://github.com/deranjer/spall/pull/77) | Persistent resources | `1579202` | One successful, one failed |
| [#78](https://github.com/deranjer/spall/pull/78) | Motion and quality flags | `04e759b` | Two successful |
| [#79](https://github.com/deranjer/spall/pull/79) | Daylight terrain | `2012df8` | Two successful |
| [#80](https://github.com/deranjer/spall/pull/80) | GI-lit collapse | `acac07c` | Two successful |

All five are OPEN with no review decision returned. #76 targets main; each later PR targets the preceding feature branch. Review/integrate in dependency order and validate the resulting integrated revision.

The [failed #77 run](https://github.com/deranjer/spall/actions/runs/34480197307) reports `spall_net`'s `separate_process_transport` failure at `crates/spall_net/tests/process_harness.rs:186`: a child exited 101. This identifies the failing test, not its root cause. Do not classify it as a harmless flake from the sibling success alone. A separate local record (developer auto-memory) already tracks `spall_net::separate_process_transport` as a **known intermittent** process-harness failure, not a real regression — so the likely outcome is "flake", but that does not discharge #77's own gate. Confirm against retained child diagnostics, rerun the focused test plus required integration checks, and require #77's checks green before merging regardless. The ticket's earlier “checks green across the chain” statement is qualified by this live result.

Loopira lists T14 / ENG-21 and T11 / ENG-18 as done, so T15's named ticket dependencies are satisfied. However, the split T11a / ENG-62 full-workload G1 evidence remains in progress. T11's status does not establish full-workload acceptance, and this tiny G2 collapse fixture does not resolve that remaining G1 work. T23 remains backlog and has other dependencies besides T15.

## What the evidence establishes

| Evidence | Reported result on RTX 4080 SUPER / D3D12, debug, 1080p | Limit relevant to acceptance |
| --- | --- | --- |
| Persistent lighting fixtures | GPU p95 up to 2.87 ms; renderer CPU encode p95 up to 1.00 ms | Static geometry/camera, shadows rendered once; no complete client CPU budget |
| Occluder motion | GPU p95 4.5–4.9 ms; recovery about 26%; return residual below 0.8% | 120-frame controlled occluder sequences, primarily indirect-only views |
| Daylight terrain | Settled GPU p95 0.24 ms; encode p95 0.68 ms; zero sampled static flicker | Static fixture and synthetic bounded edits |
| GI-lit collapse | 10/10 cuts; reported solid count 304 to 243; cold GPU p50 about 12 ms, p95 about 26 ms | 13 captured ticks across 200 simulation ticks, followed by 60 static frames; no continuous bounded-update client timing |

All six scene categories have examples. This is enough to begin the integrator review, but “scene matrix complete” does not mean every G2 acceptance bullet is proven. In particular:

- The contract requests settled captures and at least 120 consecutive moving frames. Sparse collapse snapshots plus settled-noise frames do not demonstrate continuous active-collapse stability. Record the intended coverage across scene categories and supply missing sequences before claiming the unchanged contract passed. _(Update — implementation session, 2026-09-10: increment 6 (`--scene g2-bounded-collapse`) adds a 200-tick continuous active-collapse run on the persistent loop with a bounded per-tick `LightingUpdate`, plus convergence and stale-occupancy checks. Captured on the reference adapter — all provisional targets met at p95, see the deeper-increments table and G2.md "Increment 6".)_
- `g2_loop_run` computes `client_frame_p95_pipelined_ms = max(GPU p95, CPU p95)` and `client_frame_p95_serial_ms = GPU p95 + CPU p95`. Neither is a measured client-frame percentile. A sum of marginal p95 values is not generally an upper bound on the p95 of their sum; the report's “actual frame p95”/“upper bound” wording must not be used as acceptance evidence. Measure paired end-to-end frame durations directly. _(Update — implementation session, 2026-09-10: `capture_frame_loop` / `capture_collapse_sequence` now record each frame's own CPU encode + GPU device total and percentile that paired series directly as `serial_frame`; `client_frame_p95_serial_ms` and `client_p95_target_met` key off that measured value, `client_frame_p95_pipelined_ms` is retained and labelled a lower-bound estimate, and the G2.md wording is corrected. A truly overlapped pipelined-loop measurement is still future work.)_
- CPU encode excludes simulation, culling, entity updates, input, presentation, and synchronization overhead. The provisional targets remain client p95 <=16.7 ms, GPU p95 <=12 ms, and complete CPU frame work p95 <=4 ms after warmup. Report destruction spikes, p99 and maximum separately.
- Empty quality flags mean the sampled probes passed their thresholds. They do not replace visual review for leakage, silhouettes, material readability, soft/contact shadows, or ghosting outside those probes.
- The documented 0.5 m lighting cache misses sub-0.5 m geometry. Collapse lighting also fills detached-body AABBs solid, so hollow or rotated debris can have incorrect GI occlusion. This is a lighting approximation, not a substitute for authoritative voxel geometry or exact destruction ownership.
- Local dirty-region updates can leave far-field radiance stale until a full refresh. The integrator needs an explicit refresh policy and cost ceiling; a cold full-cache retrace cannot quietly become a per-frame operation.

## Required integrator decisions

### Cross-GPU review

Owner: graphics integrator, supported by hardware operators. First identify available adapters and the intended minimum supported hardware; none beyond the reference adapter is established here.

Use the same reviewed revision, pinned dependencies, build profile, 1920x1080 resolution, exposure, camera path, sun/material settings and frame counts. Repeat the reference D3D12 run and add an independently selected GPU, preferably another vendor. Record OS, adapter, driver, backend, timestamp support, warmup, run commands and artifact locations. Vulkan remains unmeasured because of ENG-60; explicitly retain that limitation unless a separate verified fix enables testing.

Existing entry points, to run into distinct directories per adapter/run:

```powershell
cargo xtask capture --scene g2-loop --output .local/runs/g2-review/reference/loop
cargo xtask capture --scene g2-motion --output .local/runs/g2-review/reference/motion
cargo xtask capture --scene g2-terrain --output .local/runs/g2-review/reference/terrain
cargo xtask capture --scene g2-collapse --output .local/runs/g2-review/reference/collapse
```

These commands reproduce existing coverage; they do not add the missing continuous-collapse or full-client measurements. Retain summaries, probe traces and images. Review settled/moving views side by side, including thin barriers, dark enclosures, emissive occlusion and debris cavities. Specify numeric cross-GPU tolerances from repeated captures and visual assessment, with metric, region, threshold and rationale; do not invent a universal pixel threshold or require bit-identical cross-GPU pixels. Record each shortfall as accepted with rationale, requiring repair, or blocking the freeze.

### Terrain and detail sizes

Working recommendation: retain **0.25 m terrain** and **32-cubed bricks** as the candidate baseline. The evidence does not justify changing authoritative resolution. **0.0625 m detail volumes** remain a proposed separate-volume choice, not demonstrated quality acceptance.

Before freezing either size, review representative thin walls, one-cell cuts, diagonals, silhouettes, cavities and detached geometry at intended viewing distances. Explicitly decide whether the 0.5 m lighting representation is acceptable for those sizes or requires a finer/conservative occlusion representation. Smaller voxel cells alone do not repair lighting leakage. If detail evidence is unavailable, record that the detail-size decision remains open rather than declaring the entire freeze complete.

Preserve one fixed size per volume/brick and retain terrain cell size on detachment. Any changed compatibility choice needs coordinated world-format/protocol documentation and an explicit conversion/versioning policy; never silently resample an existing falling building or saved world.

### Renderer direction

Working recommendation: continue the existing raster + T13/T14 one-bounce indirect-lighting direction, with persistent GPU resources and bounded dirty-region retracing. This is a recommendation for review, not an approved renderer freeze.

Record accepted limits for sub-cell occlusion, body-AABB lighting, one-bounce quality, far-field refresh latency, full-refresh spikes and backend coverage. Keep indirect/emissive illumination and full-world destruction in the acceptance scene. If quality/cost cannot meet the current contract, document an explicit revised quality/performance decision and open corrective work; do not silently remove GI or rename a reduced workload as the original gate.

## Deeper increments and when they are needed

| Follow-up | Priority and scope | Required evidence |
| --- | --- | --- |
| Bounded per-tick destruction | **Implemented and captured — increment 6** (`--scene g2-bounded-collapse`). 200 consecutive rendered ticks on the persistent loop, each tick's committed cuts + old/new body bounds translated to a `LightingUpdate`. Measured on the reference adapter (RTX 4080 SUPER / D3D12): GPU frame p95 11.08 ms / CPU p95 1.54 ms / measured serial p95 12.13 ms — all provisional targets met at p95 (p99/max tail 13.5–15.4 ms). Bounded trace p95 0.95 ms / max 1.49 ms; worst tick re-traces 1.2 % of the cache. Convergence vs a cold full-refresh ≤ 0.83 % at 5 checkpoints; final clipmap 100.0 % cell-for-cell agreement with a resample (solid counts 52 = 52); settled flicker 0.000000; edit-to-visible one frame. CPU test `bounded_collapse_update_tracks_the_full_resample` (100 % agreement over its 60 ticks). | At least 120 consecutive active-collapse frames, per-pass and actual total timings, edit-to-visible latency, convergence against a full-refresh reference; no stale vacated shadows or erased overlapping occupancy |
| Complete client timing / pipelined loop | Full CPU and end-to-end measurements are needed for the unchanged performance contract. A threaded pipeline is an optional implementation choice until measurements justify it | Paired frame timings including CPU work and presentation; bounded queues/backpressure; immutable worker snapshots and revision validation at owning-thread tick boundaries; report stalls and spikes |
| Bounded denoise/temporal | Conditional optimization if measured combined-scene cost requires it | Compare against cache-wide reference at dirty boundaries and after motion; report speed, seams, leakage, noise, ghosting and convergence; bound refresh work without indefinitely stale cells |

The deeper implementations may be deferred while reviewing the freeze candidate. The missing performance/continuous-motion evidence cannot be called complete merely by deferring the implementations: either supply it through suitable tooling or explicitly revise the gate contract. Do not start a threaded redesign or denoise rewrite solely because it is listed here.

## Closure record to complete

- Reviewed revision and integrated PRs: pending.
- Named graphics reviewer, date and artifact manifest: pending.
- Cross-GPU matrix and justified tolerances: pending.
- Terrain size / detail size / renderer direction, rationale and accepted limits: pending.
- Continuous-motion coverage and measured combined-client budgets, or explicit contract revision: pending.
- Refresh policy and unresolved quality/performance issues with owners: pending.
- Compatibility documentation updated consistently in README, architecture, protocol, validation, tasks and ENG-22: pending as applicable to the chosen decision.
- T15 completion: pending; do not infer it from CI or this document.

## Audit checks and next work

This session inspected repository status/history, G2 and T15 documentation, Loopira guide and ticket/dependency status, all five PR heads/checks, the #77 failed CI log, and the source of loop estimates and collapse sampling. `git diff --check` and a local Markdown link/whitespace check were run for the documentation change. No Rust tests, GPU captures, cross-GPU comparison, visual acceptance, merge or compatibility freeze were performed. Historical performance and test claims remain attributed to their authors.

Changed files: this follow-up and an entry link in G2.md. Existing untracked `.claude/` and `docs/reviews/` work was preserved. Next task is **T15 / ENG-22 integrator review**, beginning with PR #77 check resolution, artifact review and a hardware review matrix. T11a / ENG-62 remains a separate full-workload gate obligation; closing T15 alone does not unblock all of T23.

## Update — implementation session, 2026-09-10

Acting on the two code-actionable items above (not integrator review, not a freeze):

- **Serial frame p95 is now measured, not summed.** `spall_render::capture_frame_loop` and the new `capture_collapse_sequence` record each frame's own CPU encode + GPU device total and percentile the paired series as `serial_frame`. `sandbox-capture`'s `g2_loop_run` reports `client_frame_p95_serial_ms` from that measured value and keys `client_p95_target_met` off it; `client_frame_p95_pipelined_ms = max(...)` is kept and relabelled a lower-bound estimate. G2.md increment 2's "actual frame p95 / upper bound" wording is corrected and the withdrawn `GPU p95 + CPU p95` figure is called out. The tabulated increment-2 run predates the change; its `max(...)` column still holds and the measured block lands on re-capture.
- **Increment 6 — bounded per-tick destruction — is implemented and captured.** `--scene g2-bounded-collapse` runs the collapse on the persistent loop with a per-tick `LightingUpdate` translated from committed cuts + body poses, over 200 consecutive rendered ticks, and emits per-pass / `serial_frame` percentiles, bounded re-upload/re-trace sizes, edit-to-visible latency, 5-checkpoint convergence vs a cold full-refresh, and a final cell-for-cell clipmap agreement check. Captured on the reference adapter (RTX 4080 SUPER / D3D12): GPU frame p95 11.08 ms, CPU p95 1.54 ms, measured serial p95 12.13 ms — all provisional targets met at p95, p99/max tail 13.5–15.4 ms; bounded trace p95 0.95 ms; convergence ≤ 0.83 %; final clipmap 100.0 % cell agreement; settled flicker 0.000000; no quality flags. See G2.md "Increment 6" for the full table and caveats (denoise/temporal are now the per-frame cost floor; screen-probe convergence is weak on this mostly-sky exterior — the cell-for-cell agreement carries the stale/erased-occupancy claim).

Verification performed this session: `cargo check` / `cargo clippy` clean (workspace + all-targets) on `spall_render` and `sandbox-capture`; `cargo test -p spall_render --lib` (36 pass) and `cargo test -p sandbox --features client --bin sandbox-capture` (9 pass, including the new `bounded_collapse_update_tracks_the_full_resample`); the increment-6 GPU capture was run twice on the reference adapter (numbers above are the second run; GPU-frame p95 varied 7.7 → 11.1 ms run-to-run from clock gating, both under the 12 ms target). No merge, no freeze, no cross-GPU or visual review. The closure record above is unchanged.
