# ENG-118: loading and walking correction pass

The generated world was present on the server, but the window withheld solid
terrain and tree wood until an entire visible greedy-mesh pass finished. Soft
foliage arrived independently. The user saw floating plants for minutes before
ground/trunks appeared, severe backward jerks while walking, and about 50 FPS.
This follow-up corrects the live rendering path and camera presentation.

## Changes

- Publish four changed terrain chunks at a time, ordered nearest the player.
  Keep the request in flight until completion; restart from retained valid
  chunks when authoritative edits supersede the immutable snapshot.
- Cache empty mesh results. Skip exactly empty/enclosed bricks and sample
  surface bricks through a padded 34-cubed occupancy snapshot. Read tokens now
  include resident halo revisions, so digging into adjacent rock invalidates
  previously enclosed chunks. Pending-cut previews now exclude all 26 halo
  neighbours from caching, including AO edge/corner dependencies. Unknown, AO, face exposure and voxel ownership
  retain their original semantics.
- Build soft vegetation on a dedicated bounded worker from immutable frames.
  Upload its buffer only on a completed changed result. Omit fully enclosed
  opaque leaf cells and merge contiguous quarter-metre cells along X, retaining
  their material, occupied exterior, holes and shadow geometry. Rotated blades
  are retained individually.
- Draw solid chunks only inside the current pass's frustum. Shadow cascades
  use their own frusta, preserving off-camera shadow casters.
- Delay soft vegetation until its soil chunk is drawn; mature tree foliage also
  waits for its retained wood-tip chunks. Recheck after terrain batch uploads.
- Track the actually applied prediction correction separately for presentation.
  The camera eases ordinary corrections over 120 ms. Teleport-sized corrections
  (>1 m) are immediate. Server position, collision, replay and raw error/displacement
  measurements are unchanged. This does not fix every underlying reconciliation
  discrepancy.
- Add bounded real movement input to scripted captures: `move:SECONDS,X,Z`.
  Generated-world galleries now use the window's greedy terrain geometry.
  Original ENG-118 cube-builder galleries were reference art evidence, not
  evidence of live terrain loading performance; the earlier report is corrected.

## Measured evidence

All runs used this machine's debug build, seed 1, Showcase, 512 cells, autumn,
1280x720 window, uncapped FPS. These are bounded view/traversal measurements,
not the full-world performance gate or equivalent workload comparisons.

The user's captured baseline had a 21.67 ms average frame time over its last
400 samples (about 46 FPS), with 16.08 ms average submission time. A later
reproduction identified 430–445 ms render-thread foliage generation stalls.
A complete initial live greedy capture took 81,441.9 ms for 1,708 chunks even
after the first CPU meshing optimizations. Waiting for that full pass caused
the missing-ground presentation. The final live run published nearby chunks
while loading continued; its first nonempty HUD report had eight solid chunks
and a 322.7 ms mesh-worker measurement. Initial world generation/transfer still
has its own delay; this is not a measurement of click-to-play startup time.

A 90-second wait plus eight seconds of actual walking, with worker/compaction
and frustum culling, reached roughly 75–100 FPS in the tested view. The final
run added supporting-mesh gating and camera correction smoothing and used:

```powershell
cargo xtask play --worldgen showcase --seed 1 --worldgen-size 512 --season autumn --uncapped --shots "walk;wait:45;move:4,0,-1;move:4,0,1;wait:6;walk" --shots-dir .local/vegetation-perf-gated-shots --ticks 12000
```

Its late HUD reports were approximately 100–125 FPS. Last-20-second recorded
render timings: 2,663 frames, mean 9.074 ms, p95 11.700 ms, maximum 52.936 ms.
Those timings exclude some outside-render scheduling work; HUD FPS records the
frame cadence. Distant solid chunks were still loading (236 chunks near the end),
so these numbers cannot establish steady performance with all 1,708 chunks resident.
The first and final captures are in `ENG-118-loading-evidence/near-first.png`
and `after-walking.png`; the latter looks uphill after an actual descent/return
walk and includes the existing debug capsule overlay from scripted Walk mode.

The final traversal had 69 matched moving comparisons: raw error mean 0.001232 m,
p95 0 m, maximum 0.085 m. Across 1,063 reconciliations, maximum applied raw
displacement was 0.238799 m. Moving snapshot interval mean was 49.54 ms, p95
76 ms, maximum 144 ms. Earlier runs saw matched moving errors up to 0.225 m and
unmatched rebase displacements up to 0.358 m. Raw corrections still exist;
only their ordinary camera response is smoothed. The forward-motion regression
uses a 0.23 m backward correction and requires no camera reversal plus convergence
within 1 mm after stopping.

Gzip-compressed structured timing/correction logs, the earlier longer-run logs and final runtime
stdout are retained beside this report. `summary.json` records measured final
statistics. No processes belonging to the user's session were terminated.

## Validation

The first full check exposed an existing six-neighbour preview exclusion that
was insufficient for the newly complete AO halo read tokens. Extended it to
all 26 neighbours; the unchanged pending-cut restoration regression now passes
(`cargo test -p spall_client --lib pending_cut_remeshes_without_poisoning_the_mesh_cache`,
1 passed). The first failure is retained in `xtask-check-first-failure.log`.
Final `cargo xtask check` completed successfully (exit 0): 1,071 passed,
0 failed, 62 ignored, including workspace formatting, warnings-denied Clippy,
all-feature tests and documentation tests. `git diff --check` also passed.
The archived `xtask-check.log` is the final verification record. Earlier targeted checks passed:
`cargo test -p spall_mesh --lib` (41 tests),
`cargo test -p spall_render --lib instances::tests` (3 tests),
`cargo test -p spall_client --lib input_tests` (9 tests before the supporting-mesh
scenario was added), and
`cargo test -p sandbox --features client --test vegetation` (6 tests).
`cargo clippy --workspace --all-targets --all-features -- -D warnings` passed
before the final supporting-mesh helper/test extraction. The full gate validates
that final extraction and the 26-neighbour preview correction.

## Remaining risks and next task

Initial generation/transfer and full distant meshing still take time in debug;
large default/4,096-cell worlds and steady all-resident GPU/FPS gates were not
measured in this pass. Frame outliers and raw prediction discrepancies remain.
The existing water worker does not meet its overall real-time budget; this pass
does not close water feasibility gates. This is a measured improvement to the
reported live vegetation/loading path, not an overall engine performance signoff.
Next unblocked broader world-generation task: ENG-114. Its existing performance
and integration gates remain open. No push or publication is required.
