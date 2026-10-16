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

An edit uses a declared removal-only open-system rule. Removed source material
leaves with its instantaneous rigid-body velocity field. The body origin and
angular velocity stay fixed, while COM linear velocity changes by
`omega × (new_COM - old_COM)` so every surviving point keeps its world velocity.
The API returns the exact old-minus-new source-cell mass. Additions and material
replacement are rejected until an incoming momentum policy exists; the adapter
does not silently count them as removed material or manufacture their momentum.
Mass and inertia are rebuilt before shape mutation. An empty leaf part retires
only its collider and source ownership. A parent part with surviving descendants
is rejected before mutation because reparenting/frozen-pose metadata has not
been defined. Removing the final part retires the aggregate body. `PART`
hierarchy is pose structure only: there are no inferred cross-part bonds or
fracture connectivity. Selective fracture remains gated on an explicit
connectivity/bond policy and source-volume ownership rule.

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
| Trout / Native Voxels | 5 / 5 | 28.8–69.5 | 17.8–34.1 | 25.4–34.5 | 3.59–13.19 |
| Trout / Merged Cuboids | 5 / 17 | 20.1–23.2 | 14.7–29.1 | 17.1–22.3 | 1.90–3.12 |
| Crow / Native Voxels | 6 / 6 | 35.5–41.4 | 17.7–23.1 | 15.2–23.2 | 3.67–14.32 |
| Crow / Merged Cuboids | 6 / 14 | 16.9–22.7 | 22.4–25.4 | 14.0–17.5 | 2.18–4.70 |
| Deer / Native Voxels | 9 / 9 | 47.6–55.0 | 22.0–24.8 | 31.2–42.6 | 20.95–38.61 |
| Deer / Merged Cuboids | 9 / 21 | 28.2–30.3 | 23.6–32.6 | 20.0–21.6 | 3.87–4.77 |

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
invalid-density edit rejection before mutation, additions/material changes
rejected before mutation, both collider representations, exact mass deltas,
asymmetric rotating-body point-velocity and remaining/removed momentum checks,
empty leaf removal, final aggregate retirement, and rejected parent removal
with child query identity intact. A coordinator-added independent closed-form
oracle checks mass, COM, and every entry of the 3×3 inertia tensor for a rotated
negative-origin box plus a nested mixed-density cube under reversed input
order.

## Remaining gate

The bounded frozen-compound physics candidate is feasible and does not infer
cross-part bonds from `PART`. Its acceptance does not accept full-destruction
connectivity or selective detachment: disconnected source regions remain under
the same compound owner until an explicit connectivity and split-ownership
policy exists. The next launch prerequisite is to define connectivity/bond
semantics, stable multi-volume split ownership, and what happens to disconnected
cells. No hidden anchors or topology inferred from hierarchy are used.
No protocol, storage, UI, host lifecycle, weather-combined run, 1 km workload,
GPU capture, or population acceptance was performed. Those remain beyond this
bounded physics candidate and must be gated separately.

## Checks

- `cargo check -p spall_physics` — passed.
- `cargo test -p spall_physics --test articulated_review -- --nocapture` — 13 scenarios passed across both representations.
- `cargo test -p spall_physics --all-targets` — 90 passed, 1 ignored; 13 articulated regressions and the independent tensor oracle also passed.
- `cargo check -p sandbox --example animal-collision-probe` — passed.
- `cargo run --quiet -p sandbox --release --example animal-collision-probe -- --corpse-bounded` — 18 live-rig transfer/edit cases completed and raw JSON saved.
- `cargo fmt --package spall_physics --package sandbox -- --check` — passed.
- `cargo clippy -p spall_physics --all-targets -- -D warnings` — currently blocked by `needless_range_loop` in the separately committed coordinator test `tests/corpse_mass_review.rs:17`. `--lib` and `--test articulated_review` strict clippy checks pass.
