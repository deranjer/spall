# World generation (`spall_worldgen`)

Status: ENG-113 (crate) is implemented; ENG-114 (sandbox/server/xtask wiring,
materials) and ENG-115 (editor panel) are not. Nothing here is playable yet.
Numbers below are measured on the dev machine unless marked target.

## Contract
- A world is a **pure function** of `WorldGenSpec { preset, seed, size_cells,
  palette, volume_id }` and `GEN_VERSION`. Same inputs, same voxels, on every
  platform and in debug and release (the noise uses only `+ - * /`, `floor`
  and `sqrt`; no libm transcendental functions). `golden_digest_for_version`
  pins the output.
- **`GEN_VERSION` policy.** Any change that alters generated voxels for an
  existing `(spec, seed)` bumps `GEN_VERSION` and the pinned digest in the same
  commit. Generated bricks never overwrite modified bricks of a saved world
  (see `docs/architecture.md`: modified-air tombstones win over regeneration);
  a save written under version N keeps its bricks under version N+1.
- **Dependencies:** `spall_core` and `spall_voxel` only. The game passes its
  own material ids through `WorldgenPalette`; the engine never names game
  materials.
- **Water is data, not a material.** `GeneratedWorld::water` (`WaterPlan`) lists
  full water cells between the terrain and `SEA_LEVEL`. The consumer builds the
  fluid solver's `WaterSetup` from it. Generation fails with
  `GenError::WaterBudget` rather than truncating when the fluid domain
  (water box plus sandbox margins) exceeds `WATER_DOMAIN_BUDGET` (4M voxel cells).
- Every brick of the arena is resident (air included), so nothing inside it
  reads as `Unknown`. Outside the arena is absent; the game must bound it.

## Pipeline
1. `columns.rs` (2D, cheap, previewable): layout weights from warped
   normalized coordinates -> height, `Biome`, basin weight, surface `Stack`.
2. `caves.rs`: a scalar field sampled on a 4-cell lattice per brick.
   Natural caves (cheese chambers + spaghetti tunnels) are gated: never below
   cell 40, never within 6 cells of the surface (24 under the swamp basin).
   Eight explicit *mouths* (sloped tunnels ending in a chamber) guarantee
   walkable entrances.
3. `generate.rs`: per brick column, per brick: above terrain -> uniform air;
   deep pure rock with no cave potential -> uniform brick; else dense fill.
   Rock bands by height: bedrock `y < 32` (one brick, the anchor plane),
   deep stone `< 96`, stone above. Columns are built on scoped threads and
   inserted on the calling thread.

## Showcase preset (256 m arena, 1024 cells; world height 96 m)
Mountains (slate/stone, snow above `SEA_LEVEL + 105`) in the north, meadow
hills in the middle, desert (sand over sandstone) south-east, and a circular
swamp basin south-west (moss/mud shores, gravel and clay beds) holding a lake
and a river. Terrain outside the basin is always above sea level, so all water
is inside one bounded basin. Coordinates are normalized, so the layout scales
to smaller arenas (tests use 256 cells).

## Measured (1024-cell arena, seed 1, release, this machine)
- Generation 304 ms (2D columns 29 ms).
- 12,288 resident bricks: 8,150 uniform, 4,138 dense = 271 MB of dense cells.
  **This is the dominant cost**; surface bricks are dense by nature.
- Water: 153,635 cells; fluid domain 2.68M voxel cells of the 4M budget.
- Caves: 8.3% of deep rock (band asserted: 2-15%).
- Not yet measured: terrain collider cost, late-join snapshot size, meshing
  time for the arena (ENG-114).

## Not included
Trees, grass, props, flowing rivers (water is initially static, filled to sea
level; springs/sinks are a consumer decision), streaming/infinite worlds, 3D
editor preview, persistence of spec/version in `StoredWorldMeta`.
