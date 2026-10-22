# ENG-209 articulated collision feasibility

## Finding

The bounded exact-geometry gate passes for the checked-in trout, crow, and deer assets with both `NativeVoxels` and `MergedCuboids`. A single Rapier position-based kinematic root can carry one exact collider for each source part while preserving source `VolumeId`, revision, source grid, and current part pose. The adapter stays inside the existing `PhysicsWorld`; no Rapier handles escape the physics crate and no second solver or per-voxel bodies are added.

The gate establishes exact source-cell provenance for ray hits against a selected assembly and ordinary dynamic-body contact with the assembly. It does not establish articulated dynamic/corpse ownership, contact-impulse ownership by source part/cell, world-occlusion/game-hit behavior, locomotion response, or a throughput target.

## Adapter surface and invariants

`spall_physics::articulated` exports the opaque `ArticulatedBodyId`, `SourcePart`, `PartTransform`, `ArticulatedPose`, `SourceCellHit`, and `ArticulatedError` types. `PhysicsWorld` adds APIs to attach an assembly, install a complete tokened pose, schedule a tokened root movement for the next solver step, place the root diagnostically, rebuild one part grid, retire the assembly, count its colliders, and raycast its live Rapier shapes.

Each part retains its own `OccupancyGrid`, `VolumeId`, source revision, source cell size, hierarchy parent, and integer grid coordinates. Input and flattened transforms are finite normalized quaternion plus translation only; scale is not accepted. The adapter rejects smooth convex geometry, duplicate part/volume ownership, invalid hierarchy, empty parts, stale or repeated topology/revision/pose tokens, invalid poses, unrepresentable source cells, and work above the documented bounds before mutation. Grid rebuild prepares its exact new shape and checks aggregate dense cells, occupied cells, and merged primitive ceilings before removing the old collider. Failed rebuilds retain current geometry and tokens.

The current admission envelope limits source grid cell coordinates to i32 provenance, each axis's grid origin/extent and each root/part translation to 1,000 m, the assembly to 4,000,000 dense cells and 1,000,000 occupied cells, and attached shapes to 65,536 merged primitives (4,096 per merged part). The 1,000 m bound prevents non-finite collider inputs; it is not a precision guarantee. Accuracy was measured on near-origin/local animal fixtures only. Large-world callers must use their rebasing path, and precision at the admission limit remains unverified.

Ray queries use the shared Rapier query pipeline and the live collider handles, then map the actual collider TOI back through the validated root and part transforms to occupied source cells. A source hit includes part ID, source volume and revision, i32 source cell, TOI in metres, topology token, and current pose token. At exact internal faces, the deterministic rule chooses the lexicographically smallest touching occupied local cell; when part TOIs differ by at most 10 μm, stable part ID breaks the tie. No floating-point bit-exactness is claimed. `raycast_articulated` filters to the requested assembly, so game code still owns world occlusion and cross-object target selection.

After a completed solver step, installing a changed child-local pose explicitly updates both its parent-relative and current world pose before query refit. This fixes stale query hits when the parent no longer has a modified-body flag. Kinematic root targets publish their pose token only after `PhysicsWorld::step`; stale or duplicate targets and child pose changes while a root target is pending leave the installed pose/token unchanged. The fast diagnostic root-placement API is a teleport and is not evidence of moving-surface response.

## Verification

`cargo test -p spall_physics --test articulated_review` passes 8/8 independent review scenarios for both exact representations where applicable: negative coordinates and thin gaps, nested rotated hierarchy, deterministic overlapping-part/cell ties, TOI-zero cell-face boundaries, stale/invalid atomicity, scheduled-root pose epochs, post-step pose refresh and rotated rebuild/restoration. The contact scenario reports positive Rapier impulse while a unit dynamic probe rests on the kinematic assembly, then verifies that the probe falls after assembly retirement.

Physics unit tests also cover tokened source-grid replacement, stale rebuild preservation, aggregate dense-budget overflow rejection, i32 source-coordinate and f32::MAX cell-size rejection without mutation, unique live assembly ownership for each source volume, and root-versus-part motion response. The bounded motion diagnostic drives the exact same single-cell path by 0.01 m for 60 fixed steps (0.6 m total at 60 Hz) and measures dynamic-probe response:

| Representation | Root-driven probe x | Root impulse sum | Root contact steps | Part-local probe x | Part-local impulse sum | Part-local contact steps |
|---|---:|---:|---:|---:|---:|---:|
| NativeVoxels | 0.07972926 m | 10.102019 N·s | 2 | 0.05839338 m | 0.6742044 N·s | 10 |
| MergedCuboids | 0.07968524 m | 10.104371 N·s | 2 | 0.05839338 m | 0.6742024 N·s | 10 |

The source-local path produces much smaller impulse because the kinematic root velocity does not represent the authored child collider's local motion. This confirms that geometry/query refresh is feasible while physical response to animated limbs remains inadequate for some interactions. A separate fast case targets a root 0.5 m in one 16.67 ms step: Rapier tracks one contact pair, but the dynamic probe stays at x=0 with vx=0 and receives zero impulse. Teleport-style placement and large one-tick targets cannot be treated as gameplay collision response.

