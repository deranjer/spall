# ENG-210 articulated movement and response feasibility

## Candidate and scope

The experimental candidate uses one position-based kinematic Rapier body per
authored source part, attached to the existing `PhysicsWorld` pipeline. All
part bodies and exact source colliders share one conceptual `BodyId` owner;
each collider remains bound to its `part_id`, `VolumeId`, source revision,
integer occupancy grid, and material ownership. Complete pose sets validate
all IDs/transforms and schedule all part bodies together. Query pose tokens and
source transforms publish only after the next fixed step. No SPVX parent link
is converted into an impulse or multibody joint, and there are no per-voxel
bodies or second solver.

This is a kinematic clip rig, not a ragdoll. The dynamic probe accepts impulses
from its moving source parts, but the animal does not recoil or yield to an
impact. Rapier's kinematic-fixed active collision pair is enabled for the
candidate, though terrain contacts do not alter prescribed kinematic targets.
Root translation therefore goes through an explicit conservative movement
method. It sweeps a sphere enclosing the current source-part bounds against
the live Rapier query world, clips the requested translation at the first
shape-cast hit, and schedules the clipped root plus every part for the same
step. The bound is conservative and can reject a clear narrow passage. Root
rotation is not swept: direct candidate root teleports/targets are rejected;
the movement adapter currently accepts translation at the current root
orientation only. Animated part targets are also not swept against terrain.

The outer movement caller supplies gravity as a translation request. The rig
itself is not a dynamically falling body. The support-removal scenario
integrates gravity in the caller, sweeps it against terrain, removes the fixed
floor collider, and continues the same fall requests. This exercises support
loss through the actual solver query world without adding a hidden support
anchor or pretending the kinematic rig has dynamic mass.

## Contact evidence

The unchanged 0.6 m / 60-step probe path now uses a kinematic rigid body for
the animated source part. Its solved impulse matches the original
root-controlled result and materially exceeds ENG-209's local-collider result.

| Representation | Candidate probe X | Candidate impulse | Candidate contact steps | ENG-209 root impulse | ENG-209 local-collider impulse |
|---|---:|---:|---:|---:|---:|
| NativeVoxels | 0.07972926 m | 10.102019 N·s | 2 | 10.102019 N·s | 0.6742044 N·s |
| MergedCuboids | 0.07968524 m | 10.104371 N·s | 2 | 10.104371 N·s | 0.6742024 N·s |

This confirms useful prescribed surface velocity for this bounded path. It does
not establish a general impact threshold, animal recoil, or population target.

The original fast one-step trajectory is retained unchanged in
`world::tests::diagnostic_fast_one_tick_root_target_contacts_without_probe_response`.
The candidate repeats its 0.5 m / 16.67 ms movement without a sweep: NativeVoxels
records one pair and zero impulse; MergedCuboids records zero pairs and zero
impulse. The one-step high-speed interaction gate therefore remains failed.
Sweeping root translation prevents tunneling into fixed terrain; it does not
repair fast limb-to-dynamic-body contact response.

## Terrain and support scenarios

The original 2 m / 2 s fixed-wall case remains in
`world::tests::diagnostic_kinematic_root_crosses_fixed_terrain_but_character_sweep_blocks`:
the prescribed legacy root crosses the 0.25 m wall, while the same character
query sweep advances only 0.6799 m. It remains a failed/limited baseline.

The candidate's unchanged-orientation root translation uses a Rapier shape
cast of the full assembly's enclosing sphere. A new fixed-wall scenario
requests a 2 m translation toward a 0.25 m obstacle; the cast stops at first
conservative contact and publishes the new pose only after `PhysicsWorld::step`.
The test also verifies that direct unswept root movement/rotation and duplicate
or stale pending pose updates are rejected. The accepted move may be shorter
than exact voxel collision requires because it sweeps a sphere around the
assembly bounds.

For support removal, the same conservative sphere settles on an actual fixed
floor using caller-supplied gravity steps. The measured root Y at conservative
support was 0.43318623 m. Removing the live floor collider, refreshing queries,
and running 30 more gravity steps moved the root to -0.8339391 m (1.2671253 m
of measured fall). The source geometry remains kinematic and may sit farther
from the floor than exact source-cell contact because the enclosing sphere is
the movement bound.

## Contact ownership and damage attribution

