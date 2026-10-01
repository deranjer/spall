# World generation (`spall_worldgen`)

Status: ENG-113 (crate) and ENG-114 (sandbox materials, `--worldgen` server
flag, `cargo xtask play --worldgen`, water region, arena wall) are implemented;
ENG-115 (editor panel) is not. Numbers below are measured on the dev machine
unless marked target.

```
cargo xtask play --worldgen showcase --seed 1 --release
sandbox-server --serve ... --worldgen showcase --seed 1 [--worldgen-size 512]
```

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
  reads as `Unknown`. Outside the arena is absent, so the generator builds a
  **border wall**: the outermost brick ring (`BORDER_CELLS` = 32 cells = 8 m) is
  uniform bedrock up to `WALL_TOP`, above every peak. Generated content, water,
  spawns and cave mouths are kept strictly inside it (water columns in the wall
  are dropped). The arena edge given in `size_cells` includes the wall.

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

## Sandbox integration (ENG-114)
- `sandbox::game::manifest_v5_terrain` appends eight materials (ids 210-217:
  bedrock, deep-stone, sand, mud, moss, gravel, clay, snow) above the lamp (200)
  and below the 256+ appearance-variant range. A manifest extension may only add
  ids above the previous highest, so the low ids the first plan assumed were
  not available. Grass, sandstone and slate reuse the playground ids 10, 13, 14.
  The v5 content hash is pinned in `tests/appearance_evolution.rs`; clients and
  servers built before this no longer match the handshake hash.
- `sandbox::worldgen_scene` maps a `GeneratedWorld` to `WorldSetup` (terrain
  cloned, sharing dense payloads), a `WaterSetup` (box + margins, coarsen 2,
  worker at 20 Hz, same constants as editor scenes) and spawns.
- Water starts static (no springs or sinks); it is solver state from then on.

## Measured (1024-cell arena, seed 1, release, this machine)
- Generation 275 ms including the water plan (2D columns ~29 ms).
- 12,288 resident bricks: 8,599 uniform, 3,689 dense = 242 MB of dense cells
  (with the wall). **Dense surface bricks are the dominant memory cost.**
- Stand-up in the authoritative simulation: `Simulation::new` 1.2 s (per-brick
  terrain colliders + water region); 120 ticks 92 ms total, worst tick 31 ms;
  a player at the spawn rests grounded (`worldgen_scene` test, `--ignored`).
- Water: 153,635 cells; fluid domain 2.68M voxel cells of the 4M budget.
- Caves: 8.3% of deep rock (band asserted: 2-15%).
- Real play session (`cargo xtask play --worldgen showcase --seed 1 --release
  --shots ...`, release, one local client): the late-join baseline installed all
  12,288 bricks ~4.5 s after connecting; terrain first appeared on screen ~15-20
  s after joining and the client streams/meshes a window around the camera, so a
  camera jump (for example an overview from 110 m up) can show water only for
  40 s or more. Once meshed the window showed ~400k-940k terrain cubes at 60 fps.
  This is the existing client streaming behaviour, not generation; it is the
  main usability cost of a 256 m arena and is not yet optimised or gated.
- Seen in screenshots: snow-capped slate/stone peaks, meadow hills, sand/
  sandstone desert with a sharp grass border, the swamp basin with lake and
  river, cave-mouth pits in the meadow, and the dark bedrock wall. Cave
  interiors, moss/mud detail and lighting inside tunnels were not inspected.

## Client rendering and the hammer (ENG-114 follow-up)
Playing the first showcase exposed three problems, fixed in `spall_client`:
- **Frame rate (~36-45 fps flying).** The debug terrain renderer drew one cube per
  exposed cell (936k at the spawn). Frame time tracked the instance count
  (936k = 28 ms, 747k = 22 ms, <=490k = vsync). `build_instances` now merges
  exposed cells into boxes (`greedy_boxes`: x runs, equal rows, equal slabs), so
  the same views are 183k / 82k / 43k / 100k instances.
