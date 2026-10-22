# ENG-210 source-cell contact witness feasibility

This scoped candidate reports a geometric source-cell witness and the solver's
matching point-level normal impulse for articulated part contacts. It does not
implement gameplay damage, edits, networking, lifecycle, or corpse behavior.
The existing pair-level `ContactImpulse::source_cells` stays `[None, None]`;
pair totals are never divided among voxels.

## Candidate contract

Pinned Rapier 0.35.3 retains solver manifolds and a per-tracked-contact impulse.
For each solver point, `SolverContact::contact_indices()` maps the solver anchor
back to that manifold's tracked point. `ContactManifoldData::
solver_contact_world_points` resolves the actual solver anchors through the
participating rigid bodies at the just-completed solver substep. The candidate
transforms the source-side anchor through that part collider's actual pose at
that substep, then snapshots only stable engine IDs, volume/revision/topology,
target pose token, substep index/fraction, world witnesses, normal, separation,
optional point impulse, and source-cell result. It does not retain Rapier
handles.

Records accumulate privately through the same world's solver substeps and
become readable after `step()` returns. They are capped at
`MAX_CONTACT_WITNESSES_PER_STEP = 4096`; `contact_witness_overflow()` counts
omitted source-bearing points. A transient hit can remain in the returned
outer-step snapshot after the final narrow-phase state has no active pair. A
pose token names the authored target for the tick, while `substep_fraction`
and the captured world witness describe the intermediate physical pose. The
public ray-query epoch is not advanced by this snapshot path.

Cell attribution is deliberately narrow. The source-local normal must be
axis-aligned within `0.9999`; the source witness must lie on the corresponding
voxel face within `max(10 µm, 1e-4 cell widths)`; both tangential coordinates
must lie outside that tolerance from edges; the candidate cell must be
occupied; and the outward neighboring cell must be empty. The returned cell
uses the grid's global integer origin and includes its material. Penetration,
shared/internal faces, empty cells, edges/corners, uncertain face positions,
and non-axis-aligned witnesses are explicitly unattributed. There is no
nearest-cell fallback or manifold rounding.

The optional `normal_impulse_n_s` is Rapier's impulse magnitude on that mapped
solver contact point in that substep. Its direction is defined by the contact
normal. This establishes the physical contact point and impulse value; an
application still needs a separate damage threshold, accumulation/cooldown,
cell-selection, and edit policy. A resolved cell is not by itself permission to
damage it.

## Evidence

Validation on the scoped worktree:

- `cargo test -p spall_physics --all-targets`: 89 unit tests passed, 1 manual
  occupancy timing diagnostic ignored, and all 8 articulated-review tests
  passed.
- `cargo clippy -p spall_physics --all-targets -- -D warnings`: passed.
- `cargo clippy -p sandbox --example animal-collision-probe -- -D warnings`:
  passed.
- `cargo fmt --all -- --check`: passed.
- `cargo run -p sandbox --release --example animal-collision-probe -- --bounded --kinematic-parts --solver-substeps=64`:
  completed and wrote the linked JSON below.

`cargo test -p spall_physics articulated_contact_witness_resolves_only_unique_source_face_contacts -- --nocapture`
passed for NativeVoxels and MergedCuboids. Each representation retained 16
substep contact points, resolved 16/16 to the nested child part's global cell
`[-2, 0, 0]`, recorded five positive solver-point impulses, and overflowed
zero. The last-state pair API had zero records: these points were transient and
would have been lost by a final-state-only query. The scenario also verifies
source volume/revision/topology/pose tokens, negative source origin, nested and
off-center part transforms, stale-pose rejection, and that a source-grid
replacement leaves the old completed-step witness unchanged while new ray
provenance returns the replacement revision/topology. Pair-level source cells
remain unattributed.

The independent resolver unit cases resolve both sides of a single cell and a
face beside a hole. It rejects an occupied internal shared face, exact and
near-edge witnesses, diagonal normals, and initial penetration. Thus the
resolver retains only face-interior evidence that identifies exactly one
occupied exterior source cell.

The real-asset run used the authored idle pose at tick 0 and one exposed +X
source face chosen from the independent occupied-cell set for each animal. A
quarter-cell dynamic voxel approached the face in the same Rapier world for
eight solver substeps. Both representations resolved 32/32 records for each
animal; nine point impulses were positive per run, and no overflow or
unattributed point occurred. Rig + probe totals were trout 7 bodies/6
colliders, crow 8/7, and deer 11/10. The raw JSON also preserves the existing
all-clips/all-phases transition and ray-oracle workload:

| Animal | Representation | Body/collider count | Solver points | Resolved | Positive impulses | Pipeline µs | Witness capture µs |
|---|---|---:|---:|---:|---:|---:|---:|
| Trout | Native Voxels | 7 / 6 | 32 | 32 | 9 | 225.2 | 14.9 |
| Trout | Merged Cuboids | 7 / 6 | 32 | 32 | 9 | 57.7 | 4.5 |
| Crow | Native Voxels | 8 / 7 | 32 | 32 | 9 | 64.9 | 5.9 |
| Crow | Merged Cuboids | 8 / 7 | 32 | 32 | 9 | 45.0 | 3.7 |
| Deer | Native Voxels | 11 / 10 | 32 | 32 | 9 | 71.4 | 4.6 |
| Deer | Merged Cuboids | 11 / 10 | 32 | 32 | 9 | 53.3 | 10.9 |

These are single optimized-build observations for this isolated one-step
contact fixture, not percentiles or performance targets. The existing complete
update/query/step benchmark in the same JSON ran all 48 authored transitions,
accepted 48, blocked 0, and reported 0 representation disagreements. Its
independent oracle performed 38,520 source-cell rays plus 66 miss checks with 0
mismatches. It measured the full transition/query/step operation separately;
the standalone contact-case timings above exclude probe setup and report
Rapier pipeline and witness capture separately.

- [ENG-210 authored-asset witness and transition measurements](ENG-210-contact-witness-assets.json)
- The previous unchanged 32/64-substep reports and all-clip transition costs
  remain in [ENG-210 fast-contact response report](ENG-210-contact-substeps.md).

Capture time is measured separately from Rapier pipeline time in
`StepTiming::contact_witness_capture`; the raw JSON reports it alongside the
single contact step time. These are CPU observations, not population targets.

## Remaining gates

This proves a bounded witness candidate only for finite, solver-active point
contacts whose exposed source face is uniquely established. The source-cell
oracle has no fallback for edge/corner contacts, diagonal contact normals,
penetration, contacts against cell interiors, or ambiguous/clustered geometry;
those remain unattributed by design. The current per-point mapping depends on
the pinned Rapier contact-index contract and should be rechecked on any Rapier
upgrade. The prototype does not define how multiple substep impulses should be
combined into a gameplay damage event, how two articulated bodies share one
contact event, or how damage regions should spread across cells. It is not
production approval for source-cell damage, all actual animal clip contacts,
population performance, runtime actors, corpses, or full-world lifecycle.