`ContactImpulse` now reports `source_parts` for either side when the actual
participating collider is bound to an articulated source part. That attribution
comes from the live collider handle and assembly table. `source_cells` remains
`None` for both sides. Rapier's manifold witness/impulse aggregate does not name
an authoritative integer voxel; rounding the point to a cell would guess.
Damage rules may use the identified source part as context, but must leave
source-cell damage unattributed until a separately validated geometric
resolver proves a cell against the contact surface and handles ambiguity.

## Actual asset and clip workload

The bounded release probe exercises the real trout, crow, and deer SPVX assets,
including all clips, hidden parts, and nested deer antlers, with both exact
NativeVoxels and MergedCuboids representations. It compares every live query
against the independent transformed occupied-cell oracle. All six rows pass
38,520 total ray checks and 66 guaranteed misses with zero mismatches, zero
timed misses, and zero source-cell removal/restoration mismatches. The
per-scenario counts and full raw timings are in
[ENG-210-animal-workload.json](ENG-210-animal-workload.json).

The probe uses 10 warmups and 100 measured samples. `pose + rays + step`
includes a complete pose update, one ray per source part, query refresh and one
fixed step. Rebuild timing covers a source-part grid replacement. Times below
are measured microseconds on Windows x86_64, optimized release, AMD64 Family
26 Model 68, 16 reported logical processors.

| Animal | Shape | Cells / parts | Bodies / colliders / primitives | Attach p50 / p95 / p99 | Pose + rays + step p50 / p95 / p99 | Rebuild p50 / p95 / p99 |
|---|---|---:|---:|---:|---:|---:|
| Trout | NativeVoxels | 136 / 5 | 6 / 5 / 5 | 20.4 / 21.5 / 43.8 | 6.2 / 6.8 / 8.9 | 2.9 / 8.0 / 8.3 |
| Trout | MergedCuboids | 136 / 5 | 6 / 5 / 17 | 6.0 / 6.3 / 8.1 | 5.7 / 6.2 / 6.6 | 1.1 / 2.1 / 2.2 |
| Crow | NativeVoxels | 217 / 6 | 7 / 6 / 6 | 23.3 / 25.3 / 43.6 | 7.5 / 8.0 / 11.1 | 3.8 / 5.0 / 5.0 |
| Crow | MergedCuboids | 217 / 6 | 7 / 6 / 14 | 6.3 / 6.5 / 6.7 | 6.7 / 7.3 / 8.1 | 1.2 / 1.4 / 1.5 |
| Deer | NativeVoxels | 285 / 9 | 10 / 9 / 9 | 33.7 / 34.5 / 37.0 | 11.1 / 12.1 / 14.2 | 3.1 / 9.4 / 9.5 |
| Deer | MergedCuboids | 285 / 9 | 10 / 9 / 21 | 9.7 / 21.7 / 25.3 | 9.7 / 10.8 / 13.5 | 1.0 / 1.9 / 2.2 |

Body totals include the empty conceptual root bookkeeping body plus one
position-based kinematic body per source part. The extra empty root is not
connected to the parts and has no collider or solver contact. No population
acceptance target exists; these are single-animal CPU measurements only.

## Decision and remaining gates

The candidate is worth retaining for review: it restores the slow animated
part impulse to the root-motion baseline, keeps the exact source-query matrix
passing, and gives root translation a conservative fixed-terrain movement
contract. ENG-210 is not fully passed. High-speed one-step part contacts still
produce no useful impulse; the body is kinematic rather than impact-responsive;
the conservative sphere can overblock; root rotation and animated part motion
are not swept against terrain; and source-cell damage remains unattributed.
Dynamic multi-volume corpse ownership, live lifecycle/runtime integration,
replication, persistence, and population performance remain separate gates.

### Focused sweep correction (supersedes earlier wall distance)

Review found the earlier envelope radius used half of the squared AABB extent,
which overestimated radius by sqrt(2). The radius now uses the Euclidean length
of the three half-extents. Re-running the original 2m fixed-wall scenario with
the final zero-offset support handling clips to 0.15852165m (fraction
0.079260826). The earlier 0.0668138m radius-bug result and 0.15652314m
biased-query result are superseded and must not be used as the final
measurement. This remains a conservative enclosing-sphere contract, not exact
articulated clearance.

