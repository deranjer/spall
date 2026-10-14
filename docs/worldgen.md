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
  (water box plus sandbox margins) exceeds `WATER_DOMAIN_BUDGET` (64 Mi voxel cells; ENG-126).
  This gate is checked from the column map before terrain/water allocation and
  is also shown in the editor preview. Valid size syntax is not evidence that
  a complete world fits: seed 1 at 4096 cells (1024 m) needs 47,421,308 domain
  cells and is rejected. Large playable worlds need a separate capacity and
  integration change; the editor does not silently remove water or shrink them.
- **The fluid domain grows with digging (ENG-120).** The generated domain is the
  water box plus 1 m margins with closed sides, so water cannot leave it. The
  worldgen scene enables `WaterGrowth`: a committed terrain edit within 16
  voxels (4 m) of the domain that is not wholly inside it replaces the region
  with a larger one covering the edit plus 32 voxels (8 m), preserving every
  fluid cell's exact amount. Growth is capped at 3M voxel cells and refused (and
  counted) beyond that, over another region or non-resident bricks.
  See `docs/reports/ENG-120.md`.
- Every brick of the arena is resident (air included), so nothing inside it
  reads as `Unknown`. Outside the arena is absent, so the generator builds a
  **border wall**: the outermost brick ring (`BORDER_CELLS` = 32 cells = 8 m) is
  uniform bedrock up to `WALL_TOP`, above every peak. Generated content, water,
  spawns and cave mouths are kept strictly inside it (water columns in the wall
  are dropped). The arena edge given in `size_cells` includes the wall.

## Natural: banks at rest (version 6)
Fresh water stands level, so a bank lower than the water beside it lets it run
out. Through version 5 a river on a side slope, or a lake whose rim a river
channel was cut through, left dry columns up to 40 cells below the surface next
to the water (about 2,000-8,000 columns a world, 5-8 cells at the worst
places that mattered), and the solver started from that plan spilled the water
over them in a curtain and went on draining the lake. `raise_banks`
(`columns/natural.rs`) runs after the heights and water surfaces are computed and
raises every dry column beside fresh water (surface above sea level plus one) to
the highest water surface it touches: a levee, one column wide, never lower than
before. It depends only on the original columns, so it does not depend on
visiting order. The sea is left to the shore, and wet neighbours at different
levels (rapids and falls) are unchanged. The version is 6.

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
  cloned, sharing dense payloads), a `WaterSetup` (box + margins, coarsening picked per world to fit a
  50,000 fluid-cell target (3 at 512 cells, 4 at 1024), worker at 20 Hz) and spawns.
- Water starts static (no springs or sinks); it is solver state from then on.

## Historical measurement (1024-cell arena, seed 1, before ENG-114 palette storage)
- Generation 275 ms including the water plan (2D columns ~29 ms).
- 12,288 resident bricks: 8,599 uniform, 3,689 dense = 242 MB of dense cells
  (with the wall). **Dense surface bricks are the dominant memory cost.**
- Stand-up in the authoritative simulation: `Simulation::new` 1.2 s (per-brick
  terrain colliders + water region); 120 ticks 92 ms total, worst tick 31 ms;
  a player at the spawn rests grounded (`worldgen_scene` test, `--ignored`).
- Water: 153,635 cells; fluid domain 2.68M voxel cells of the then-4M budget
  (the current bounded capacity is 64 Mi voxel cells).
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
  (`GAME_RULES_VERSION` 3, reach 12 m, radius 0), as one voxel in the empty cell
  against the surface under the crosshair: it fits in a crack and does not fill
  the hole it lights. The renderer's lighting cache cannot resolve an emitter
  smaller than about a metre (a lone lamp voxel lit nothing, and boosting its
  emission did not help), so the window turns the nearest 8 emissive voxels into
  real-time **point lights** (`spall_render::PointLight`: warm, 14 m range,
  smooth falloff, no shadows, so light also leaks through thin walls). Measured
  offscreen at night (`worldgen_torch_light`): ground a metre away is 3.9-5.7x
  brighter. `worldgen_torch` checks the placement end to end.
- **Colour jitter.** Terrain used one flat colour per material. Opaque,
  non-emissive materials now get per-voxel variation in the opaque shader
  (`Material::jitter`, brightness about +-14% plus a smaller per-channel drift,
  hashed from the voxel the fragment lies in, so merged boxes keep their speed).
  It applies to axis-aligned cube instances only (terrain), not rotated bodies,
  and is client-side and cosmetic: no material ids, replication or saves change.
