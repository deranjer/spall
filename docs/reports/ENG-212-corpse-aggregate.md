# ENG-212 editable corpse aggregate feasibility

Status: **bounded candidate demonstrated; full lifecycle gate remains open**.
This work proves the physics-adapter path only. It does not accept host/runtime,
protocol, storage, or population interfaces and does not satisfy the 1 km
animal-world requirement.

## Representation, ownership, and mass

The candidate converts an existing ENG-210 articulated kinematic owner in one
synchronous physics-world operation into one dynamic Rapier body with exact
per-source-part colliders. It retains the same `VolumeId`s, part IDs, source
integer cells/materials/revisions, topology token, root pose and sampled death
poses. New colliders are prepared before ownership mutation; stale topology or
pose, a pending swept pose, invalid density/mass, duplicate ownership, or
aggregate-budget failure returns before the old rig changes. The conversion
removes the child kinematic bodies and replaces their colliders on the existing
root body without publishing an intermediate physics step with two owners.

Mass sums material-density times exact source-cell volume. Every cell also
contributes cube self-inertia and a parallel-axis term about combined COM,
yielding the full tensor for rotated/off-center parts. Overlaps count once per
source volume for mass even when their physical colliders geometrically
overlap; this additive rule is explicit. Native voxels and merged cuboids retain
the same exact source ownership. Admission/rebuild limits cover aggregate
dense and solid cell totals, part/shape bounds, and a 65,536 total merged
primitive cap. The geometry extent guard is 1,000 m; f32 coordinates at that
scale have roughly 0.061 mm spacing, so this is a finite-shape bound, not a
claim of exact precision at the limit.

On transfer, each occupied source cell uses its live part body's validated
linear/angular velocity and sampled world transform. The adapter computes
aggregate linear momentum and angular momentum about COM including each cube's
self-spin, then solves for the single rigid body's linear/angular velocity.
This conserves the source-cell sum of linear and angular momentum for the
sampled instantaneous motions. It does not conserve energy imposed by a
prescribed animation clip: kinematic motors may have added or removed energy
before the death pose, and those actuator impulses are not reconstructed.

An edit uses a declared open-system rule. Removed source material leaves with
its instantaneous rigid-body velocity field; the remaining aggregate keeps its
world pose and twist. Mass and inertia are rebuilt from the surviving cells
before shape mutation, while the prior twist is explicitly restored afterward.
The path makes no closed-system momentum claim across destructive removal and
does not infer energy from COM/tensor changes. Empty source parts retire only
their collider and ownership record. Removing the final part retires the single
aggregate body. `PART` hierarchy is pose structure only: there are no inferred
cross-part bonds or fracture connectivity. Fracture remains gated on an
explicit separate connectivity/bond policy and source-volume ownership rule.

## Authored-animal measurements

The release probe now creates an ENG-210 kinematic rig from an actual authored
asset pose, stages the sampled death pose, advances the physics step, and
converts that live owner. It checks source-hit identity, then runs 240 fixed
steps against a floor and removes one exposed occupied source cell with the
open-system edit. There are three sampled poses each for trout (136 cells, 5
parts), crow (217 cells, 6 parts), and deer (285 cells, 9 parts), under both
representations: 18 cases. Raw per-case source counts, tensors, death momenta,
collider/primitive counts, transfer/edit timings, contact steps, and query
results are in [ENG-212-corpse-candidate.json](ENG-212-corpse-candidate.json).

| Animal / representation | Colliders / primitives | Living attach μs | Transfer μs | Edit μs | Mean step μs |
|---|---:|---:|---:|---:|---:|
| Trout / Native Voxels | 5 / 5 | 27.0–78.4 | 16.9–32.2 | 21.4–29.5 | 3.50–12.75 |
| Trout / Merged Cuboids | 5 / 17 | 16.4–20.9 | 12.8–25.2 | 15.3–19.5 | 1.84–2.98 |
| Crow / Native Voxels | 6 / 6 | 34.5–39.2 | 17.1–25.3 | 13.9–17.1 | 3.63–13.94 |
| Crow / Merged Cuboids | 6 / 14 | 16.3–19.2 | 18.6–24.3 | 13.1–15.2 | 2.15–4.56 |
| Deer / Native Voxels | 9 / 9 | 46.9–53.0 | 21.1–24.2 | 30.5–37.0 | 19.00–38.39 |
| Deer / Merged Cuboids | 9 / 21 | 25.5–30.7 | 20.5–34.1 | 19.9–21.1 | 3.76–4.77 |

Across all 18 cases, every converted owner retained one dynamic body and all
source-part colliders; every source query identity check passed; every edit
removed exactly one cell and the removed cell no longer hit; surviving world
geometry did not jump when COM changed. Contact impulses occurred during
145–188 of the 240 steps. Still authored frames produced zero momentum;
animated samples produced nonzero measured linear/angular momentum. Values use
the probe's uniform 1,000 kg/m³ material assumption (33.203 kg trout, 53.223 kg
crow, 69.580 kg deer), not biological mass estimates. Timings are one optimized
local run, not percentiles or performance targets. No population acceptance
target exists.

Focused adapter regressions cover stale and pending transfer rejection leaving
the three-body source rig intact, successful one-body transfer with nonzero
sampled motion, post-transfer source queries, dynamic pose-update rejection,
invalid-density edit rejection before mutation, both collider representations,
source-cell edit and stale edit, empty-part removal and final aggregate
retirement. A rotated/off-center negative-origin source fixture checks
nonzero products of inertia. It does not independently establish every
analytic tensor entry at arbitrary orientations; the analytic mass oracle and
more extensive transformed random fixtures remain follow-up evidence.

## Remaining gate

Cross-part fracture/connectivity is deliberately not inferred from `PART`.
ENG-212 proves a frozen compound with intact source-part colliders and
whole-owner retirement, not selective detachment of bonded or unbonded parts.
If lifecycle requires cross-part fracture, the next decision must define
connectivity/bond semantics and stable ownership transfer before implementation.
No protocol, storage, UI, host lifecycle, weather-combined run, 1 km workload,
GPU capture, or population acceptance was performed. Those remain beyond this
bounded physics candidate and must be gated separately.

## Checks

- `cargo check -p spall_physics` — passed.
- `cargo test -p spall_physics --test articulated_review -- --nocapture` — 11 scenarios passed across both representations.
- `cargo test -p spall_physics --all-targets` — 90 passed, 1 ignored; rerun passed after adding the pending-pose regression.
- `cargo check -p sandbox --example animal-collision-probe` — passed.
- `cargo run --quiet -p sandbox --release --example animal-collision-probe -- --corpse-bounded` — 18 live-rig transfer/edit cases completed and raw JSON saved.
- `cargo fmt --package spall_physics --package sandbox -- --check` — passed.
- `cargo clippy -p spall_physics --all-targets -- -D warnings` — passed.