The root sweep synchronizes pending broad-phase edits before casting and
filters blockers to fixed, non-sensor colliders. Dynamic and kinematic actors
remain handled by the same Rapier solver after a move is scheduled. It casts
against fixed terrain colliders individually so exact zero-time support hits
can be classified by normal: tangent or separating motion is ignored, while
motion into the surface remains blocked. There is no query-space offset. New
cases cover grounded 0.25m lateral and 0.2m upward movement, downward motion
into the floor, a wall adjacent to the floor inside a single NativeVoxels
collider and a single MergedCuboids compound, initial-overlap rejection, a
0.1µm inward request, and departure from a one-cell ledge. For very short
requests, the cast extends along the same normalized direction to 10µm to avoid
Parry float-precision loss, discards any hit farther than the requested
distance, and converts the accepted TOI back to the request fraction. The
support-direction threshold is angular (`1e-6`) rather than proportional to
movement distance. The wall-removal case casts again immediately after
collider removal without manual query synchronization.

`update_articulated_pose_with_swept_root_translation` validates the complete
sample and requested root target before staging either side, then commits the
part poses and clipped root at the same physics boundary. Its combined sweep
uses a root-centered sphere enclosing old and proposed source corners through
interpolated part rotations/translations. This can overblock and is not exact
clearance for discontinuous authored motion. Rejected malformed and
near-world-extent combined updates leave root pose, source-cell query result,
and pose token unchanged through a following physics step.

## Checks

Exact checks run in the assigned worktree:

```powershell
cargo test -p spall_physics --lib --test articulated_review
cargo test -p spall_physics articulated_ -- --nocapture
cargo test -p spall_physics articulated_sweep_allows_grounded_horizontal_and_upward_recovery -- --nocapture
cargo test -p spall_physics articulated_sweep_rejects_initial_penetration_and_allows_small_ledge_departure -- --nocapture
cargo test -p spall_physics articulated_sweep_does_not_skip_later_wall_in_shared_l_terrain_collider -- --nocapture
cargo test -p spall_physics articulated_combined_range_rejection_does_not_leave_part_targets_pending -- --nocapture
cargo test -p spall_physics diagnostic_kinematic_root_crosses_fixed_terrain_but_character_sweep_blocks -- --nocapture
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo clippy -p sandbox --example animal-collision-probe -- -D warnings
cargo fmt --package spall_physics --check
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
git diff --check
cargo build -p sandbox --release --example animal-collision-probe
.\target\release\examples\animal-collision-probe.exe --bounded --kinematic-parts
.\target\release\examples\animal-collision-probe.exe --bounded
```

The physics suite passed 80 tests with 1 existing ignored diagnostic, plus all
8 independent `articulated_review` scenarios. The focused ENG-210 scenario run
passed all selected cases, including unbiased support tangency, terrain removal
freshness, and combined pose/root staging. Strict Clippy, formatting,
whitespace, and the bounded release asset probe passed. A near-extent combined
update was rejected before staging and preserved root pose and hit provenance
through a following step. GPU, network, storage, live actor, corpse transfer,
and population target gates did not run.

The final line reran the unchanged ENG-209 single-root mode against the new
shared code and passed the same 38,520 ray / 66 miss matrix with zero
mismatches or timed misses. Its raw output is saved in
[ENG-210-eng209-regression.json](ENG-210-eng209-regression.json).

### Follow-up boundary and terrain-query validation

After the focused fixes, the full suite passed 78 physics unit tests plus 8
review scenarios (1 existing ignored diagnostic). The updated candidate and
default actual-asset probes were rerun with both authored clips and all sampled
phases. Each mode reports 38,520 source-cell ray checks, 66 miss checks, zero
mismatches, and zero timed misses. Candidate counts remain 5/6/9 source
colliders and 6/7/10 rigid bodies for trout/crow/deer respectively; default
uses one rigid body per animal. Raw files:
[kinematic candidate](ENG-210-animal-workload-followup-kinematic.json) and
[default path](ENG-210-animal-workload-followup-default.json).

Candidate p50 attach / pose-query-step / source-rebuild times in microseconds:
trout native 22.3 / 6.2 / 3.0, trout merged 6.0 / 5.7 / 1.1; crow native
23.4 / 7.5 / 3.8, crow merged 6.4 / 6.8 / 1.3; deer native 34.3 / 11.6 / 3.1,
deer merged 9.3 / 10.2 / 1.0. These are diagnostics, with no agreed population
target. `cargo test -p spall_physics --lib --test articulated_review`, strict
all-target physics Clippy, formatting, and diff checks passed. Release probe
commands were `animal-collision-probe.exe --bounded --kinematic-parts` and
`animal-collision-probe.exe --bounded`.