- **Hammer latency.** A swing took ~3.5 s to show on the full arena. Measured
  (`worldgen_hammer` with `HAMMER_SIZE=1024`, release; the commit-handler hook
  splits server from client): the server committed the cut 3458 ms after the
  click and the client replica saw it 93 ms later, so the cost was on the
  server. `stage_edit` built the structural index of the **whole terrain** for
  every cut (about 12k bricks: a 64 KiB per-cell label array even for uniform
  bricks, flood-filled, then a full graph reassembly twice, whose brick linking
  scanned 1024 cell pairs per face). It is now incremental and still exact:
  brick labels are compact (uniform bricks and single-component bricks need no
  per-cell array), memoized by revision in a pipeline-owned `LabelCache` written
  only from builds of the live volume (never a dry run), warmed when the
  simulation is created; per-component facts (faces, anchor layer) are
  precomputed; and neighbouring bricks link through 32x32-bit face masks
  (`BrickLabels::face_mask`) instead of cell scans. Cut commit on the full arena:
  3000 ms -> 135 ms; then three more removals took it to 44-52 ms (60-70 ms
  with the water running): (1) each brick memoizes its content hash and solid
  count (`Brick::content_hash`/`solid_cells`, shared by clones and snapshots,
  reset by any mutation), so a commit rehashes only the bricks it changed
  instead of every brick, and `logical_solid_cells` no longer reads 32,768
  cells per uniform brick; the client verifies each transaction with the same
  hash, so its share fell from ~80 ms to ~7-12 ms; (2) the water region
  re-captures its boundary (a scan of the whole fluid domain that also wakes the
  solver) only when a committed terrain brick is at or within one brick of its
  domain, not on every edit anywhere; (3) the server warms the label, hash and
  count caches when the world is created and the client warms its hash caches
  right after installing the baseline, so the first swing is not slower.
  Measured click to replica change on the full arena (release, real server and
  client): 3551 ms -> first swing 122 ms, later swings 91 ms (79 ms server +
  12 ms client). The `worldgen_hammer` test prints both. On the client, the terrain draw is also cached
  per brick (`TerrainInstanceCache`, keyed by the revisions of the brick and its
  neighbours and the window clip) and rebuilt the moment the replica's terrain
  changes: 1-4 ms (was 90-150 ms). The topology contract is unchanged: the
  structural scenario and oracle tests (38) pass, and a randomized test checks
  the compact labels, per-component facts and face masks cell for cell against a
  reference flood fill.

## Rendering after the ENG-100 merge

Main now draws terrain from incremental greedy meshes (`spall_mesh`), so the
window's rebuild worker was reworked on top of that path:

- Terrain meshes use a `vs_terrain` pipeline entry so the per-voxel colour
  jitter (keyed on the world cell) stays on for static terrain and off for
  moving body meshes.
