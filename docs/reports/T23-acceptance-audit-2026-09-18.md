# T23 acceptance audit — 2026-09-18

Status: **not accepted**. T24 remains dependent on T23. This review compares
the merged `7d3ca00` tree and retained raw evidence with `docs/validation.md`,
not only the increment-completion labels in G3.md.

## Findings

1. **Checkpoint integrity regressed during integration.** The current
   `ResidencyPass::capture_checkpoint` cache-miss path loads an evicted brick
   without `EvictedBricks::verify_candidate`. The increment-36 error variant
   and regression test remain, but the verification call does not. The fresh
   residency suite fails its corruption regression (12 passed, 1 failed).
   Incremental capture legitimately reuses unchanged, previously encoded
   records; the regression must exercise a changed brick that actually reads
   backing, and separately prove safe reuse of an unchanged cached record.
2. **Baseline/repair integrity is unfinished.** `snapshot_world`,
   `baseline_volume`, and `logical_brick_repair_patch` also trust evicted
   backing content without comparing its revision/content with the retained
   digest. This violates the exact-revision capture boundary even though
   ordinary valid-backing scenarios converge.
3. **The G4 soak evidence is partial.** Retained
   `.local/runs/t23-g4-soak-2min/server.summary.json` and
   `.local/runs/t23-g4-soak-30min/server.summary.json` record 1,212 and 18,180
   committed transactions, respectively, with replay/restart agreement in
   their session summaries. Those are useful historical measurements, not
   proof of the complete G4 performance contract. Neither records server
   tick p95/p99, physics p95, process peak memory, reliable backlog age/bytes,
   or client frame time. Both have zero structural-split/large-collapse
   samples; their session summaries configure no latency targets.
4. **The workload is narrower than the required integrated gate.** The
   64-brick collapse is a separate ignored CPU test, not part of the
   eight-client soak. The sustained stream cycles over a short cell list;
   transaction count alone does not measure continuing rubble accumulation.
   The active debris falls into empty space. These choices are disclosed in
   the fixtures, but do not demonstrate the full interacting workload. The
   near-observer count is checked at spawn rather than throughout measurement;
   other debris starts at 400 m and 700 m offsets despite the 256 m terrain
   envelope (`spall_sim::fixtures::spawn_g4_workload_bodies`).
5. **Network and visual evidence remains incomplete.** Whole-run egress
   totals and west/east ratios do not measure the steady per-client ceiling,
   backlog recovery within five seconds of a blast, or the full workload at
   both required impairment envelopes. G2.md still lists complete-client
   timing, cross-GPU/human review, and a freeze decision as outstanding even
   though Loopira marks T15 done. This is an evidence/status discrepancy;
   this audit does not silently reopen T15 or invent an acceptance waiver.

The upstream character-controller limitation ENG-66 retains its previously
recorded spawn mitigation. Literal consolidation of dormant residency classes
is not an acceptance objective. Neither fact waives the findings above.

## Residency contract qualification

The pin/admission increments improve the live adapter but do not implement
all six frozen boundaries in G3-residency-hash.md. `serve` calls `sim.tick`
before `ResidencyPass::run`; swept pins therefore describe the preceding
movement, not a capacity reservation before dispatch and collision entry.
`pending_dependency_bricks` limits the brush/halo union to 4,096 bricks and
can fall back to just the brush centre. Reactive staging/commit guards still
prevent unavailable geometry from silently becoming air; they are not proof
of complete structural consumer-lifetime reservations.

Required reloads bypass both admission caps. Their pressure flag compares
brick count only, and does not defer affected operations when required dense
bytes cannot fit. The process peak covers aggregate allocations but does not
provide the required separate retained job, baseline/snapshot, or incremental
checkpoint-cache byte measurements. These are source-audit findings, not
newly reproduced physics corruption. A dependency-safe preflight design and
pressure tests remain necessary before declaring row 7 complete.

## Work in this pass

Luna workers have isolated assignments for checkpoint integrity, baseline
integrity, and warmup-excluded G4 server/physics measurement. The coordinator
owns review, integration, and Loopira changes. These are T23 work; the
requested T24 prototype remains blocked on its T23 dependency.

## Initial verification on the merged tree

- `cargo test -p spall_server --test residency_pass --test client_residency --test logical_baseline`:
  client residency 6/6 and logical baseline 4/4 pass; residency 12/13 passes,
  with the checkpoint digest-mismatch regression failing.
- `cargo test -p spall_sim --test logical_reload`: 3/3 pass.
- `cargo test -p spall_store --test abrupt_crash`: one parent test passes;
  the ignored test is its child-process entry point.
- `cargo xtask crash-test --suite persistence --output .local/runs/t23-current-audit-crash`:
  all 12 in-process scenarios pass. Raw summary is retained in that directory.
- `cargo xtask scenario --name t23-g3-residency-disk --output .local/runs/t23-current-audit-disk --timeout-ms 120000`:
  seven commits; server, four clients, replay, cold restart and reconnect
  agree on `cac2893cf5e76d866c9c1111b97fe0f822a86e9956f56f3d3ee3a8740d18e6de`.

Historical soak and GPU figures were not remeasured by this initial audit.
Next unblocked task: **T23 / ENG-30**, then T24 / ENG-31 once accepted.
