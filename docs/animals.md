# Animated animal foundations

User authorized implementation on 2026-10-14 after the SPVX/world feasibility
audit. The first pass establishes reusable assets, CPU animation and live-habitat
validation. It does not claim that animals spawn in the shipped game.

## Ready assignments

| Slice | Ticket | Scope | Prerequisite |
| --- | --- | --- | --- |
| A0 | ENG-201 | CPU `spall_asset` SPVX assembly decoding and transform sampling | Existing SPVX v1.1 contract, T25 attachment point |
| A1 | ENG-202 | Deterministic articulated trout, crow and deer fixtures | Existing SPVX v1.1 contract |
| A2 | ENG-203 | Sandbox live terrain/water habitat candidate validation | Existing terrain and column-water snapshots |
| A3 | ENG-204 | Articulated render extraction and bounded GPU preview | Reviewed A0/A1 contracts |
| A4 | ENG-205 | Authoritative creature lifecycle and world integration contract | Accepted A0-A3 foundations |
| A4-P | ENG-209 | Exact articulated collision and source-cell query feasibility | Accepted source assets; independent of runtime lifecycle APIs |
| A4-R | ENG-210 | Articulated movement and collision-response rig feasibility | Reviewed ENG-209 measurements |
| Zoo | ENG-211 | Interactive aquarium, aviary and paddock showroom | Accepted A0-A3 assets and render bridge; independent of runtime actors |

These assignments are independently ready. A1 writes the frozen binary format,
not an invented animation interface. A2 consumes existing world queries and
does not depend on the asset codec. The coordinator checks A1 using A0 before
accepting the combined foundation. Workers use isolated managed worktrees,
Luna 6 at Medium reasoning (the user's explicit model choice), and do not push
or integrate their own work. Coordinator owns shared docs and Loopira updates.

## Frozen boundaries

The interactive zoo is a visual inspection scene using the production renderer
and all three source assets at their original cell scale. Launch with
`cargo run -p sandbox --features client --example animal-zoo`. Controls:
0 overview, 1 trout, 2 crow, 3 deer; WASD move, Q/E vertical, Shift faster;
click for mouse look; Space pause; Tab idle/action; Esc release/exit.
The free camera has no collision. The aquarium water and aviary are display
geometry, and specimens play authored clips without navigation, contact response,
spawning, damage, persistence or replication. This does not accept ENG-210.
For bounded evidence use `-- --capture --out .local/animal-zoo` (12 images),
or `-- --frames 90` (native presentation, resize and display-mode smoke).
See [ENG-211 evidence](reports/ENG-211-animal-zoo.md).

- Engine-owned asset parsing/sampling is CPU-only and has no game species,
  graphics, window, network or physics dependencies. Existing static importers
  continue their explicit rejection of unsupported assemblies until separately
  integrated; adding a library is not runtime animation support.
- SPVX v1.1 `PART`/`ANIM` encodings, 60 animation ticks per second, exact voxel
  runs, portable materials and hash rules come from `spvox-format.md`. Do not
  infer gameplay joints, damage or physical bodies from an authored hierarchy.
- Use unit-scale articulated parts. No skinning, animated topology, finer
  cell-size codes, second physics engine or per-voxel entities. An animated
  consumer may clearly reject unsupported structural/layer features.
- Authored animals use 6.25 cm cells; small-animal detail remains limited. Each
  asset has explicit reusable identity, anatomy, rest transforms and clip
  expectations. Generator determinism does not prove visual quality.
- Habitat checks validate a declared axis-aligned body envelope against current
  immutable terrain and water. Unknown terrain and exhausted query budgets
  reject candidates. Generation biome/climate can select habitat, but cannot
  establish live occupancy, clearance or support.
- Candidate validation is not pathfinding, articulation-aware collision or
  flight/swimming physics. Terrain removal and water draining must invalidate
  previously suitable candidates when queried again.

## Foundation acceptance

A0 needs bounded malformed-input/hash/canonical-order/hierarchy/reference tests
and rest/interpolation/loop/ping-pong pose tests. A1 needs deterministic
regeneration and independently decoded fixtures, including meaningful tail,
wing and leg motion. A2 needs full-volume edge collision, unknown terrain,
depth/draining, missing support, step/slope and work-budget tests. CPU checks
are separate from rendered captures and population/performance gates.

## Current runtime (2026-10-20)

Live animals are plain server-authoritative entities: an id, a species key, a
root pose and a clip. They are drawn from the species' SPVX asset and own no
world voxels, so they cannot be cut apart voxel by voxel and are not saved with
the world. The server moves deer and crows with a capsule swept against terrain;
clients receive spawn, despawn and motion records. Players and bodies pass
through them. `--animals N` spawns deer and crows in a worldgen sandbox; trout
need live column-water placement and are not spawned yet. The model, what it
replaced, and the planned hunting design are in
[animal lifecycle](animal-lifecycle.md). Habitat, species and spawn rules are
game content in `examples/sandbox`.

Planned, not built: hunting (death despawns the animal and the game emits
drops), swimming and flight physics beyond the capsule, saving animals, and the
population/performance measurements a populated world needs.