- Torch/lamp point lights come from an `EmitterCache` (emissive cells of the
  visible dense bricks, rescanned only when a brick's revision changes).
- The worker sends terrain meshes first, then emitters and the sky grid; the sky
  grid is skipped when the terrain generation is unchanged and the player is
  still near the grid anchor.
- Hammer swings are previewed on a private copy of the replica terrain until the
  server's cut arrives (800 ms cap). Preview bricks and their neighbours bypass
  the mesh and emitter caches: a revision number identifies contents only along
  the server's history.
- The older instanced-box terrain builder (`build_instances`, with its brick
  cache) remains as the reference path for measurements; the live window does
  not use it.

## Presets (worldgen v4, ENG-186 to ENG-191)

`Preset::{Showcase, Natural, Gallery}`; each carries its own `gen_version()` and
world height, so adding or changing one never changes another's output.

| Preset | Version | Height | What it is |
|---|---|---|---|
| `showcase` | 3 (unchanged, digest-pinned) | 96 m (12 bricks) | the small arena above |
| `natural` | 5 | 128 m (16 bricks) | landform, erosion, hydrology, climate, thirteen biomes |
| `gallery` | 1 | 128 m | one pad per biome with all its plants (a showcase for looking at flora) |

`cargo xtask play --worldgen natural --seed 4 --worldgen-size 2048 --release`
(`gallery` with `--worldgen-size 1024` gives 54 m pads). The default world size is
4096 cells (1 km). A natural or gallery world has no `GEN_VERSION` digest pin;
tests assert invariants (below) instead.

### Natural preset

Everything below is derived from a 1 m coarse grid and sampled to 0.25 m cells.
Same determinism rule as the showcase (integer hashing, `+ - * /`, `floor`,
`sqrt`).

1. `landform.rs`: seed-placed ranges, a coast on every world (one sea edge, or a
   corner of two), tilt toward the sea, hills. **Every world has a sea**: ocean
   and beach are biomes too. (Earlier versions rolled landlocked maps; those seeds
   now get one sea edge.)
2. `erosion.rs`: stream-power erosion plus thermal relaxation; large pits are
   breached first so rivers reach the sea.
3. `hydrology.rs`: priority-flood, D8 flow, accumulation, lakes at their spill
   height, rivers from springs (and lake outflows), bed height that never rises
   downstream, river heads attributed once (no double counting through lakes).
4. `climate.rs`: temperature (latitude, lapse rate), wind and orographic rain,
   wetness near fresh water.
5. `columns/natural.rs`: the cell-resolution `ColumnMap` (height, biome,
   moisture, temperature, river distance, water surface, springs, sinks) and the
   biome table below.

**Biomes** (first match wins; `t` is degrees C, `m` wetness 0-1):

| Biome | Rule | Surfaces (top material) | Plant class |
|---|---|---|---|
| Ocean | under sea level water, more than 2 cells from a bank | sand, gravel, clay by depth | sea |
| Riparian | fresh water (rivers and lakes) and gentle ground within 7 cells of it | gravel, clay, mud, moss, grass | wet |
| Beach | at most 6 cells above sea, gradient < 0.5, 10+ cells from rivers | sand, gravel, sandstone | coast |
| Snow | `t < 1` | snow, slate, stone | cold |
| Scree | gradient >= 1.2, or >= 0.8 when `t < 6`, or cold and dry | slate, stone, gravel | rock |
| Swamp | within 40 cells of water, `m > 0.5`, gradient < 0.25, `t > 5` | mud, clay, moss, grass | wet |
| Alpine | `t < 4`, `m >= 0.22` | moss, gravel, dirt, grass | cold |
| Boreal forest | `4 <= t < 9`, `m >= 0.3` | moss, dirt, mud, grass | forest |
| Pine forest | boreal (and cool temperate) ground inside a pine-stand noise patch | dirt, moss, gravel, mud | forest |
| Steppe | cool and dry, or warmer with `m` below 0.32 | gravel, clay, grass, dirt | dry |
| Desert | `m < 0.16 + 0.012 (t - 9)` | sand, sandstone, gravel, clay | dry |
| Meadow | warm, `m` 0.32-0.5 | grass, dirt, moss, gravel | grass |
| Temperate forest | warm, `m >= 0.5` | grass, moss, dirt, mud | forest |

Slopes of 1.1 or more are bare rock in every biome. Biome borders are dithered.

**Every biome in every world.** After classification, `ensure_every_biome`
paints in any biome holding under 1% of the land (and at least 64 columns): a
ragged patch where terrain and climate suit it best, blending the biome's
moisture and temperature into the surroundings (`ColumnMap::repaired_biomes()`
reports what was painted). Thresholds are tuned so painting is rare: over 40
seeds at 1024 cells Swamp was painted in 7, Alpine in 2, Snow in 2, Desert in 1,
and nothing else (measured, `diag_biome_coverage`). Test:
`every_world_shows_every_biome_in_a_real_share` (24 seeds at 512, 6 at 1024).
**Three surfaces per biome**: `natural_stack` uses three patch noises plus
wetness; `every_biome_shows_at_least_three_surfaces` checks 16 worlds.

**Water.** The plan lists every wet column (`WaterPlan`: surfaces, springs,
sinks). Rivers start as full channels at rest. Springs are discharge sources
(`Discharge`, fixed-point 2^-16 cells^3/s, persisted in `ColumnWaterState` v4);
sinks hold the sea and terminal lakes level. The solver is
`spall_fluid::columns` (stacked columns with virtual pipes; see
`docs/water-agent-handoff.md`). Measured (seed 1, 4096 cells, 30 min of fluid
time): step p50 14-17 ms, p95 26-32 ms (budget 50 ms); wet columns off the
planned channels plateau at about 28k (0.17% of the map) holding about 1,200 m3.
Steady-flow sleep exists (`steady_steps`, exact ledger) and is **off**: it gave no
measured benefit.
Memory: `ColumnWorld::allocate` loads solid masks in batches of 256, and with
`ColumnConfig::lazy_ocean` still deep sea (a region wholly under one flat level
with all eight neighbours the same) is kept as a per-column record (about 8 KiB
instead of about 200 KiB) and revived with exact volume the moment an edit,
spring or drain, restore, or moving neighbour touches it. Measured on
`eng187-natural-1024m-stress` (Natural seed 5, 4096): server process peak 9.07 ->
8.40 -> **7.73 GiB** (target 8). A restore from a save allocates every saved
region (not lazy).

**Spawns** sit near water (2-20 m), falling back to any flat dry spot.

### Gallery preset

Thirteen pads (4x4 grid, the sea alone in the last row), each 54 m at 1024 cells:
Meadow, Temperate forest, Boreal forest, Pine forest / Steppe, Alpine, Scree,
Snow / Desert, Swamp, Riparian, Beach / Ocean. Each pad has its own surfaces and
a wetness ramp from drier west to wetter east; the wet ones have water (a sea 16 m
deep, a riparian pond 10 m deep and a shallow stream, swamp pools). No cave
mouths. `LivingState::generate` places every species of a biome on its pad
(trees twice, other plants four times). One spawn per dry pad.
`cargo run --release -p sandbox --example gallery_legend` prints and writes
`.local/gallery-legend.md`: each pad's centre in metres, trees, plants and cover.

## Vegetation v2: living plants and stateless ground cover (ENG-189)

`spall_ecology` has two kinds of plants. **Living** plants (trees, reeds, ferns,
flowers, scrub, and the underwater kelp, corals and weeds) are organisms in
`LivingState` (version 2): they grow, seed, die and are harvestable; trees are
real wood in the terrain. **Ground cover** (`spall_ecology::cover`) is stateless:
`ground_cover(set, seed, cell, soil, moisture, temperature, biome, season, depth)`
is a pure function, so there is no per-plant record, save or wire message. The
climate it reads is a run-length-coded 4 m raster (`CoverField`: biome, moisture,
temperature and water level) that the world carries in `LivingState` and every
`VisualFrame`.

- **Per-biome catalogue** (sandbox): 25 trees, 27 living ground and underwater
  plants, 48 cover species, with species masks over the thirteen biomes (`u16`).
  Measured against real worlds (a kind counts with 40+ matching columns): Meadow
  11 living / 9 cover, Alpine 14/8, Swamp 7/5, Desert 7/3, Beach 4/3, Riparian
  12/11, Temperate forest 11/6, Boreal 12/6, Scree 6/4, Snow 3/2, Steppe 12/9,
  Ocean 6/6, Pine forest 7/1. (`tests/biome_flora.rs`.)
- **Wetness**: cover density is `0.12 + 0.55w + 0.25w^2` of a species' density
  (dry ground about a fifth), height scales 0.85-1.15x, and most cover species only
  set a minimum wetness, so wetter ground admits more kinds.
- **Low forms** beyond blades: mounds (moss, thrift), flower patches, pebbles and
  shells, fallen leaves, pine needles; underwater kelp, branching coral, sea fan,
  barrel sponge. **Pine forest** has needles as its only cover.
- **Underwater**: `Species.depth` / `CoverSpecies.depth` give the water depth
  range in cells (`[0, 0]` is dry land). Kelp needs 4 m of water, fresh-water
  weeds 7-8 m; land plants refuse any water.
- **Destroyed cover** (`LivingState.cleared`, sorted `(x, z, at_s)`, at most
  65,536): an edit that reaches a column's topsoil records it. Cleared cover
  stays bare 20 s of ecological time, regrows small (0.3x) to 90 s, medium (0.65x)
  to 180 s, then is full size and the record is dropped. A tunnel well below the
  surface leaves cover alone. The client holds back cover in a column whose surface
  just changed until the record arrives (3 s cap).
- **Saves**: `LivingState` v1 upgrades on decode: carpet species (forms 6, 9,
  11, 12) become cover species and only their organisms are dropped; the clock,
  ids and every other organism carry over. `LivingState.preset` (0 showcase, 1
  natural, 2 gallery) makes the server rebuild the climate the world was made
  from (it used to assume the showcase).

### Living vegetation at 1 km (ENG-190)

- **Ecological placement** (`spall_ecology::placement`). Trees are dart-thrown in
  a fixed pseudo-random order over 1 m candidates. Density comes from the
  *spacing*: a biome's canopy value (Pine forest 1.0, Temperate forest 0.95,
  Boreal 0.85, Riparian 0.7, Swamp 0.45, Meadow 0.18, Alpine 0.12, Steppe 0.06,
  Beach and Scree 0.03, Desert 0.01, Snow and Ocean 0) scales a species' spacing
  from 2.2x (open) to 0.85x (dense); glade noise (56-cell lattice) thins or
  removes patches of canopy. A 40 m stand has one dominant species (72% of its
  trees; the choice ranks the species that could grow there by a stand hash, so
  it does not shift with local soil), the rest are mixed, which makes ecotones
  at stand edges. The understory (living ground plants) is rolled per 1.25 m
  candidate by biome fertility, and plants that are not within 7 cells of a tree
  are kept at 55%, so it gathers in shade. Placement is deterministic for a seed.
