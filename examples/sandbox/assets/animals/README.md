# Sandbox articulated animal fixtures

These are small authored SPVX 1.1 source assets, made for format/animation
handoff and later importer fixtures. They are not wired to spawning, gameplay,
rendering, or an animation runtime. Each file has raw canonical
`META`/`MTRL`/`PALT`/`PART`/`VOXL`/`ANIM`/`HASH` chunks; there is no `STRC` or
`LAYR`. Geometry is sparse part-local topology at SPVX cell-size code `1`
(6.25 cm), with unit rest scale and integer-cell pivots. Animations transform
parts only; they never change voxel topology.

## Reproducible authoring

The source and schema writer are the standalone sandbox example
`examples/sandbox/examples/animal_fixtures.rs`. From the repository root:

```powershell
cargo run -p sandbox --example animal_fixtures -- --write
cargo run -p sandbox --example animal_fixtures -- --check
```

`--write` recreates these three files. `--check` generates each asset twice in
memory, checks byte-for-byte determinism against the checked-in files, verifies
the raw chunk bounds and final logical BLAKE3 hash, and checks authored part,
clip, duplicate-cell, and rest-bounds invariants. No generator dependency was
added; the example uses the sandbox's existing `blake3` dependency.

## Portable materials and palette

The sorted `MTRL` keys map explicitly to existing names in
`sandbox::game::manifest()`:

| Key | Sandbox material | Authored use |
| --- | --- | --- |
| `dirt` | `materials::DIRT` (`dirt`) | Deer muzzle and hooves |
| `moss` | `materials::MOSS` (`moss`) | Trout fins |
| `snow` | `materials::SNOW` (`snow`) | Deer antlers and eye accents; crow eyes |
| `stone` | `materials::STONE` (`stone`) | Trout/crow bodies and crow wings |
| `wood` | `materials::WOOD` (`wood`) | Deer coat/legs, crow beak/legs |

The palette holds display-only sRGB variation; it does not add material keys or
change material physics. Palette slot 0 is omitted from a run and means the
manifest's ordinary material appearance. Palette entries are:

| Slot | sRGB | Intended accent |
| ---: | --- | --- |
| 1 | `#4C81AA` | Trout back |
| 2 | `#CDB37B` | Trout underside |
| 3 | `#2C4331` | Dark fins and crow plumage |
| 4 | `#785B37` | Warm fur and legs |
| 5 | `#E0DCBF` | Light beak, eyes, and antlers |

Each asset has a distinct, fixed portable UUID (bytes shown in canonical UUID
hex form):

| Asset | UUID | Rest bounds, cells (X × Y × Z) | Envelope, metres (X × Y × Z) | Authored cells | Parts |
| --- | --- | ---: | ---: | ---: |
| Trout | `5a871001-2026-4001-8000-000000000001` | 15 × 7 × 7 | 0.9375 × 0.4375 × 0.4375 m | 199 | 5 |
| Crow | `5a871002-2026-4001-8000-000000000002` | 14 × 10 × 13 | 0.875 × 0.625 × 0.8125 m | 221 | 6 |
| Deer | `5a871003-2026-4001-8000-000000000003` | 14 × 19 × 3 | 0.875 × 1.1875 × 0.1875 m | 221 | 7 |

The bounds and cell counts are measured directly from the generated authored
cells and parent/rest translations. They describe asset-local bounds, not a
game-world size or visual-quality claim. The root pivot is the origin at
`(0,0,0)`; each child pivot is placed at the named joint in its parent's local
cell grid. All rest rotations are identity, all rest scales are unit, and all
`structural_profile_slot` fields are zero.

## Parts and expected motion

| Asset | Part hierarchy highlights | Looping transform clips (60 ticks/s) |
| --- | --- | --- |
| Trout | Body/root; rear tail peduncle and forked caudal fin; dorsal fin; near/far pectoral fins | `idle` gently bobs the body over 120 ticks; `swim` yaws the tail joint through a 60-tick cycle. |
| Crow | Torso/root; head and beak; independently hinged near/far wings; tail fan; legs/feet | `idle` gently lifts the torso over 120 ticks; `flap` alternates the two wings in opposition over 48 ticks. |
| Deer | Torso/root; neck; head/muzzle; branched antlers; forelegs; hindlegs; raised tail with pale underside | `idle` gently lifts the torso over 120 ticks; `walk` swings forelegs and hindlegs in opposing phase over 48 ticks. |

The main directional features are modeled as actual occupied voxels: trout
torso, taper, fins and fork; crow head, tapered beak, paired broad wings, fan
tail and feet; deer long neck, muzzle, branching antlers, four lower limbs and
tail. Clip intent is stored as part transforms only. It remains unexecuted by
the sandbox until a later importer/runtime task implements it.