A separate fixed-terrain case schedules a kinematic root along a 2 m path through a 0.25 m fixed obstacle over 2 s. The root reaches x=1.0 m, crossing the wall, while Rapier records zero contact pairs and zero manifold penetration because the pinned defaults omit kinematic-fixed narrow-phase pairs. The zero manifold reading does not imply that the shapes did not geometrically overlap. After retiring the assembly, the same character query sweep sees that wall and advances only 0.6799 m of the requested 2 m. This confirms that the existing query-based character controller can block against terrain, while a prescribed articulated root does not receive fixed-terrain solver blocking. Navigation/locomotion must use an explicit sweep or another bounded movement policy; this experiment does not supply one in the animal lifecycle.

Contact impulse output intentionally names participating rigid bodies, not source parts or cells. The dynamic probe proves a normal Rapier dynamic/kinematic solver path is enabled. It does not establish reliable contact-cell attribution. Kinematic root motion can carry useful surface velocity, while local animated part motion can lag substantially; fixed-terrain solver blocking is also absent with current active-collision defaults. There is no source-cell provenance inferred from a manifold point.

## Actual-asset workload

The separate [animal workload report](ENG-209-animal-workload.md) and its JSON companion contain the exact release command, machine/profile, all sampled clip ticks, full source cell/part/collider counts, percentile timings, and oracle details. It decodes real trout, crow, and deer SPVX files, including hidden parts and nested deer antlers, and compares each exact adapter hit against an independent transformed source-cell union. The final matrix passes all six animal/representation combinations: 38,520 ray checks and 66 guaranteed misses, with zero oracle mismatches or adapter misses. The report also records the earlier post-step cache miss and its resolution so that the failed run remains auditable.

No population throughput target was accepted. The workload timings describe one CPU, one assembly, bounded warmups/repeats, and include pose installation, query refresh, ray work, solver step, and topology replacement according to the fixture report. They are diagnostic measurements, not release acceptance limits.

## Remaining limits

- No stable ownership/API for a dynamic corpse assembled from multiple volumes; that needs a separate feasibility gate.
- Animated child motion yields much weaker probe response than root movement in the bounded contact comparison.
- No fixed-terrain solver blocking for the position-prescribed kinematic root, and no contact-manifold source part/cell attribution; locomotion and damage must not infer a response or owning cell from ray hits.
- The query API tests one assembly only and does not replace world-level ray ordering or occlusion.
- Precision at the 1,000 m admission envelope and whole-world coordinate rebasing are untested.
- No many-animal/population performance target was run or accepted.

## Coordinator integration and acceptance (2026-10-15)

Integrated prototype commit `c46b7c6` follows the reviewed ENG-205 lifecycle
audit. Accepted scope is the bounded experimental geometry/query adapter and
measured contact diagnostics, not a shipping creature rig. ENG-210 is the next
unblocked movement/response feasibility ticket; the editable multi-volume
corpse representation remains a separate prerequisite. Live actors, durable
recovery, multiplayer, far-origin precision and population acceptance remain
unproven.

Changed implementation files: `crates/spall_physics/src/articulated.rs`,
`src/world.rs`, `src/lib.rs`, `tests/articulated_review.rs`,
`examples/sandbox/examples/animal_collision_probe.rs` and its Cargo example
registration. Three report/evidence files record the adapter and workload.
Shared `docs/animals.md`, `docs/animal-lifecycle.md` and `docs/tasks.md` now
reflect the measured limits and ENG-210 dependency.

Exact checks on the integrated checkout:

```powershell
cargo test -p spall_physics -p spall_asset -p sandbox --lib --test articulated_review --test animal_assets --test animal_habitat_world
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo clippy -p sandbox --example animal-collision-probe -- -D warnings
cargo fmt --package spall_physics --check
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
cargo build -p sandbox --release --example animal-collision-probe
.\target\release\examples\animal-collision-probe.exe --bounded
git diff --check
```

The combined suite passed 118 tests: 71 physics unit, 8 independent collision
review, 13 asset, 23 sandbox library, and 3 authored/live-habitat integration
tests. One existing manual physics timing diagnostic was ignored. The first
strict probe lint found a manual range check and an eight-argument diagnostic
helper; the coordinator used an inclusive range and bundled topology/pose
tokens, then reran lint successfully and rebuilt/reran the complete release
matrix. Its final committed JSON/table contain that integrated-checkout run:
six cases, 38,520 oracle checks, 66 guaranteed misses, zero mismatches/timed
misses. Formatting and whitespace checks passed. Compiler cache hard-link
warnings used file-copy fallback and did not fail checks.

No full-workspace `xtask check`, GPU, server/client actor, recovery, or
population gate was run for ENG-209. Earlier preview-wide validation remains
separately recorded in ENG-204 and is not acceptance of these later gates.
