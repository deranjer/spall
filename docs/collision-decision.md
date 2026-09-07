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
| Hollow building drop — settles, interior clearance kept | yes, 1.0 m | yes, 1.0 m |
| Mass / centre-of-mass error vs analytic reference | 0 / 0 | 0 / 0 |
| Sorted principal-inertia error vs analytic reference | 6e-6 | 0 |
| Fast CCD projectile vs 0.5 m wall — max speed still stopped | **20 m/s** | **220 m/s** |
| Worst-case fragmentation (16³ checkerboard, 2048 isolated cells) — primitives | 1 | 2048 |

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

## Known limitation and the coarse-fracture policy

The greedy decomposition degenerates to **one box per cell** on highly
fragmented occupancy: a 3-D checkerboard of 2048 isolated cells produces 2048
compound parts. Thin diagonal filaments and single-voxel speckle do the same.
Unbounded, this defeats the performance argument for a badly-shaped body.

Deterministic mitigation, to be enforced by `spall_sim` when it owns collider
builds (T08) and measured at the scale gate:

- **Primitive budget per body:** `B = 4096` merged boxes (provisional).
- **When `greedy_boxes(grid).len() > B`:** build the collider from a
  **coarsened** occupancy instead — downsample the body's own grid by an integer
  factor `k` (2, then 4) where a coarse cell is solid iff **any** covered fine
  cell is solid. This is conservative: it only ever *adds* collision volume
  inside the body's own footprint, never removes gameplay matter, and the fine
  voxel grid remains the authoritative geometry. Record `k` in the body's
  collider revision so replication and persistence agree on what was built.
- **Never** silently drop parts or fall back to a convex hull.

If a future workload shows fragmentation is common rather than pathological, the
fallback to re-evaluate is `parry`'s incremental voxel edits (`Voxels`
per-cell mutation) rather than a full voxel-shape rebuild — not measured here.

## What this does not settle

- Cell size stays 0.25 m for terrain (unchanged). No cell-size revision is
  proposed from collision feasibility.
- Contact-energy thresholds for impact-triggered destruction are T21, not here.
- Large-collapse throughput (`≤ 2 s` for the designated stress case) is a G1
  integration measurement (T11), not a single-body microbenchmark.
- `enhanced-determinism` (a `rapier3d` feature) is available if cross-platform
  reproducibility is later required; the default `f32` build was used here and
  physics tests use position/energy tolerances, not exact hashes.
