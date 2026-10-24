# Water direction: findings and options (decision needed)

Date: 2026-10-23. Status: decided 2026-10-23 (see Decisions): option C. Nothing here is implemented
on `main` except PR #226 (the dam fix and slow-motion far springs).

## The question

How the engine should keep water "alive" (river currents, floods, canals the
player cuts) inside the server's water budget on a 1 km world and larger. The
options are below; the recommendation is **option C: the same column model at
1 m cells, with evaporation, a held sea and settling at world creation**, with
no fallback to option E, because rivers must be dammable (see Decisions).

## Goals and constraints

- Rivers visibly flow and swimmers feel their current. Floods and dam breaks
  happen. A canal the player cuts fills and flows.
- Server-authoritative, CPU-only dedicated server, multiplayer, full-world
  destruction (unchanged requirements).
- Budget: the water worker gets half the cores; about 8 ms per 1/60 s slice is
  the working target. Every timing below is on one development PC with all
  cores, so the worker would be slower than shown.
- No visible failures: water never walls up or hangs in the air
  (`docs/water-agent-handoff.md`, user priority 1).

## What we measured

All runs: `examples/water_long_probe.rs`, natural preset, seed 1, 4096 cells
(1 km), player standing at (374, 388) m unless stated. Experiment code is on
the local branch `exp/water-hybrid` (environment variables, not for merge).

