# Collision representation decision (T06)

Status: feasibility outcome for the G1 gate. Frozen numbers are provisional
until the G1 integration (T11). This document records what `spall_physics`
measured comparing the two candidate voxel collider representations on pinned
`rapier3d 0.35.3` / `parry3d 0.30.2`, and the representation the engine adopts.

## Candidates

Both are built from one [`OccupancyGrid`](../crates/spall_physics/src/occupancy.rs)
— a dense solid mask over the tight axis-aligned cell box of a body — so they
describe exactly the same set of cells.

- **Native voxels** (`Representation::NativeVoxels`): one `parry` `Voxels` shape
  over the grid's solid cells (`ColliderBuilder::voxels`).
- **Merged cuboids** (`Representation::MergedCuboids`): a compound of the
  axis-aligned boxes produced by a deterministic greedy decomposition
  ([`merge::greedy_boxes`](../crates/spall_physics/src/merge.rs)) — the
  exact-occupied-space baseline the architecture requires. A single convex hull
  is never used: it would seal a hollow building's rooms.

## Measurements

Reference host: AMD Ryzen 7 9700X (8C/16T), 63.6 GiB RAM, Windows 11,
`rustc 1.96.1` `x86_64-pc-windows-msvc`. Release build. Reproduce with:

```
cargo run --release -p spall_physics --bin collision-bench -- --out .local/runs/t06-collision
```

Raw JSON: `.local/runs/t06-collision/collision-feasibility.json` (ignored path).
CI runs the same scenarios at `--small` size as `spall_physics` tests, asserting
behaviour only (settles, finite, interior preserved, mass/COM/inertia match,
compound stops a 60 m/s CCD projectile).

| Metric | Native voxels | Merged cuboids |
| --- | ---: | ---: |
| 64-brick connected body (128³-cell region, hollow) — primitives | 1 | 6 |
| … one-shot collider build | 15.1 ms | **7.8 µs** |
| … estimated collider memory | ~8 MiB (dense upper bound) | ~0.6 KiB |
| Collider rebuild after an edit (hollow tower), p50 / p99 | 224 µs / 229 µs | **1.5 µs / 2.5 µs** |
| Debris settle (256 pieces on a floor), step time p95 / p99 | 3.70 ms / 6.41 ms | **0.13 ms / 0.18 ms** |
| Debris settle — all pieces finite, at rest, asleep | yes | yes |
| Editable-collider sleep/wake — settled asleep, woke on an in-place collider rebuild, re-slept, woke on a blast impulse, travelled ~1.7 m, left the floor and re-collided, re-slept, stable throughout | yes | yes |
| Hollow building drop — settles, interior clearance kept | yes, 1.0 m | yes, 1.0 m |
| Mass / centre-of-mass error vs analytic reference | 0 / 0 | 0 / 0 |
| Sorted principal-inertia error vs analytic reference | 6e-6 | 0 |
| Fast CCD projectile vs 0.5 m wall — max speed still stopped | **20 m/s** | **220 m/s** |
| Worst-case fragmentation (16³ checkerboard, 2048 isolated cells) — primitives | 1 | 2048 |

### Sleep / wake verification

T06 requires that an editable collider goes to sleep when it settles *and* wakes
and responds when something acts on it. `report::sleep_wake_cycle` measures the
wake path directly for both representations rather than inferring it from a
count of bodies that slept:

1. a dynamic voxel cube is settled to sleep on the fixed floor;
2. its collider is **rebuilt in place** — the exact operation the T08
   authoritative edit path performs after an accepted topology transaction — and
   the body is observed to leave the sleeping state, then settle back to sleep;
3. a **blast impulse** sized from the body's own mass (~4.5 m/s) is applied; the
   body is observed to leave the sleeping state, rise clear of the floor, fall
   back and re-establish a floor contact near its rest height, then settle to
   sleep a second time.

Every state stays finite and contact penetration stays below half a cell across
the whole cycle. Both representations pass. CI asserts this at `--small` size in
`report::tests::small_feasibility_run_behaves`.

## Decision: merged cuboids

`spall_physics` builds **merged-cuboid compounds** for both static terrain and
dynamic detached bodies. Rationale, in order of weight:

1. **Editability.** A cut rebuilds the collider in ~1.5 µs versus ~224 µs, and a
   fresh large body in ~8 µs versus ~15 ms. The authoritative edit path (T08)
   rebuilds a body's collider on every accepted topology transaction; the
   compound cost disappears into the tick, the voxel-shape cost does not.
2. **Continuous collision detection.** `parry`'s `Voxels` shape gained no CCD
   benefit in testing — a fast body tunnels a 0.5 m wall above ~20 m/s — whereas
   the cuboid compound stops a 220 m/s projectile. Fast damaging bodies (T19,
   impact-triggered destruction) need working CCD.