- **Spatial index** (`Spatial`, 16-cell buckets, derived and never saved) answers
  spacing, trunk-footprint and "a tree never grows over a ground plant" checks in
  a few buckets, where they used to scan every organism.
- **Caps** `MAX_TREES` 24,576 and `MAX_GROUND` 32,768 (cover is stateless and not
  counted); skeleton cell limit 2.5 million.
- **New forms and species.** Forms 22 shrub (woody stems under a leafy crown),
  23 soft bush (a leafy mound), 24 tall herb (stems with paired leaves and a
  flower spike, seed head out of season). Thirteen species, ids 120-132: Hazel,
  Bramble, Juniper, Heather, Sagebrush, Bog myrtle, Dwarf willow, Mullein,
  Foxglove, Fireweed, Joe-pye weed, Tamarisk, Sea buckthorn, across every fertile
  biome including Desert and Beach.
- **Saved state version 3** saves only what changes. A tree's skeleton is a pure
  function of its root, species and id (`skeleton`, keyed `hash(seed ^ id)`), so
  the save keeps the removed-cell indices and the skeleton grows back on load.
  `skeleton()` is part of the format: changing a species' shape needs a new state
  version. Version 1 and 2 saves still load; a tree in them that does not match
  the rule is refused loudly rather than reshaped. `encode()` skips per-cell
  skeleton checks (the program grew them); decode of an older save does them all.
