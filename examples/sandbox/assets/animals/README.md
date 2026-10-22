# Sandbox articulated animals

Five authored SPVX 1.1 animals the sandbox spawns as live creatures: trout,
crow, deer, bear (grizzly) and leopard (snow leopard). Each file has raw
canonical `META`/`MTRL`/`PALT`/`PART`/`VOXL`/`ANIM`/`HASH` chunks; there is no
`STRC` or `LAYR`. Geometry is sparse part-local topology at SPVX cell-size code
`1` (6.25 cm), with unit rest scale and integer-cell pivots. Animations
transform parts only; they never change voxel topology.

## Reproducible authoring

The generator is the sandbox example `examples/sandbox/examples/animal_fixtures.rs`,
with one module per species under `examples/animal_fixtures/`. Shapes are built
from ellipsoids, tapered limbs and painted cells. From the repository root:

```powershell
cargo run -p sandbox --example animal_fixtures -- --write
cargo run -p sandbox --example animal_fixtures -- --check
```

`--write` recreates these files and `assets/characters/player.spvox`. `--check`
generates each asset twice in memory, checks byte-for-byte determinism against
the checked-in files, verifies the raw chunk bounds and final logical BLAKE3
hash, and checks authored parts, clips, duplicate cells and rest bounds.

## Materials and palettes

Material keys map to names in `sandbox::game::manifest()`: `wood` for fur,
feathers and antlers, `stone` for hooves, claws, noses, fish skin and crow
plumage, `moss` for fins, and `clay`/`snow` for a few eye and nose accents.
Each animal has its own display palette, so colour comes from the tints, not
the material. Palette slot 0 means the manifest's ordinary appearance.

## Assets

Facing +X, +Y up, +Z to the animal's left. Sizes are the generated rest bounds
(including antlers, tail and wings).

| Asset | UUID | Rest bounds, cells (X × Y × Z) | Approx. size | Cells | Parts |
| --- | --- | ---: | --- | ---: | ---: |
| Trout | `5a871001-2026-4001-8000-000000000001` | 14 × 7 × 5 | 0.85 m long | 102 | 5 |
| Crow | `5a871002-2026-4001-8000-000000000002` | 16 × 10 × 19 | 0.5 m body, 1.2 m wingspan | 228 | 6 |
| Deer | `5a871003-2026-4001-8000-000000000003` | 33 × 32 × 9 | 2.0 m long, 1.1 m at the shoulder | 2,023 | 13 |
| Bear | `5a871004-2026-4001-8000-000000000004` | 44 × 22 × 13 | 2.75 m long, 1.3 m at the hump | 6,578 | 12 |
| Leopard | `5a871005-2026-4001-8000-000000000005` | 39 × 13 × 7 | 1.2 m body plus 0.8 m tail, 0.75 m tall | 1,112 | 13 |

## Parts and clips

Quadruped legs have an upper and a lower part, hinged about Z at the
shoulder/hip and knee/hock; a positive angle swings a hanging limb forward.
All clips loop at 60 ticks/s.

| Asset | Parts | Clips |
| --- | --- | --- |
| Trout | body (root), tail with forked fin, dorsal fin, two pectoral fins | `idle`, `swim` |
| Crow | torso (root), head and beak, two wings, tail fan, legs | `idle` (perched, wings folded), `flap`, `hop`, `peck`, `land`, `takeoff` |
| Deer | torso (root), neck, head with ears, antlers, tail, 8 leg parts | `idle`, `walk`, `run`, `graze`, `alert`, `sleep` (bedded) |
| Bear | torso with hump (root), neck, head, tail, 8 leg parts with clawed paws | `idle`, `walk`, `run`, `swipe`, `eat`, `stand` (rears up) |
| Leopard | torso (root), neck, head, two-part tail, 8 leg parts | `idle`, `walk`, `stalk`, `pounce`, `run` |

The live game plays `idle` and each species' move clip (`walk`, `flap` or
`swim`). The other clips are authored for the animal behavior work (ENG-226)
and are not selected by the director yet. Render every clip with:

```powershell
cargo run -p sandbox --features client --example animal-preview -- --bounded --out .local/animal-preview
```

It writes four poses per clip, one contact sheet per animal and `summary.json`.
