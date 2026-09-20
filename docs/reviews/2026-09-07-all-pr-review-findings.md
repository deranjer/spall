# Spall all-PR review findings

Reviewed 2026-09-07. Scope: all eleven GitHub pull requests visible for
`deranjer/spall`, their exact published heads, the unreviewed T10 base branch,
and the Voxel Engine project state in Loopira. This is a findings report, not an
acceptance or merge report. No PR was merged or modified during this pass.

## Repository and PR state

- PRs #1-#7 have green checks and are marked merged. The repaired heads for
  #1-#7 were rechecked against the earlier review plan.
- The stacked merges did not put T03, T04, or T07 into `origin/main`.
  `origin/main` was `6868b7f`; `72270cd`, `3dd4619`, and `03bd00a` were not its
  ancestors. This is tracked as ENG-33.
- PRs #9-#12 were open with green GitHub checks at review time.
- T10 at `da241b7` is the base of PR #12 but has no pull request among the
  eleven reviewed PRs. Its integration and review are included in ENG-33.

## Loopira bugs opened

All issues below are in project **Voxel Engine**, carry label **Bug**, and are
in backlog. Priority 1 is urgent, 2 high, and 3 medium.

### Urgent

- ENG-54 — Make authoritative edit commit failures fully atomic.
- ENG-55 — Apply occupancy-grid origin when installing and rebuilding voxel colliders.
- ENG-56 — Remove stale physics collision when an authoritative volume becomes empty.

### High

- ENG-33 — Integrate merged T03/T04/T07 stack into main and publish the missing T10 review.
- ENG-34 — Preserve the durable journal high-water mark after checkpoint retention.
- ENG-35 — Propagate final checkpoint failures instead of reporting a successful saved shutdown.
- ENG-36 — Stop automatic world resume or reinitialization when recovery reports corruption.
- ENG-37 — Verify checkpoint and replay hashes before accepting recovered world state.
- ENG-38 — Reconstruct journalled bodies with their authoritative mass and participant state.
- ENG-40 — Measure complete collider construction before accepting T06 feasibility.
- ENG-41 — Install fine-grid mixed-material mass properties into the actual physics body.
- ENG-42 — Replace active-body collider inflation with an exact bounded feasibility policy.
- ENG-43 — Stop reporting CPU readback and PNG encoding time as GPU timing.
- ENG-44 — Bound sparse-volume meshing by resident data rather than the full bounding hull.
- ENG-47 — Validate network action claims before creating authoritative edit intents.
- ENG-48 — Bound replication host queues and per-tick inbound work.
- ENG-49 — Prevent synthetic brick repairs from colliding with real transaction IDs.
- ENG-50 — Complete T16's bounded asynchronous persistence pipeline and durable pose cadence.
- ENG-59 — Validate saved world identity and algorithm versions before recovery.

### Medium

- ENG-39 — Resume simulation time at the durable journal suffix rather than the old checkpoint tick.
- ENG-45 — Make the depth debug view report linear camera-space depth.
- ENG-46 — Use requested capture dimensions for the camera aspect ratio.
- ENG-51 — Exercise abrupt process death and actual SQLite failures in the persistence gate.
- ENG-52 — Apply the negotiated bulk-stream guard to public raw stream access.
- ENG-53 — Keep datagram activity from masking a dead inbound control stream.
- ENG-57 — Replay the original action status for duplicate request IDs.
- ENG-58 — Add the missing editable-collider sleep and wake feasibility scenario.

## Verification

- GitHub PR inventory and exact heads fetched with `gh pr list`; every listed
  head had successful checks at review time.
- `cargo test -p spall_store -p spall_server --test persistence --test
  durability`: 18 passed, 0 failed.
- Focused T16 probes: 5 expected failures, reproducing journal sequence reset,
  unchecked checkpoint hash, wrong replayed body mass, stale recovered tick,
  and silent acceptance of reported corruption. Source and output are under
  `.local/reviews/2026-09-08/`.
- Focused PR #11 probes at exact `fc942b9`: 3 passed by asserting the observed
  defects (post-error mutation, collider origin displacement, and retained
  empty-body collider). Evidence is in the isolated review worktree under
  `.local/reviews/2026-09-08/worktrees/pr11-sim-probes/`.
- `cargo test -p spall_physics`: 16 passed. The release collision benchmark
  passed, but its reported build timer excludes preprocessing; ENG-40 tracks
  remeasurement.
- `cargo test -p spall_mesh -p spall_render`: 41 passed, one GPU test ignored.
  The ignored GPU test and a six-shape 320x240 capture passed on RTX 4080 SUPER
  / Vulkan. That capture also confirmed the timing field is not a GPU timestamp.

The current checkout acquired separate T17 work while this review was running.
Those changes and the pre-existing untracked review plan were preserved.
