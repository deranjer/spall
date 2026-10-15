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

These assignments are independently ready. A1 writes the frozen binary format,
not an invented animation interface. A2 consumes existing world queries and
does not depend on the asset codec. The coordinator checks A1 using A0 before
accepting the combined foundation. Workers use isolated managed worktrees,
Luna 6 at Medium reasoning (the user's explicit model choice), and do not push
or integrate their own work. Coordinator owns shared docs and Loopira updates.

## Frozen boundaries

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

## Following integration

Only after reviewing these foundations, assign a bounded animated preview and
then authoritative creature runtime work. Runtime needs stable identities,
species-owned rules, server-owned movement and collision/hit semantics,
replicated clip/time with client presentation, versioned durable recovery,
interest streaming and edit-aware movement validation. Keep creature voxels
separate from the quarter-metre static editor-scene flattening path.

The first world integration proof should cover one trout, crow and deer;
current water draining, ground destruction, obstacles, two-client late join,
reconnect and save/load must work before claiming populated-world support.
Ocean ray, gull and fox broaden the next content pass. No population capacity,
frame-time or navigation performance is accepted without measurement.

## Implemented evidence (2026-10-14)

ENG-201/202/203 are reviewed and accepted for their bounded foundation scopes.
The independent decoder reads all three authored animals; sampled joint motion
and loop endpoints pass CPU tests. The generated gallery seed-1/256-cell test
finds aquatic, flying and land candidates using live terrain/column water, then
rejects the fish after draining a column and the deer after removing support.
Dormant-ocean reads match eager water without waking or allocating regions.

ENG-204 adds sandbox `animal_visual`, preserving per-part geometry, source
materials/tints, joint transforms, parent visibility, pivot and 6.25 cm scale.
Unit scale is required; expansion is capped before allocation. Its bounded
production-renderer example writes 24 poses and three contact sheets:

```powershell
cargo run -p sandbox --features client --example animal-preview -- --bounded --out .local/animal-preview
```

Visual review on RTX 4080 SUPER / D3D12 prompted thinner fish fins and eyes,
charcoal crow plumage and a wider deer with four independent leg hinges.
Camera and floor are fixed per animal so root motion is not cancelled by the
preview. This is source animation/rendering evidence, not running creatures
in a world. Exact commands, results and limits: `reports/ENG-204-animal-preview.md`.

ENG-205/A4 is in progress to freeze the missing authoritative creature
lifecycle and game-facing tick contract before population implementation.
Current body-spawn and committed-edit APIs do not provide an animated actor
lifecycle.
`InitialGameWorldSetup` runs only at fresh-world creation, and the existing
`SimWorld::spawn_body` path bypasses committed topology creation (the server
uses it before client baselines). It cannot safely stand in for animal births,
deaths or spawning during play.
Creature collision/hits, editable ownership after damage/death,
replicated asset/clip state, durable recovery and streaming/reset identity all
need an explicit shared contract. The preview must not become a local-only
population shortcut.

The user authorized a Luna worker for ENG-205 after the preview closeout.
One Luna 6 Medium worker audits the cross-cutting lifecycle contract in a
dedicated `animal-lifecycle` worktree, starting from `71540f8`. It owns a new
`docs/animal-lifecycle.md` proposal and ticket-specific evidence; the coordinator
retains shared-doc integration and Loopira status/log ownership. The audit
must map existing APIs and freeze identity, source-part/cell ownership,
tick-boundary ordering, collision/hits, births/deaths, durable state and
baseline/interest/reset semantics before dependent runtime implementation.
Implementation slices may fan out only after that shared contract is reviewed.