- **Rebuild time (1-2.4 s per rebuild, terrain popped in seconds late).** The
  walk did six `BTreeMap` lookups per solid cell. Each brick is now scanned from
  a padded array (`BrickGrid`) and fully buried uniform bricks are skipped:
  2300 ms -> 110 ms at the spawn, ~1000 ms -> 60-70 ms elsewhere (release,
  `worldgen_render_cost` test, `--ignored`). Equivalence with the old per-cell
  scan is tested on a random multi-brick volume.
- **Water always drawn.** Water was a global list of translucent cubes, sorted
  and uploaded every frame, with no distance limit, so it floated in the sky
  where terrain had not streamed. Water columns are merged into boxes once per
  keyframe and clipped every frame to the terrain view window (none before any
  terrain has landed).
- The terrain window now reaches 96 m below the camera (was 32 m), so flying
  above the peaks keeps the ground in view.

Measured in a real `cargo xtask play --worldgen showcase --uncapped --release`
session (frame time, not vsync-limited): 6-7 ms at the heaviest view (221k
instances, ~140 fps), 1.5-4 ms elsewhere. **Note:** with vsync on, a window that
is occluded or in the background is throttled by Windows to about 4 fps; use
`--uncapped` to measure.

**Hammer.** A crosshair marks the screen centre (white with a target in
reach, red without), a cyan wireframe shows the cells a swing would break, and
a label shows the radius and the material under the crosshair (or "click to
capture the mouse" until the cursor is captured). The target is found each frame
by casting the camera ray through the client's replica of the terrain with the
game's `DIG` reach (12 m). Left click (cursor captured) sends `DIG` along the
camera ray; hold to repeat (140 ms); the wheel sets the radius, 0-8 cells
(default 2; radius 0 breaks exactly one voxel, 1/2/3 break 7/33/123 cells; the
server caps it per tool). The client queues an `ActionRequest` through
`InteractiveSession::push_action` (which nothing drained before) and the
network thread sends it. The server finds the hit cell from the ray; the client
only supplies eye, direction and radius. `worldgen_hammer` is an end-to-end test:
a real server hosts a generated world and the client replica receives the crater.
The renderer culls back faces and only draws exposed cells, so flying (no
collision) into the ground shows through it; the ground is solid (see the
structure tests) and digging reveals the layers.

## Walking and the torch (ENG-114 follow-up)
- **WASD did nothing while flying worked.** The client predicts walking against
  its own physics world, and `ClientPhysics::set_terrain` built one dense grid
  over the whole replica: it found the bounding box by sampling every cell of
  every resident brick (hundreds of millions of samples for the 256 m arena) and
  the grid exceeds the physics grid cap (8M cells), so no collider existed and
  prediction never started (`ticks: 0`). For worlds whose resident bricks'
  bounding box exceeds the cap the client now mirrors a 193 x 129 x 193-cell
  window around the player (`windowed_occupancy`, scanned per brick) and rebuilds
  it when the player comes within 8 m of its edge; smaller worlds are unchanged.
  `worldgen_walk` (256 cells in CI, `WALK_SIZE=1024 --release` for the full
  arena) walks a scripted player: 34 m, 98% grounded, one 0.9 mm correction.
- **Torch (`T`).** Places the game's emissive lamp, the new `PLACE_LAMP` tool
  (`GAME_RULES_VERSION` 3, reach 12 m), as a radius-2 ball (33 voxels, about
  1.25 m across) against the surface under the crosshair. Smaller lamps do not
  light anything: measured offscreen at night (`worldgen_torch_light`), ground a
  metre away is 1.0x at radius 0 or 1, 1.6-3.1x at radius 2 and 2.7-20x at
  radius 3, because the lighting cache cannot resolve an emitter under about
  1 m. `worldgen_torch` checks the placement end to end.

## Not included
Trees, grass, props, flowing rivers (water is initially static, filled to sea
level; springs/sinks are a consumer decision), streaming/infinite worlds, 3D
editor preview, persistence of spec/version in `StoredWorldMeta`.
