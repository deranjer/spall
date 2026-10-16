# ENG-213: full-world animal transition locality

**Scope:** CPU feasibility on the complete authoritative generated terrain. This is not a live-animal population or weather acceptance run.

## Implementation

Articulated transitions now synchronize the physics query view, build one conservative AABB per source part, and ask Rapier's current broadphase BVH for overlapping fixed, nonsensor colliders. Each AABB encloses the segment between the part body's current and requested world origins, expanded by the greatest occupied source-cell corner distance from that part origin plus 0.1 mm. Rotation preserves that distance, including the shortest-arc mid-path; the margin only retains extra candidates. Candidate handles are deduplicated before the existing exact nonlinear casts. The one-million source-cell × candidate-collider cap and exact source-cell geometry/provenance are unchanged. Budget rejection reports the source-cell and candidate counts.

The small-world regressions check an off-center 180-degree mid-arc wall, query freshness after removal, a newly replaced wall, an occupancy edit that moves that wall away, and 3,509 distant fixed colliders with a 285-cell animal at world x=250 m. That last transition retains zero candidates and issues zero casts; it would exceed the former Cartesian cast cap if all fixed colliders were multiplied by all source cells.

The established authored-animal source-cell oracle also passed unchanged: `--bounded --kinematic-parts` sampled all clips, checked 38,520 exact source-cell rays plus 66 intentional misses, reported zero mismatches and zero representation disagreements, and passed its NativeVoxels/MergedCuboids transition cases. The structured output is [ENG-213-small-oracle.json](ENG-213-small-oracle.json).

## Full 1 km measurement

Run in release mode on Windows x86_64, AMD64 Family 26 Model 68, 16 available hardware threads:

```powershell
cargo build --release -p sandbox --example animal-collision-probe
target\release\examples\animal-collision-probe.exe --generated-1km
```

The probe generated the complete natural seed-1 world at 4096 cells per edge (1024 m) in 9.09 s and built the authoritative `SimWorld`'s per-brick physics in 3.99 s. The server physics contained **123,892 fixed terrain brick colliders**. It checked 10 authored clip phases each for trout, crow and deer, using both `NativeVoxels` and `MergedCuboids`, against the full terrain at habitat points validated from the tick-zero generated terrain and water plan. All 60 transitions were accepted with zero terrain blocks and representation parity.

| Animal | Cells | Max terrain candidates | Max exact cell casts | Transition p50 / p95 |
| --- | ---: | ---: | ---: | ---: |
| Trout | 136 | 0 | 0 | 7–8 µs / 14–37,717 µs |
| Crow | 217 | 1 | 217 | 499 ms / 527–533 ms |
| Deer | 285 | 2 | 570 | 1.308–1.315 s / 1.328–1.377 s |

The six transition samples are in [ENG-213-full-world.json](ENG-213-full-world.json); initialization and per-species progress are in [ENG-213-full-world.stderr.txt](ENG-213-full-world.stderr.txt). Timings are the observed single bounded run, not a sustained throughput result. Trout had no terrain AABB candidates at its validated water position. Crow and deer show that exact per-cell casts remain expensive even after reducing the candidate set to one or two colliders.

The separate global step measurement used all three articulated rigs together: 23 articulated bodies (three roots plus 20 part bodies), 20 part colliders, 123,892 fixed terrain colliders, and zero dynamic bodies. One measured outer step was 485.1 ms at one solver substep and 485.0 ms at 32 substeps. These are single samples; the near-equal result indicates the full-world pipeline's fixed-collider overhead dominated this fixture. Weather and water stepping were excluded. The water plan was used only to choose a valid initial trout habitat.

## Limits and next work

The former global multiplication would request 35,309,220 exact casts for the 285-cell deer against 123,892 terrain colliders and reject on its one-million cap. The localized run needs at most 570 casts for that same pose sample while preserving the cap and the full terrain. However, the measured 0.5–1.4 second crow/deer transition costs and roughly 0.485 second global step mean this run does **not** establish a responsive 1 km animal population or accept a performance target. It used the reviewed kinematic movement candidate, had no dynamic bodies, did not exercise weather, and did not include the separate ENG-212 corpse aggregate gate.

Next unblocked prerequisite: ENG-212 corpse aggregate feasibility. A later integrated run needs an explicit population/per-tick target and a measured broadphase/query plus solver budget before 1 km fully simulated animals can be accepted.