- **Regional vegetation replication.** The server builds the full keyframe once a
  second on a worker; each regional client (one with a streamed radius) then
  gets only the plants and destroyed-cover records inside a square of
  `(radius + 2) * 32` cells around its player; a client that streams the whole
  world still gets everything. The climate raster never changes, so it is sent
  with a session's first keyframe and every tenth after (a client keeps the last
  one; a world reset clears it). The keyframe is droppable traffic as before.
- **Fair work** still visits a bounded number of organisms per second:
  `clamp(n / 40, 128, 512)`.
- **Growth is throttled.** A new world's trees are all fully grown. Trees seeded
  while the world runs grow one wood cell per 300 ticks (5 s) in total, however
  many are growing (`GROWTH_INTERVAL_TICKS` in `spall_sim`). Wood is real
  terrain, so each cell is a full edit commit on the tick thread: at 4096 cells
  about 0.2 s of staging and commit plus about 0.3 s to refresh the water's
  terrain (an O(world) remap of 6.6 million spans). Before the throttle, a 1 km
  world with 8,600 trees spent 130 to 660 ms of a tick in `tick.water` once a
  second and a walking player rubber-banded; after it, a 10 minute run of
  simulated time had three 300 to 390 ms ticks.

- **Foliage level of detail (client).** Tree crowns beyond 36 m are drawn at half
  resolution (`LivingState`'s `VisualFrame::parts_lod`): leaf blocks of 2 x 2 x 2
  cells, half a metre, filling the same space with about an eighth of the cubes.
  Ground plants and near trees are unchanged. On the 4096 natural world the
  foliage set fell from 69,935 to 42,203 instances, its rebuild (on the foliage
  worker thread, about once a second) from about 50 to 23 ms, and a standing frame
  from 8.3 to 7.2 ms (121 to 141 fps; scene GPU 3.4 to 2.6 ms). A coarser second
  level (1 m blocks beyond about 65 m) gave nothing measurable and was dropped.

Measured (natural, seed 1, 4096 cells, release, `vegetation_scale` example):

| | before W5 (512 / 8,192 caps) | after |
|---|---|---|
| trees / living ground plants | 512 / 8,192 (cap-bound; at 2048 cells) | 8,651 / 10,080 |
| saved state | 0.52 MiB at 2048 cells | 0.39 MiB at 4096 (5.75 MiB before the v3 save) |
| keyframe per client per second | 0.14 MiB at 2048 cells | whole world 0.65 MiB; regional 8 / 14 / 24 KiB at 4 / 8 / 16 bricks |
| state copy on the tick thread | 10.3 ms (v2 layout) | 0.73 ms |
| `encode()` incl. validation | 7.6 ms | 2.9 ms |
| `advance` per second of ecology | 0.23 ms (2048) | mean 2.7 ms, worst 5.5 ms |
| vegetation generation | n/a | 1.23 s of a 10.5 s world start |

## Editor panel update (ENG-191)

The World Gen dialog (File > Generate World...) now picks the world (Natural,
Showcase, Gallery; default Natural) as well as seed, size and season, and shows
the preview as one of six layers drawn from the same `ColumnMap` the server
builds (`spall_worldgen::debug::layer_rgba`):

| Layer | Shows |
|---|---|
| Biomes | biome colour shaded by height (the old minimap), legend of shares |
| Height | green lowland through brown to white peaks; water by depth |
| Moisture | dry tan to wet blue, range in the legend |
| Temperature | cold blue through warm yellow to hot red, range in degrees C |
| Rivers, lakes and springs | sea, fresh water, springs (yellow), sinks (magenta) |
| Vegetation density | an **estimate** from biome fertility and wetness (`vegetation_density`), not a plant count |

Switching layer redraws the stored map (no regeneration). Run in game passes the
chosen preset to `cargo xtask play --worldgen`. Logic is in
`tools/spall_editor/src/worldgen_panel.rs` (tested on the CPU), layers in
`crates/spall_worldgen/src/debug.rs` (`tests/preview_layers.rs`).


## Editor panel (ENG-115, first version)

File > Generate World... opens a dialog in `spall-editor` (no project needed):
seed field (empty = 1), arena size (128 m or 256 m), Randomize seed, Generate
preview, and Run in game.

- The preview is a top-down map of the server's own `ColumnMap` (biome colour
  shaded by height, snow caps, water darkening with depth) with a legend of
  biome shares, water share and height range. A test pins it to
  `top_down_rgba(ColumnMap::compute(spec))`, so what you see is the world the
  server builds from the same seed and size.
