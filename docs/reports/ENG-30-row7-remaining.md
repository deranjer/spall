# ENG-30 row 7: remaining acceptance work

Coordinator review, 2026-09-18. This is a contract-to-code audit, not a newly
reproduced runtime failure or an acceptance waiver.

The historical notes calling controller unification an optional follow-up do
not close this row. The [post-merge review](../reviews/2026-09-10-eng-30-post-merge.md)
explicitly rejected that interpretation. The six serve-loop boundaries and
memory requirements in [the frozen residency contract](G3-residency-hash.md)
remain the acceptance criteria.

Literal class consolidation is not the objective. `ResidencyPass` and
`ClientResidencyPass` are the live adapters; the older controller's tested
policy can be reused without moving storage dependencies into simulation.

## Confirmed remaining gaps

- `spall_server::ResidencyPass::run` receives player feet and runs after the
  simulation step. Its interest set does not explicitly reserve or pin the
  union of pending edit, structural, and swept-collision dependencies required
  by the contract. Reactive `EvictedGeometryRequired` reload/retry is useful
  existing protection, but is not evidence for the complete preflight and
  consumer-lifetime pinning contract.
- `ResidencyLimits::budget_bricks` is documented as a reported ceiling. Loads
  happen before an `over_budget` count; the live adapter has no dense-byte
  admission limit. A measured dense-byte total does not enforce capacity or
  prove bounded operation under pressure.
- The summaries count resident dense payload and disk footprint, not the
  complete retained memory required by the frozen contract: digests, retained
  snapshots/jobs, capture buffers, backing memory, and process peak memory.

These findings do not establish that an evicted voxel is currently sampled as
air by physics: derived colliders and reload guards must be considered too.
Swept-collision and delayed-repair admission need explicit integrated evidence.

## Preserve delivered behavior

Atomic digest validation, reload/retry, synchronous capture-before-evict,
disk-backed restart, bounded client reload requests, and checkpoint capture
without reloading the live cache have landed. Do not reopen those fixes merely
because they use a different API shape than the old controller. In particular,
a boolean synchronous capture result is not by itself proof that exact-revision
acknowledgement is missing; any API replacement needs a demonstrated need.

## Next bounded implementation pass

Keep the live adapters and use the existing `spall_voxel::ResidencyCache` policy
where its semantics fit. First define ownership of geometry reservations and
pins across stage, commit/rejection, capture, and collision use; reconcile
multi-player union interest with the existing hysteresis. Then enforce brick
and dense-byte admission, explicitly deferring work when the required pinned
set cannot fit. Preserve the existing logical digest and authoritative repair
lifecycle throughout.

Acceptance needs pending-edit/structural pin-lifetime tests, swept-collision
and impaired client repair tests, hard brick/dense capacity pressure tests,
and paired residency-on/off scenarios with identical topology/replay/recovery.
Add retained-memory and blocked-load/pin metrics to the scenario assertions.
Do not replace the full workload with a smaller fixture under its original
name, or close the row from controller-only unit tests.

Next unblocked task: **T23 / ENG-30 row 7**. T24 and T25 retain their T23 dependency.