### Nonlinear authored-pose and root-rotation transition gate

The pinned Parry 0.30.2 query API re-exported by Rapier 0.35.3 provides
`QueryPipeline::cast_shape_nonlinear`, backed by `NonlinearRigidMotion`. The
new `update_articulated_pose_with_swept_root_pose` validates the complete part
set and root transform before querying each exact source-part collider against
current fixed, non-sensor terrain. The declared one-tick path is linear motion
of each collider origin and shortest-arc rotation from its current world pose
to the requested flattened part pose composed with the requested root pose.
Any hit rejects the entire update without staging root/part bodies or tokens;
an accepted update stages all of them for one shared physics boundary. This is
a continuous cast for that explicitly defined independent-rigid-part path. It
does not claim a joint-constrained interpolation between authored samples or
solver-driven kinematic recoil. Dynamic and sensor colliders remain outside
the terrain-blocking cast; dynamic interactions stay in the same Rapier solver.

A new regression rotates an off-center source part 180 degrees through a fixed
wall. The sweep rejects at an interior TOI and leaves pose token/query
provenance at token 0. Removing the wall is visible without manual query sync;
the same update then publishes root and part rotation together at token 1.
Post-step broad-phase AABBs for transitioned articulated colliders are
refreshed before tokens publish, so immediate queries see large rotations in
the same boundary.

The release probe now tests every adjacent sampled phase within each clip for
trout, crow, and deer in both collider representations. Each independent case
places a floor 2 cm below the lowest endpoint-cell face and a wall 2 cm beyond
the highest endpoint-cell face, then combines the clip sample with a 10-degree
root yaw. It verifies one source ray per part against the independent cell
oracle after each accepted step. Across six animal/representation cases, 48
clip transitions were measured: 46 accepted and two blocked. Both blocks are
deer NativeVoxels walk transitions, 0→12 and 24→36 ticks, at part IDs 8 and 6,
with TOI 0.454593 and 0.454594. Deer MergedCuboids accepted those same
intervals, so representation-dependent nonlinear casting remains an explicit
unresolved gate; this is conservative rejection evidence, not a claim that
either path matches the other. All 38,520 independent source-cell rays and 66
miss checks still passed with zero mismatches/timed misses in the candidate
path. The default path rerun also passed the same ray/miss oracle.

With terrain and one post-step source ray per part included, complete
update/query/step p50/p95 costs were: trout native 133.5/234.9 µs, trout merged
103.2/162.7 µs; crow native 132.0/190.6 µs, crow merged 94.9/103.2 µs; deer
native 334.8/387.8 µs, deer merged 194.2/196.7 µs. Counts are 5/6/9 source
parts, 6/7/10 candidate bodies, and 5/6/9 source colliders for trout/crow/deer;
primitive totals remain native 5/6/9 and merged 17/14/21. These are bounded
single-animal measurements, with no agreed population target. Raw results are
in [kinematic transition workload](ENG-210-animal-transition-workload.json)
and [default workload](ENG-210-animal-transition-workload-default.json).

The original 0.5 m / one-step fast-impact test is unchanged. Adding CCD to the
dynamic probe body did not change its result: NativeVoxels had one broad pair
but zero positive-impulse pairs in both CCD modes; MergedCuboids had zero
pairs/impulse in both modes. Thus pinned CCD did not resolve the kinematic
target's zero-impulse gate. The prior 0.6 m / 60-step path response, fixed-wall
root crossing, source-cell damage attribution, population target, live actors,
and corpse transfer remain unaccepted. Manifold positions are still not used
to guess damaged source cells.

The final transition-gate check reran
`cargo test -p spall_physics --lib --test articulated_review`: 81 passed, 1
existing ignored diagnostic, and all 8 review scenarios passed. Strict
`cargo clippy -p spall_physics --all-targets -- -D warnings`, strict sandbox
probe Clippy, `cargo fmt --check`, and `git diff --check` passed. Focused runs
also included
`cargo test -p spall_physics nonlinear_pose_and_root_rotation_sweep_blocks_wall_then_accepts_after_removal`
and the unchanged
`cargo test -p spall_physics articulated_part_body_fast_single_step_contact_is_measured_without_sweep
-- --nocapture` (which prints CCD-off/on measurements for both shapes).