- Run in game spawns `cargo xtask play --worldgen showcase --seed N
  --worldgen-size S` from the workspace, like the scene Run button.
- It is a 2D map, not an in-viewport 3D preview: the viewport only renders
  `.spvox` scenes. Caves are not shown (the map is columns only).
- Logic lives in `tools/spall_editor/src/worldgen_panel.rs` (CPU-testable);
  the dialog is in `ui.rs` and the texture upload in `main.rs`.

## Not included (showcase preset)
The showcase arena itself still has no trees, grass or flowing rivers; the
natural preset below adds rivers, and vegetation lives in `spall_ecology`.
Still not included anywhere: infinite worlds, a 3D editor preview, persistence of
spec/version in `StoredWorldMeta`.


## Vegetation plan boundary (ENG-116)

`spall_ecology` consumes terrain and explicit ecology inputs to produce a separately versioned plan. It does not modify `GeneratedWorld`, the worldgen version, or the terrain golden digest. The sandbox supplies its initial species and material mappings. The bounded clearing example routes accepted wood proposals through the existing simulation edit/staging surface, but production server ownership, checkpoint/journal recovery, and replication remain a subsequent assignment. Grass remains patch state; no blades or foliage voxels are generated. The existing worldgen pipeline still has no vegetation pass.

## ENG-126 larger world integration

The bounded fine water domain now permits 64 Mi voxel cells. Canonical water
schema 2 is used only beyond schema 1's 32 Mi limit; legacy smaller states
retain schema 1. Seed-1 4096-cell (1024 m) worlds keep their exact 47,421,308
cell water box. Coarse-solid overlaps retain authored water via conservative
displacement or the trapped ledger, including checkpoint/journal recovery.
Terrain generation and its golden digest are unchanged.

The existing coarsening selector stops at factor 8, so large worlds may exceed
the 50,000 fluid-cell target. No solver tolerance, dt or workload is reduced to
claim that target. Live water transport retains its existing limits. Large
world network baselines use negotiated version 3 with bounded segment staging;
see `docs/protocol.md` and `docs/reports/ENG-126-large-world-integration.md` for
measurements and remaining hardware/performance gates.

## ENG-114 memory and full-world stress follow-up

Exact internal palette storage, streamed collider staging and atomic reset
payload reuse are recorded in [memory/stress evidence](reports/ENG-114-memory-stress.md).
The fixtures exercise full 4096-cell residency, eight foundation digs, two
network replicas and two resets. Performance and impaired-network gates remain
open; consult the report before claiming large-world acceptance.
