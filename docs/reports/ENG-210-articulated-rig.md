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

## Checks

Exact checks run in the assigned worktree:

```powershell
cargo test -p spall_physics --lib --test articulated_review
cargo test -p spall_physics articulated_ -- --nocapture
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

The physics suite passed 75 tests with 1 existing ignored diagnostic, plus all
8 independent `articulated_review` scenarios. The focused ENG-210 scenario run
passed all 9 selected cases. Strict Clippy, formatting, whitespace, and the
bounded release asset probe passed. GPU, network, storage, live actor, corpse
transfer, and population target gates did not run.

The final line reran the unchanged ENG-209 single-root mode against the new
shared code and passed the same 38,520 ray / 66 miss matrix with zero
mismatches or timed misses. Its raw output is saved in
[ENG-210-eng209-regression.json](ENG-210-eng209-regression.json).