### Representation mismatch resolution: continuous source-cell sweep

The two NativeVoxels-only deer blocks above were caused by the pinned query
implementation, not by a difference in occupied animal cells. Rapier 0.35.3
re-exports Parry 0.30.2. Its `nonlinear_shape_cast_voxels_shape.rs` explicitly
states that rotating moving Voxels are not supported: the code assumes the
moving voxel shape is translational and approximates the other shape's AABB
from endpoint orientations. The original aggregate-collider mismatch remains
preserved, unchanged, in
[pre-cell-oracle raw workload](ENG-210-animal-transition-workload-aggregate-cast-before-cell-oracle.json).

The terrain transition implementation now casts every occupied source grid
cell as a temporary Cuboid query shape. Each cell follows its owning source
part body's linearly interpolated origin and the same shortest-arc angular
delta used by Rapier's `RigidBodyPosition::pose_errors` when deriving a
position-based kinematic target velocity. Its rotation center is the source
part body origin, including negative grid origins. The live assembly is still
one kinematic body and exact NativeVoxels/MergedCuboids collider per source
part; query Cuboids do not become bodies, alter source IDs, or replace live
colliders. The combined update remains all-or-nothing. Requests exceeding one
million occupied-cell × fixed-terrain-collider casts reject with
`WorkBudget` before any body/token is staged.

The actual-asset probe independently validates the complete 48-case matrix
against a continuous source-cell clearance oracle. It evaluates each cell's
oriented-box support extent at each interval midpoint and subtracts a motion
bound for every point in that cell:
`(|part-origin delta| + angular delta × (cell-center radius + cell radius)) ×
interval half-width`. A clear interval is certified only when both fixture
plane gaps exceed that bound plus 20 µm; otherwise it bisects, to depth 24,
and reports inconclusive as not clear. The oracle uses the authored start/end
cell transforms and is separate from Rapier's nonlinear cast path. It treats
the floor and wall planes as infinite, so the certificate is conservative for
the finite fixtures and can overblock; it is not a discrete-time sample
claim. Parry's cast remains an iterative numerical query (GJK distance
tolerance and TOI bisection), so the independent plane certificate is the
continuous no-contact check for these cases.

All 48 authored same-clip transitions, with 10-degree root yaw, were certified
clear by that oracle and accepted by the cell-union sweep. Both deer walk
0→12 (part 8 in the former Native aggregate cast) and 24→36 (part 6) now agree
as accepted in NativeVoxels and MergedCuboids. Cross-representation outcome
disagreements: 0. The candidate retained all 38,520 independent source-cell
ray checks and 66 miss checks, with zero mismatches and timed misses; the
default-root run retained the same matrix. Current raw files are
[cell-union candidate](ENG-210-animal-transition-workload-cell-union.json)
and [default path](ENG-210-animal-transition-workload-cell-union-default.json).

Single-animal full transition + source-ray + physics-step p50/p95 µs for the
current run: trout Native 548.6/622.1 and Merged 1460.1/1543.0; crow Native
1075.3/1111.4 and Merged 2611.1/2654.4; deer Native 2246.1/2421.1 and Merged
4948.2/5072.9. This is much more expensive than the former aggregate cast;
it is a bounded correctness candidate, not a population-scale performance
acceptance, and no population target has been agreed. Counts remain 5/6/9
source parts and colliders, 6/7/10 candidate bodies, and 136/217/285 occupied
cells for trout/crow/deer; the queried terrain has two colliders in each case.

The same-commit verification reran the full physics and articulated review
suites (81 passed, 1 existing ignored, plus all 8 review cases), strict
all-target physics Clippy, and strict probe Clippy. The release actual-asset
candidate and default probes both completed, including the new representation
parity and deer-pair assertions. Rapier's pose velocity source computes
`(next.rotation * current.rotation.inverse()).to_scaled_axis()`, matching the
implemented shortest-arc angular path, including its chosen axis at π. The
original fast-impact and source-cell damage-attribution gates remain failed or
unaccepted as described above; this sweep resolves only the representation
mismatch.