| Setup | Active regions | ms per step (whole water tick) | Notes |
|---|---|---|---|
| Old code (before PR #226), 3,000 s | 136 | 4.1 | Rivers dammed at sleeping regions; calm because water was stuck |
| Dam fix (PR #226), 900 s | 657, climbing | 14.0 (solver 7.7) | Rivers reach the sea; sea wakes at the mouth |
| No springs at all | 0 | 0.6 | Fixed overhead only |
| Dam fix + evaporation + held sea, 2,400 s | 350-430 plateau | 8.9-11 (solver 6-9) | Growth stops |
| Same, settled first (3,300 s unthrottled), then 900 s of play | 754-817 | 18.6-19.6 (solver 16-17) | Settling wakes every river and lake |

Findings:

1. **The old calm was the bug.** Sleeping regions woke only when one pipe
   exceeded the sleep flow, so rivers dammed at the first sleeping region (the
   standing slab). With that fixed (PR #226) the spring-fed rivers run to the
   sea, and the in-game growth (130 to 1,137 active regions) is that water
   arriving, earlier.
2. **The world never settled because water only came in.** Springs add about
   2 m3/s; only a few drains at the sea edge took any. Evaporation sized to the
   inland water (1.45e-4 cells/s, about 4 cm per game day over 56,000 m2)
   balances them. Holding the sea at a fixed level, so a river ends where it
   meets it, removed the awake block of sea at the river mouth and halved the
   active regions.
3. **Rivers in this solver do not slosh.** Synthetic channels (slopes 1/64 to
   1/4, beds in 4- and 8-cell falls, either spring kind, pit or drain outlet,
   1/60 or 1/20 s steps) settle to a depth swing of at most 0.024 cells (6 mm)
   over 10 s.
4. **Storing rivers by freezing them does not pay.** The existing steady test
   (a 2 s window: volumes held, flows balanced, slow drift) never froze a region
   in play (its window restarted whenever a far region rested). With that and
   the still-span check relaxed, it froze 21-36 of about 780 regions even on a
   fully settled world. The settled world is about 750 regions of water that
   really moves (only 10 under 3 cm/s top speed).
5. **The cost is per moving cell.** About 20 ns per span per substep; the
   settled world steps about 570,000 spans at 1.5 substeps per slice. At 8 ms
   that budget is about 270,000 span-substeps per slice, so today's resolution
   cannot afford a world whose rivers all run.
6. **Generated water starts out of rest.** At seeding 173 columns stand over a
   metre above an open neighbour (lakes over dry cave pockets, river reaches as
   5-cell stair-steps). Settling removes them (2 left), but takes 3,300 s of
   water time and 1.9 h of wall time at today's resolution.

## Options

### A. Static water and painted currents (Valheim)

Water fills a fixed level; rivers are channels of it; currents are a texture.
Valheim's sea is a fixed plane and nothing flows downhill (community sources;
no official plan for flowing water).
- Cost: about zero. Fails canals, floods and dams. Not recommended.

### B. Keep today's model and tune it

0.25 m columns everywhere, sleep and throttle heuristics.
- Measured: 350-430 active regions and 9-11 ms per step with evaporation and
  the held sea, about 19 ms once the world has settled. Over budget, and every
  further saving is another sleep or wake rule, which is where the dams came
  from. Not recommended as the end state.

### C. The same model at 1 m cells (recommended)

One column per 4 x 4 terrain cells, still split into stacked spans, so caves,
tunnels and flooded rooms keep working. Plus evaporation, the held sea and a
settle pass at world creation, all prototyped.
- Expected cost: 16 times fewer columns; the substep limit grows with cell size,
  so a slice needs one substep instead of 1.5. The settled world would step
  about 35,000 spans: roughly 1 ms of solver per slice by the measured rate.
  **Estimate, not measured.** Settling at creation becomes about 16 times
  cheaper too.
- Rivers, floods, canals and dams stay fully simulated; the far throttle stays
  as the safety valve.
- Risks:
  - Walls thinner than 1 m: a coarse column's pipe to its neighbour must be
    closed when the voxel faces between them are walled, or a one-voxel wall
    leaks.
  - Shorelines and canal edges render blockier unless the client draws the
    surface against the voxels it holds (it already has the terrain).
  - Terrain edits must update coarse spans from 16 voxel columns.
  - A canal narrower than 1 m carries nothing (accepted, see Decisions).

### D. Hybrid: stored rivers, levels for lakes, live solver only where disturbed

Rivers kept as a stored flow, lakes as one level each, the pipe solver only near
players and edits.
- Measured: the storing half does not work with the current steadiness test
  (finding 4); a different test (flows averaged over minutes) is untried. Lakes
  as levels and the held sea do work.
- Keeps most of C's risk plus seams between stored and live water. Not
  recommended now; revisit if C's budget falls short.

### E. Pinned rivers with a mocked current (ruled out: rivers must be dammable)

No springs; river columns held at their planned level; the current comes from
worldgen's river directions; canals fill from a river as from a source.
- Measured floor: with springs removed, 0 active regions and 0.6 ms per step.
- Loses: damming a river (no flow backs up), draining or diverting a river (it
  refills), and exact volume (rivers become sources and sinks).

## Recommendation

Adopt C. Keep from the experiments:
- PR #226 (dam fix, slow-motion far springs), merged as is.
- Evaporation above sea level and the held sea, turned into real features:
  persisted, replicated, tested.
- A settle pass at world creation, saved with the world, so play starts at rest.

Drop the steady-freeze mode (finding 4) and the speed-based quiet test (it left
a 3 m pool asleep 1.4 cm out of level).

## Prototype plan for C, with go / no-go

1. Coarse spans from voxels: merge 4 x 4 voxel columns into one column's spans,
   with each pipe open or closed from the voxel faces between the two columns.
   Test: a one-voxel wall holds water; a canal 1 m wide flows.
2. Run the probe's settle-then-play on the 1 km world at 1 m cells. **Go if**
   solver time stays under 4 ms per slice on all cores (about 8 ms on the
   worker's half) and no unsupported walls form; otherwise tighten the far
   throttle and live area before going further.
3. Cut a canal from a river into a dry valley, and break a dam: check flow,
   fill, settling and exact volume.
4. Look at it in the game (vsync) before replacing the 0.25 m path.

## Decisions (user, 2026-10-23)

1. **Evaporation is accepted.** Water volume is no longer exact; a lake with no
   river drains slowly. The game already has a rain visual; rain adding water
   (the counterpart to evaporation) is later work.
2. **A canal narrower than a cell does not need to carry water.** At 1 m cells a
   canal must be at least about 1 m wide to flow. Thin walls must still hold
   water, so a coarse pipe needs only an open or closed rule from the voxel
   faces between two coarse columns, not partial openness.
3. **Damming rivers is a must-have.** Option E (pinned rivers) cannot be the
   fallback, because a pinned river cannot back up behind a dam. If C misses the
   budget, the next lever is the far throttle and a smaller live area, not E.

## Prototype result (2026-10-23): go

`examples/water_coarse_probe.rs` (1 km natural seed 1; `ColumnConfig::column_cells
= 4`, `CoarseVolumeTerrain`: a coarse cell is solid if any terrain cell in it is;
evaporation, held sea, springs and drains mapped to coarse columns; all cores):

| | 0.25 m (today) | 1 m prototype |
|---|---|---|
| Regions / spans allocated | 3,035 / about 3.4 M | 495 / 812 k |
| Settling | 3,300 s of water, 1.9 h wall | 1,600 s of water, 9.4 min wall |
| Walls left after settling | 2 | 3 |
| Play, standing: active regions (region = 32 x 32 columns) | 754-817 | 162-172 |
| Play: spans stepped per slice | about 570 k at 1.5 substeps | about 107 k at 1 substep |
| Play: solver ms per slice | 16-17 | 2.4-2.8 |
| Play: worst single slice | not recorded | 10-31 ms |

The bar (under 4 ms on all cores, no walls forming) is met on cost; 3 walls
remain where the fine world kept 2, and none appeared during play. Tests
(`crates/spall_fluid/tests/column_coarse.rs`): a wall one terrain cell thick
holds water, a 1 m canal carries it, spring volumes count in terrain cells.

Not covered yet: the server driver, frames and the client surface at 1 m,
terrain edits refreshing coarse spans, persistence, timing on half the cores,
and how 1 m shorelines look. Seeding drops 2% of the planned volume (9,463
coarse columns have no open span at the planned surface, because one solid
terrain cell closes the coarse cell). Evaporation outran the springs during
play (far springs run in slow motion, evaporation does not), so it should slow
down with its region too. The cost per span (about 24 ns) did not change; the
saving is fewer spans and one substep.

## Wired into the server (2026-10-23, `SPALL_WATER_COARSE=1`)

The server water driver runs 1 m columns behind `SPALL_WATER_COARSE=1` (read by
the generated-world setup): the coarse terrain reader, seeding from the plan,
springs and drains mapped onto coarse cells as they run (`ColumnWorld::world_cell`),
terrain edits refreshing the water region they fall in, the sea held and
evaporation on. Clients are unchanged: each 1 m region is published as its 16
terrain-region frames, every water column repeated over its terrain columns. It
is not journalled; the server refuses `--save` with it. Tests: `column_water.rs`
`wide_columns` (a pool published at its level per terrain region, a spring adds
its rate, cutting a dam lets the pool through).

Through the real driver (probe, settle then 900 s standing, all cores): the water
matches the standalone prototype exactly (settled at 1,600 s, 162-172 active
regions); the whole water tick is 4.2-6.6 ms per step (solver 2.7-4.9), against
about 19 ms at 0.25 m. Building the 16 frames from one read of the water region
took the frame cost from about 5 ms to about 1.6 ms. Not yet played in the game.

