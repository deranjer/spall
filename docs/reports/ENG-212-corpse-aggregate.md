# ENG-212 editable corpse aggregate feasibility

Status: **candidate demonstrated; feasibility gate remains open**. This report
does not accept live animal interfaces or a 1 km animal population.

## Candidate and mass rule

The adapter can attach every exact source-part collider to one dynamic Rapier
body while retaining one source grid, `VolumeId`, part ID, integer cell origin,
material occupancy, source revision, and per-part ray provenance. The authored
part transforms are frozen at the supplied death pose. No joints or cross-part
bonds are inferred. `NativeVoxels` installs one shape per part; `MergedCuboids`
installs the exact cuboid decomposition per part.

Mass is the volume sum of source cells in the flattened assembly-local frame.
Each cell contributes material density times cell volume, the solid-cube
self-inertia, and a parallel-axis term about the combined COM. Rotated parts
therefore contribute a full tensor, including off-diagonal terms. Overlapping
source cells count once **per source volume** for mass, while their physical
colliders geometrically overlap. This is an explicit additive ownership rule,
not a claim that overlapping material is physically unioned. A future lifecycle
must reject or resolve overlaps before admission if union-mass semantics are
required.

An edit rebuild accepts expected/new topology and source revisions. It replaces
only the named source collider, then installs recomputed mass properties while
keeping the rigid-body origin and collider-local source transforms fixed. This
keeps surviving cells in place when COM changes. The API caller must compute
the new aggregate mass from the post-edit source snapshot. Stale edits are
rejected before mutation. Geometry remains owned by the existing physics
adapter; there is no new actor, persistence, or protocol record.

## Authored-asset CPU measurements

Raw observations are in [ENG-212-corpse-candidate.json](ENG-212-corpse-candidate.json).
The release probe used three sampled poses for each authored asset and both
exact representations: 18 cases total. Every case used one dynamic body, one
collider per source part, and 240 fixed 1/60 s steps against a finite floor.
Each case removed one exposed occupied source cell and rebuilt that collider
and aggregate mass. Before the step, an independent cell-face ray checked the
source part and cell identity.

| Animal | Cells / parts | Representation | Colliders / shape primitives | Attach μs | Edit + mass rebuild μs | Mean step μs |
|---|---:|---|---:|---:|---:|---:|
| Trout | 136 / 5 | Native Voxels | 5 / 5 | 27.3–71.6 | 17.6–24.1 | 3.37–4.45 |
| Trout | 136 / 5 | Merged Cuboids | 5 / 17 | 12.5–20.2 | 13.6–17.7 | 1.82–2.09 |
| Crow | 217 / 6 | Native Voxels | 6 / 6 | 35.4–37.1 | 10.5–13.4 | 4.39–4.43 |
| Crow | 217 / 6 | Merged Cuboids | 6 / 14 | 14.5–17.0 | 10.0–15.1 | 2.17–2.20 |
| Deer | 285 / 9 | Native Voxels | 9 / 9 | 50.2–51.5 | 26.0–35.2 | 13.54–25.93 |
| Deer | 285 / 9 | Merged Cuboids | 9 / 21 | 24.4–32.4 | 19.6–21.1 | 4.81–4.91 |

All 18 pre-step identity checks passed, all 18 edits removed exactly one source
cell, and none of the post-edit rays hit the removed cell. Contact impulses
were present in 172–188 of the 240 steps per case. The COM-edit body-origin
shift measured zero in every case. Mass was 33.203125 kg (trout), 53.22265625 kg
(crow), and 69.580078125 kg (deer), using the probe's uniform 1,000 kg/m³
material assumption; these are fixture values, not biological mass estimates.
The probe reports exact per-case tensors and timings in the raw JSON. These are
single local optimized-build observations, not percentiles or performance
targets. No population target is defined.

The focused integration test also exercises a rotated and off-center two-part
source with negative integer origin under both representations. It verifies
nonzero product of inertia, source identity after falling, one physical body,
source-cell removal and revision update, unchanged surviving world geometry,
and stale-topology rejection leaving the prior query result intact.

## Gate blockers and limits

1. **Atomic transfer is not demonstrated.** The candidate currently admits an
   aggregate from source snapshots. It does not convert the ENG-210 live
   kinematic part rig in place or prove an all-volume ownership transaction
   whose failure leaves that rig intact. A caller could otherwise expose both
   owners or lose the old owner between calls.
2. **Death momentum is not derived.** The experimental constructor starts the
   body with zero linear/angular velocity. Kinematic clip motion does not
   provide a physically unique articulated momentum, and no per-part source
   velocities are passed. The edit path asks Rapier to adjust angular velocity
   when the mass tensor changes; it does not account for the removed cell's
   material as a separate body or conserve total system momentum. Neither rule
   is approved for lifecycle transfer.
3. **Empty-part retirement is absent.** The existing per-part rebuild rejects
   an empty grid. The candidate has whole-aggregate retirement but no atomic
   removal of one empty source part/volume while other parts survive.
4. **Cross-part fracture is unresolved.** PART hierarchy and colliding source
   volumes do not establish cell bonds. This body remains one rigid compound
   until the whole owner is retired; splitting it requires a separate declared
   connectivity/bond policy and stable volume ownership transfer.
5. Stale topology was tested in a small adapter scenario. Stale actor generation,
   death-pose epoch and solver work-result tokens do not exist here because no
   lifecycle interface was introduced. `MergedCuboids` primitive cost is
   additive across parts and has no aggregate-wide fixture cap beyond current
   admission guards.

These are material prerequisites, so ENG-212 remains **in progress**. The
candidate supports the one-dynamic-owner representation at small CPU scale;
it does not yet prove safe conversion from a living articulated owner or
physically justified momentum transfer. No host/runtime, network, storage, UI,
full-world animal, weather-combined, GPU, or population acceptance was run.

## Exact checks

- `cargo check -p spall_physics` — passed.
- `cargo test -p spall_physics --test articulated_review frozen_pose_corpse_aggregate_is_one_editable_dynamic_owner_with_source_hits -- --nocapture` — passed for both representations.
- `cargo check -p sandbox --example animal-collision-probe` — passed.
- `cargo run -p sandbox --release --example animal-collision-probe -- --corpse-bounded` — 18 authored-asset cases completed; all reported source-hit/edit checks passed.
- `cargo fmt --package spall_physics --package sandbox` — applied; final `--check` is still to be run.

Next work should close the atomic ENG-210-rig-to-corpse transfer and define a
reviewable momentum rule before any runtime lifecycle interface is started.