3. **Exactness.** The compound covers exactly the occupied cells with no overlap
   (property-tested against the occupancy grid), so a hollow building keeps its
   rooms and mass / COM / inertia match the analytic reference exactly.
4. **Per-step cost and memory** are both lower.

Native concave triangle meshes for dynamic solids remain prohibited, per the
architecture.

## Fragmented bodies: the exact active-collider policy (ENG-42)

The greedy decomposition degenerates to **one box per cell** on highly
fragmented occupancy: a 3-D checkerboard of 2048 isolated cells produces 2048
compound parts; thin diagonal filaments, single-voxel speckle, and fine
lattices do the same. Unbounded, this defeats the performance argument for a
badly-shaped body.

The **superseded** T08 mitigation was to OR-downsample the body's grid by an
integer factor `k` (2, then 4) and re-inflate it — a coarse cell solid iff any
covered fine cell is solid — and, if `k = 4` still overflowed, to "build it
anyway". For an **active** body (terrain, dynamic solids: openings must block,
deleted cells must stop colliding) this is inexact collision: it seals fine-grid
air passages, keeps removed cells collidable, and — via "build it anyway" —
never actually enforces the cap. That bypasses a failed feasibility gate with
approximate topology, which the architecture forbids. **It has been removed.**
There is no coarsen / OR-downsample / "build it anyway" path in
`spall_sim::collider::plan_collider`.

The replacement is a **total deterministic exact policy** (a *representation*
fallback — geometry is never approximated):

- **Primitive budget per body:** `B = 4096` merged boxes (provisional). This now
  selects the *representation*, never the geometry.
- **`greedy_boxes(fine).len() <= B`** — build the merged-cuboid compound from the
  exact fine grid (the common case; cheap rebuild, working CCD).
- **`greedy_boxes(fine).len() > B`** — build the exact
  `Representation::NativeVoxels` shape (`parry` `Voxels`) over the **identical
  fine solid set**: one primitive, every air passage and every solid cell
  preserved bit-for-bit. No coarsening, no inflation, no dropped mass. The cost
  is a heavier collider rebuild and the weaker CCD of the voxel shape for that
  one body.
- **Feasibility ceiling for the native fallback:**
  `MAX_ACTIVE_COLLIDER_CELLS = 1 << 17 = 131 072` fine cells (~50³). A full
  native voxel collider rebuild scales ~linearly with total grid cells —
  measured in release on a maximally fragmented connected lattice
  (`spall_sim::collider::tests::sweep_native_rebuild_cost`):

  | fine cells | greedy boxes | native full rebuild (release) |
  | ---: | ---: | ---: |
  | 13 824 (24³) | 3 457 | 0.55 ms |
  | 32 768 (32³) | 8 193 | 1.3 ms |
  | 110 592 (48³) | 27 649 | **4.5 ms** |
  | 262 144 (64³) | 65 537 | 10.4 ms |
  | 438 976 (76³) | 109 745 | 18.0 ms |
  | 884 736 (96³) | 221 185 | 36.3 ms |

  An active body rebuilds its whole collider on every accepted edit, so at the
  ceiling a worst-case rebuild is ~5 ms — about a third of the 16.7 ms tick,
  leaving room for structure analysis, the commit, and other bodies. A body that
  is **both** over the primitive budget **and** larger than this ceiling has no
  exact per-tick-editable representation; `plan_collider` returns
  `ColliderInfeasible::TooLarge` and the spawn / commit / restore **fails**. The
  feasibility gate stays *failed* rather than serving inexact collision.
  Within-budget simple bodies (a solid slab is one greedy box) are unaffected by
  the cell ceiling — the compound rebuild cost is bounded by the box count, not
  the cell count.
- **Never** silently drop parts, coarsen, inflate, or fall back to a convex hull.

`coarsen_k` in the collider plan / persisted body is now always `1`; it is
retained only so the save format is unchanged and can be dropped by a future
revision.

If a future workload shows fragmentation is common rather than pathological, the
next thing to evaluate is `parry`'s **incremental** voxel edits (`Voxels`
per-cell mutation) so an active body's rebuild cost stops scaling with its whole
cell count — which would let `MAX_ACTIVE_COLLIDER_CELLS` rise. Not implemented
here.

## What this does not settle

- Cell size stays 0.25 m for terrain (unchanged). No cell-size revision is
  proposed from collision feasibility.
- Contact-energy thresholds for impact-triggered destruction are T21, not here.
- Large-collapse throughput (`≤ 2 s` for the designated stress case) is a G1
  integration measurement (T11), not a single-body microbenchmark.
- `enhanced-determinism` (a `rapier3d` feature) is available if cross-platform
  reproducibility is later required; the default `f32` build was used here and
  physics tests use position/energy tolerances, not exact hashes.
