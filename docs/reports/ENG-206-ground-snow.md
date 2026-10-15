# ENG-206 / WX3 ground snow

## Contract

Snow is an authoritative, non-colliding coating on exposed +Y faces of static
QuarterMetre terrain. Tile layers are 4 m wide with disjoint 256-face cohorts,
actual terrain brick revisions, stable surface IDs, exact Q32 depth and a Q16
renderer snapshot. New exposure starts at its topology birth tick; retained
faces keep their accumulated climate history. Unknown geometry defers the whole
tile and retries with backoff. No client-interest or viewer position controls
authoritative accumulation.

The frozen clock remains the weather clock selected for the world. Snowfall is
2 mm/s times precipitation for Snow or Blizzard at temperature <= 0 C. Melt is
1 mm/s times `clamp(temp / 20 C, 0, 1)`. Depth is clamped at 8192 Q16 units
(0.125 m); Q32 fractions and composable clamped climate spans preserve exact
continuation across batching and restart. Known snow work catches up a fair
16 tile layers per simulation tick. Geometry discovery processes at most 16
tile queries per tick, prioritizes every live player's center, and refreshes a
world-clamped 96 m radius once per second. Each query is capped at 128 vertical
bricks and skips uniform empty/full bricks. Regional snapshot fanout sends at
most four full replacement frames per client every six ticks; empty tile frames
are retained as tombstones. Client residency is bounded to 4096 frames / 16 MiB.
The 96 m query ring is cached and interleaved across players, then prioritized
ahead of the distant resident seed; it is rebuilt only when a player changes
tile or on the one-second refresh. Snapshot delivery alternates a four-frame
player-center page with a four-frame regional page, retaining bounded outer
coverage without lexical-center starvation. A cursor outside a moved client's
new interest restarts safely at that interest's first key.

Footprints are transient, server-validated terrain contacts. They require at
least 2 mm of snow, are capped at 64 per layer and 4096 globally, and expire at
18000 ticks. New snow fades them even at the depth cap; warm melt fades them
and removes them when the supporting snow falls below 2 mm. Restart and reset
clear tracks.

## Changed files

- `crates/spall_sim/src/snow.rs`: durable sparse cohorts, fair discovery queue,
  retry/birth persistence, bounded snapshot pages, exposure reconciliation,
  footprint fill/melt/TTL/caps, and strict versioned canonical cohort checks.
- `crates/spall_sim/src/sim.rs`: guarded one-time resident seeding, bounded
  world-clamped nearby discovery, all-player center priority, topology birth
  handling, exact terrain/water/contact validation, and frame paging API; snow
  scans use the explicit terrain collider cell region for sparse unbounded
  worldgen volumes; an
  opt-in `SPALL_DEBUG_SNOW_STAMPS` diagnostic reports candidate rejection causes.
- `crates/spall_sim/src/snow_query.rs`: bounded sky-down query using either a
  bounded Volume extent or the explicit authoritative terrain cell region.
- `crates/spall_server/src/serve.rs`: bounded per-session snow interest pages,
  including non-regional clients.
- `crates/spall_server/src/persist.rs`: ordered exact snow tile replay and
  canonical checkpoint recovery.
- `crates/spall_client/src/replica.rs`: independent snow stream epoch/sequence
  fences, atomic invalidation, and bounded cache/LRU.
- `crates/spall_sim/src/lib.rs`: module export cleanup.

## Verification

- `cargo test -p spall_sim snow --lib`: **16 passed**, including policy/query,
  unknown retry and saved queue birth, page bound/cycle, cold/warm exactness,
  roof exposure, restart continuation, owner-key/overlap corruption rejection,
  tombstones, and footprint refill/melt.
- `cargo test -p spall_client delayed_weather_uses_its_own_stream_and_reset_fences_without_prior_weather --lib`: **1 passed**.
- `cargo test -p spall_client snow_replica_cache_is_bounded_and_future_epoch_fences_old_tiles --lib`: **1 passed**.
- `cargo check -p spall_server -p spall_client`: **passed**.
- `cargo test -p spall_sim snow_query --lib`: **6 passed**, including querying
  an unbounded sparse Volume only within an explicit terrain region.
- `cargo clippy -p spall_sim -p spall_server -p spall_client --all-targets -- -D warnings`: **passed**.
- `cargo fmt --all` and `git diff --check`: **passed**.
- Full `cargo xtask check`, the two-client snow replication scenario, 1 km live-world
  traversal, GPU capture, and matched performance measurements: **not run here**;
  coordinator owns combined gates and the two-client worker is rerunning against
  this queue/fanout update.

## Measured results and remaining risks

The focused test counts and successful builds above are measured. The 16-layer
per-tick, 96 m radius, four-frame/six-tick fanout, 4096-frame/16 MiB client cache,
and 1 ms/tick / 8 KiB/s targets are limits or targets, not measured 1 km costs.
No per-tick all-world brick enumeration remains; resident coordinates are
enumerated once, and nearby scans are clamped to the explicit world cell region.

If several geometry edits to an undiscovered tile queue before its query slot,
the pending entry keeps the newest birth tick and the latest authoritative mask.
This avoids retroactive snow on newly exposed faces, but may conservatively lose
some climate credit for unchanged faces whose first discovery was delayed. A
full 1 km topology-history and traversal measurement remains an integration gate.
