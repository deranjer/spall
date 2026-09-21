# Terrain-collider locality (proposal + default-off prototype) — 2026-09-20

Status: **prototype, not adopted.** ENG-30 stays open; T23 is not accepted. Compaction is a
separate decision and is not part of this proposal.

## Problem

The physics terrain is one whole-world compound collider. Every terrain edit replaces it, and Rapier
wakes everything resting on a replaced collider, so a dig far from a sleeping pile still wakes the
pile. On the integrated G4 workload 154 of 155 mass-wake bursts (107,428 of ~108k sleeping-body
wakes) coincide with a terrain-dig commit in the same tick ([G3.md](G3.md) increments 40-41). The
whole-terrain plan/extraction is also O(terrain) per commit.

## Design

`SimWorld::enable_terrain_brick_colliders()` (opt-in, idempotent, `crates/spall_sim/src/terrain_bricks.rs`)
replaces the single terrain collider by **one fixed collider per solid 32-cell brick** (8 m at the
quarter-metre cell), each built from that brick's exact occupancy. Only the *physics view* changes.

Preserved:

| Requirement | How |
| --- | --- |
| Authoritative geometry | Volume, hashes, journal and replication untouched. `validate_terrain_brick_colliders` checks that colliders cover exactly the volume's solid cells and that no empty brick keeps a collider. |
| Collision across region boundaries | A body straddling two bricks touches both colliders; merged-cuboid faces meet flush as adjacent boxes do inside the compound. Tested against the single-collider result with a slider across a brick boundary. |
| Revision validation | Each collider records the brick revision it was built from and is checked against the volume. |
| Tick-boundary publication | Plans are built during staging (fallible); publish is infallible and happens at the same point in the commit as the single-collider swap. |

A commit rebuilds only the bricks it changed, so only bodies touching those bricks wake.

## Evidence

- Distant dig: unrelated sleeping pile stays asleep (0 awake vs 64 with the single collider).
  Removing real support (dig under the pile) wakes it and it drops.
- Integrated workload, in-process, 9,500 ticks: dig-coincident wake bursts 154 to 19; sleeping-body
  wakes ~107k to ~28k.
- Cost, unchanged 3,000-tick workload, 3 alternating runs each, no graphical client: full-tick and
  physics **means within run-to-run noise** (~4.5 ms and ~1.1 ms); full-tick p99 ~56 to ~46 ms and
  `sim.commit` p99 ~42 to ~33 ms in 3 of 3 pairs. Average awake bodies is higher with bricks
  (693 vs 655), unexplained. No mean-cost claim is made.

## Not supported / open

- **Residency eviction of terrain bricks**: an evicted brick fails the commit as `Unresident`.
- **Server CLI wiring**: only the in-process API and test harness (`TERRAIN_BRICKS=1`) use it.
- **Brick-face seams**: tested for a sliding body; contact behaviour at concave seams (bowl wedges)
  is not separately characterised.
- **Broad-phase cost** of many fixed bodies at full world scale is only measured on the G4 scene.
- Long-soak behaviour: none run (30-minute soak awaits explicit go-ahead).

## Decision needed

Whether to adopt per-brick colliders as the server default after residency support and a longer
soak; whether to also address wakes from body-edit rebuilds and ordinary contact propagation
(~30% of wakes remain).

## Update 2026-09-21

- Coverage added: empty/refill, four-brick dig, cross-brick collapse, character seam walk, contact recognition, residency refusals (`terrain_brick_colliders.rs`). Experimental server wiring (`--terrain-brick-colliders`), refused with residency.
- The earlier higher `active_body_count` (693 vs 655) was a metric artefact: it counts 84 fixed brick bodies plus asleep bodies. Truly awake dynamic bodies fell 22% (507 to 394 per tick). With digs removed the modes are nearly identical, so seams do not prolong awake time.
- Networked A/B (2 alternating runs each, unchanged workload): tick p95 18.8/16.0 vs 14.7/15.8 ms, p99 63.8/68.8 vs 49.0/55.1 ms, physics p95 6.95/6.41 vs 6.15/6.08 ms; all hashes agree. n = 2, p95 overlaps: **no adoption claim**.
- Still open: residency support, longer soak, more repeats. See G3.md increment 42 item 3.
