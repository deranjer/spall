# Spall overengineering audit (2026-10-20)

Scope: every workspace package on `release/remaining-stack` @ `4d8ee6d1`. Line
counts include inline `#[cfg(test)]` modules unless stated. This is a review of
*accidental* complexity. Full-world destruction and multiplayer are user
requirements, so the complexity they force (server authority, brick revisions,
split-to-body transfer, late join/repair, durable journal) is treated as
essential and kept out of the cut list.

Verdict scale: **Proportionate** (leave it alone) · **Heavy** (trim at the edges)
· **Overbuilt** (worth a deliberate simplification pass).

## Headline numbers

| Metric | Value |
| --- | --- |
| Rust in `crates/` + `examples/` + `tools/` | ~283k lines |
| Lines under `tests/` directories alone | ~47k; 1,813 `#[test]` fns |
| `*_review.rs` files (review findings encoded as tests) | 17 files, ~7.2k lines |
| Independent schema/version constants | 47 (e.g. `PROTOCOL_VERSION 13`, `CREATURE_OWNER_SCHEMA 5`, six `BASELINE_*_WORLD_VERSION`s) |
| `pub const MAX_*` limits in engine crates | 129 |
| BLAKE3 hash-domain strings (`b"spall.…"`) | 26 |
| `BTreeMap/BTreeSet` vs `HashMap/HashSet` uses | 858 vs 311 |
| Creature/animal code | ~24k lines, but live animals are capsules that players walk through |
| Top-level design docs | ~9.8k lines; `tasks.md` 2.2k, `validation.md` 1.9k, `protocol.md` 1.0k |
| Files > 4k lines | `physics/world.rs` 14.2k, `server/serve.rs` 8.8k, `client/window.rs` 8.2k, `sim/sim.rs` 5.7k, `client/replica.rs` 4.4k |

## Cross-cutting patterns

These matter more than any single crate. Most crate findings below are
instances of them.

### 1. Lockstep-grade exactness in a server-authoritative engine

The architecture says physics is *not* lockstep and the server is the only
authority. Even so, much of the code is held to the standard a lockstep engine
needs:

- A canonical whole-world BLAKE3 topology hash (`spall_protocol::canonical`,
  `TOPOLOGY_HASH_VERSION 3`), plus chunk-stamp caches (`spall_voxel::chunk_store`)
  and an evicted-brick digest ledger (`spall_voxel::logical`, `EvictedBricks`)
  that exist mainly so the hash stays exact when bricks leave memory.
- "Bit-identical on every machine" integer math in structural strength
  (`spall_structure::strength`: `u64`/`u128`, canonical `(z,y,x)` tie-breaks).
- Canonical sort orders everywhere (858 BTree uses), including places that are
  never hashed.
- The client replica validates every transaction against `before` revisions
  *and* `result_hashes`, building a full candidate before publishing (136 hash
  references in `replica.rs`).

**Why it is too strict:** only the server computes structure, strength and
splits. Clients need *same revision, same bytes* for the bricks a transaction
touches. That is per-brick revision + content-hash checking, which is cheap and
worth keeping. They do not need a world-scale hash that stays identical across
cache placement, and the server does not need cross-machine bit-identity for
an algorithm only it runs. Same-binary determinism is enough for replay tests.

**Change:** keep per-brick `(revision, hash)` validation as the contract. Make
the whole-world hash a debug/test assertion, not a per-commit production
invariant, and retire `EvictedBricks`/logical digests once nothing in
production needs it. Drop the cross-machine bit-identity requirement from
`structural-strength.md` and let strength use `f32`/`f64` with a tolerance
where that is simpler. Use `HashMap` unless the order is observable.

### 2. Versioning and compatibility before anything has shipped

There are 47 independent version constants, six baseline-world encodings live
side by side, legacy decode paths (`initialize_legacy_weather`, "legacy
world_hash"), and a frozen "never edit this file" appearance table
(`sandbox/src/appearance_v3.rs`) protected by a manifest hash. No public build,
no player saves and no third-party clients exist yet.

**Change:** until the first external release, use **one** `PROTOCOL_VERSION`
(client and server ship together) and **one** `STORE_SCHEMA_VERSION` (an old
save fails with a clear "incompatible save" error). Reset per-record schema
numbers, delete legacy paths, and unfreeze `appearance_v3.rs`. Bring
fine-grained versioning back at the first public release, when real saves need
protecting.

### 3. Defensive bounds on every decoder, including trusted ones

There are 129 `MAX_*` constants, and every decode path preflights sizes before
allocating (`baseline_decode.rs`, `creature_owner_decode.rs` at 1.6k lines,
`spall_asset` with seven caps). Bounding **untrusted ingress** is right:
client→server records and assets from disk. The same rigor is also applied to
server→client payloads, to the server's own save DB, and to internal handoffs
between crates.

**Change:** keep strict bounds on client→server records and asset import. For
server→client and own-DB reads, a few coarse caps (total frame or blob size)
are enough. Put the surviving limits in `spall_protocol::limits` rather than
spreading them per module.

### 4. Gate and test scaffolding compiled into production crates

- `spall_server::Scene` is a list of gate fixtures (`BridgeCut`,
  `CheckerboardSplit`, `BulkSplit`, `G4Workload`, `SeparatedRegionsFar`, …).
- `ServeConfig` has 36 fields: `max_ticks`, `quiescence_ticks`,
  `drain_timeout`, `save_faults`, `dev_unvalidated_actions`,
  `fingerprint_out`, `timing_window`, and more. The production server is
  shaped like a bounded test run.
- About 4.4k lines of `fixtures.rs` sit in library crates (voxel, sim, render,
  mesh, physics). There are also 15 benchmark `src/bin` targets in engine
  crates, `spall_net::{proxy,fault,harness}` and `spall_store::fault`
  compiled unconditionally, and `spall_render::capture` at 2.3k lines.
- `sandbox-server` has 59 CLI args, `sandbox-client` 39, and
  `sandbox-capture` is 3.3k lines.

**Change:** move gate scenes and fixtures behind a `testkit`/`fixtures`
feature or into a `spall_testkit` crate. Split `ServeConfig` into a small
`ServerConfig` and a `HarnessConfig` that xtask owns. Gate the fault-injection
modules behind `test-util`. Move bench bins to `benches/` or xtask.

### 5. Machinery built ahead of the gameplay that needs it

| Built | Live use |
| --- | --- |
| `PhysicsRegionSet` + `spall_sim::RegionCoordinator` (multi-region rebased physics, ~620 lines + 4 bench bins) | Only bench binaries use them |
| `spall_voxel::{RegionLayout, ActiveRegions}` | Only bench binaries use them |
| Articulated-creature corpse transfer and fracture (`transfer_articulated_to_dynamic_corpse`, `commit_dynamic_corpse_fracture`, ~1k non-test lines of `physics/world.rs`) | Only `animal_collision_probe` calls them; live animals use capsule locomotion and players pass through them |
| Creature-owner protocol v5 with exact source-cell masks (`creature_owner.rs` + decode ≈ 4.4k lines) | The server refuses to save worlds that contain animals |
| Dormant/external body sets, giant-split bulk baselines (`BulkSplit`), deferred regional worlds | Needed eventually, but each carries its own baseline version and test suite now |

**Change:** freeze, don't extend. Move the unused region-physics code to a
branch or behind a feature until G5 needs it. For animals, put the
`creature_owner` exact-mask layer and corpse fracture on hold: replicate
`(id, species, pose, clip, time)` and add persistence when save-with-animals is
actually scheduled. That code is easy to recover from git history.

### 6. Several overlapping concurrency layers

The engine has rayon (a **vendored** crates.io copy, plus vendored
crossbeam), the `spall_jobs` `Scheduler` (lanes, priorities, budgets,
`JobToken` read sets, generations, topology epochs) with a `ThreadJobPool`,
about 30 ad-hoc `std::thread` spawns, and tokio in net, server and client.
`ThreadJobPool` has one consumer (`spall_sim::schedule`). On top of that, edit
staging uses optimistic concurrency: it stages off-tick, re-validates the
token, retries, and falls back to a serial queue when a region keeps losing.

**Change:** use rayon from crates.io; the vendoring reason (lint and feature
leakage) is solved by `exclude` plus not vendoring at all. Measure staging cost
for typical brushes. If it is sub-millisecond, stage edits inline on the tick
thread and drop the stale-retry/serial-queue path. Keep `JobToken` validation
for genuinely async work (meshing, collider builds). Then decide whether
`spall_jobs` should be ~300 lines (a token plus a bounded queue) instead of
2.5k.

### 7. God files

`physics/world.rs` (14.2k, of which ~5k is tests), `server/serve.rs` (8.8k),
`client/window.rs` (8.2k), `sim/sim.rs` (5.7k) and `client/replica.rs` (4.4k).
These are the files every feature touches, which makes agent merges
conflict-prone. The ENG-30 doc-merge damage came from the same pattern.

**Change:** mechanical splits with no behavior change. `world.rs` →
`rigid.rs` / `character_sweep.rs` / `articulated_world.rs` / `contacts.rs`.
`serve.rs` → `serve/{scene,join,tick_loop,replication_out,shutdown}.rs`.
`window.rs` → `window/{input,hud,frame,residency_upload}.rs`.

### 8. Process and documentation weight

- `validation.md` and `architecture.md` have turned into change logs: dated
  ENG-122 water sections make up hundreds of lines, and roughly 700 lines of
  `validation.md` cover water-solver comparisons from before the ENG-169
  rewrite.
- **README is stale:** it still describes "adaptive coarse MAC water …
  multigrid pressure" as the default. `spall_fluid` says that solver was
  removed in ENG-169. The rayon row in `docs/dependencies.md` also still
  describes MAC loops.
- 886 tracked files under `docs/reports` (JSONL traces, logs, test outputs),
  194 MB on disk, plus an untracked 251 MB `artifacts/`.
- The process rules (G0–G5 gates, no reduced workloads under the original
  name, exact evidence) suit research-risk gates. They have also been applied
  to features like weather audio and snow footprints.

**Change:** make `architecture.md` and `validation.md` describe the current
state only and move dated history into `docs/reports/`. Keep raw evidence
(`.jsonl`, `.log`, captures) out of git, or under one ignored `evidence/`
root with a short summary report committed. Reserve formal gates for the real
risk areas (destruction plus replication, lighting, persistence) and treat
content features as ordinary tickets.

---

## Per-system reports

### spall_core: foundation types

- **What:** coordinates (`GlobalCell`, `BrickCoord`), stable IDs and
  allocators, the material manifest, cell-size and brush units, quantized
  pose, `PlayerInput`, and the T00 JSONL process log.
- **Packages:** serde, serde_json, thiserror. **Size:** 2.1k.
- **Verdict:** Proportionate.
- **Change:** `CreatureAllocator`/`CreatureKey` with world epochs is game-ish
  identity living in core; it belongs with the creature code in sim. The JSONL
  readiness log is harness code and could move to xtask/server.

### spall_voxel: sparse brick storage

- **What:** 32³ bricks (uniform or dense, refcounted copy-on-write),
  `Volume`, transactional `EditPlan`, DDA ray queries, brushes, the
  residency/collision-readiness cache, chunked copy-on-write store with change
  stamps, the logical/evicted digest ledger, and region layout.
- **Packages:** blake3, glam, thiserror. **Size:** 7.0k (1.65k of it
  `fixtures.rs`).
- **Verdict:** Heavy at the edges, solid core.
- **Change:** the core (brick, volume, edit, query, brush) is right-sized. The
  stamped chunk store and `EvictedBricks` exist to keep the world hash exact
  (pattern 1), so revisit them if that hash is demoted. `RegionLayout` and
  `ActiveRegions` are only used by benches (pattern 5). Move `fixtures.rs`
  behind a feature. The "absent ≠ failed ≠ empty ≠ filled" sampling
  distinction is correct for streaming and should stay.

### spall_structure: connectivity and strength

- **What:** six-face connectivity to the anchor plane, component labeling,
  split planning, and a material-strength load-path model (T22).
- **Packages:** spall_core/jobs/voxel, thiserror. **Size:** 5.1k (1k of it
  scenario tests).
- **Verdict:** Proportionate for connectivity; strength is over-specified.
- **Change:** connectivity is the heart of full-world destruction, so keep it.
  For strength, drop the cross-machine bit-identity and integer-only rule
  (only the server runs it) and the "frozen `strength_algo_version`" ceremony
  until saves must survive it.

### spall_physics: Rapier adapter

- **What:** a fixed-step Rapier world behind opaque `BodyId`s; per-brick
  terrain colliders; character sweep and step; query cache; mass properties;
  occupancy grids; articulated creature bodies; corpse aggregate, fracture and
  restore; multi-region rebased world set; reports and metrics.
- **Packages:** rapier3d, spall_core/voxel, thiserror. **Size:** 27.4k
  (`world.rs` 14.2k; 6 bench bins; ~10 review/oracle test files).
- **Verdict:** Overbuilt, mostly through the animal and region layers.
- **Change:** split `world.rs` (pattern 7). Move corpse transfer and fracture
  plus `PhysicsRegionSet` behind features or a branch until gameplay calls
  them (pattern 5). The memory notes on ENG-69 (contacts starved by
  `sync_queries`, prediction under-ticking) show that the basic
  rigid/character loop still needs attention. That is where physics effort
  pays off.

### spall_fluid: column water

- **What:** a stacked-column virtual-pipe water solver plus voxel-boundary
  snapshots (MAC/VOF was removed in ENG-169).
- **Packages:** vendored rayon 1.12, spall_core/voxel, thiserror. **Size:**
  5.4k.
- **Verdict:** Proportionate after the rewrite. This crate already went
  through the simplification this report recommends elsewhere.
- **Change:** depend on crates.io rayon and delete `vendor/parallel`. Fix the
  README and `dependencies.md` text that still describes MAC (pattern 8). The
  `unsafe` shared-pointer helper near `columns.rs:3177` should get a comment
  or a safe chunked-slice rewrite.

### spall_worldgen: terrain generation

- **What:** column maps, natural landforms, hydrology, caves, noise, a
  gallery world, and debug layers.
- **Packages:** spall_core/voxel, thiserror. **Size:** 6.3k.
- **Verdict:** Proportionate.
- **Change:** `GEN_VERSION`/`NATURAL_GEN_VERSION 6` only matter once seeds must
  reproduce old saves. Fold them into the single save version (pattern 2).
  README notes that "the authoritative server still generates the whole
  world." That cost is real, but it is a performance item, not
  overengineering.

### spall_ecology: vegetation and living plants

- **What:** a deterministic bounded plan of grass and voxel trees, ground
  cover, a living-plant lifecycle, placement, and a postcard codec.
- **Packages:** blake3, postcard, serde, spall_core/voxel/worldgen.
  **Size:** 5.7k.
- **Verdict:** Heavy.
- **Change:** it carries three version constants and many caps
  (`MAX_PATCHES` and similar) for a feature that is visual flavor today. Merge
  versions into the save version and keep only the caps that protect memory.

### spall_asset: SPVX reader

- **What:** a bounded CPU-only reader for `.spvox` assemblies, parts,
  animations and transforms.
- **Packages:** blake3, glam, thiserror, zstd. **Size:** 1.5k.
- **Verdict:** Proportionate. Assets are untrusted input, so the bounds are
  justified.

### spall_jobs: background scheduling

- **What:** lane-budgeted priority scheduler, `JobToken` read-dependency
  validation, generations and topology epochs, shutdown drain, and a thin
  `std::thread` pool.
- **Packages:** spall_core, thiserror. **Size:** 2.5k.
- **Verdict:** Overbuilt for its usage. The token types are used in four
  crates, but `Scheduler` and `ThreadJobPool` are used only by sim staging.
- **Change:** see pattern 6. Keep `JobToken`/`Staleness` and a bounded queue,
  and replace lanes, priorities, budgets and pressure with rayon plus a
  channel unless a measured starvation problem exists.

### spall_mesh: meshing

- **What:** culled and greedy meshing, AO, LOD, face sampling, a reference
  mesher feature, and an LOD seam bench.
- **Packages:** blake3, glam, spall_core/jobs/voxel, thiserror. **Size:**
  2.9k.
- **Verdict:** Proportionate.
- **Change:** none needed; move `fixtures.rs` behind a feature.

### spall_render: wgpu renderer

- **What:** cascaded shadows, HDR opaque and tonemap passes, voxel-DDA
  indirect/bounce, sky visibility, occlusion culling, water, snow, weather,
  skybox, and capture/report tooling. 13 WGSL shaders (~2.1k lines).
- **Packages:** wgpu 30 (pinned `=`), bytemuck, glam, image, pollster,
  spall_core/mesh, thiserror. **Size:** 20.1k.
- **Verdict:** Proportionate core; heavy evidence tooling.
- **Change:** `capture.rs` (2.3k: collapse, motion, lighting and
  frame-series reports with flicker and settle indices) is gate-evidence
  machinery. Move it to a `capture` feature or the sandbox-capture binary.
  `examples/vulkan_shadow_probe.rs` (1.4k) is a finished probe and can be
  deleted. The lighting ambition is a user goal, so the shader passes stay.

### spall_protocol: DTOs and codecs

- **What:** versioned records and framing, canonical hashing, baselines (six
  world encodings), segments, handshake, session, input, water, snow, weather,
  vegetation, transaction attachments, lifecycle bulk, and the whole creature
  family (owner, owner decode, snapshot, sync, motion, catalog).
- **Packages:** blake3, postcard, serde, spall_core, thiserror, zstd.
  **Size:** 17.2k, of which creature code is ≈ 7k plus 3.9k of creature
  review tests.
- **Verdict:** **Overbuilt.** This is the biggest concentration of patterns 1–3.
- **Change:**
  1. Collapse schema constants into `PROTOCOL_VERSION`.
  2. Reduce baseline world encodings to the one or two the client actually
     accepts. `client/net.rs:1352` accepts only segmented or streamed.
  3. Shelve `creature_owner*` (pattern 5).
  4. Keep bounds on client→server records only (pattern 3).
  5. Cut hash domains to the few that are persisted or compared across the
     wire.

### spall_net: QUIC transport

- **What:** Quinn/rustls connection with a pinned self-signed certificate and
  join token, a control stream plus bulk streams and datagrams, heartbeat,
  dedup gates, framing, an application-level fault injector, an opaque UDP
  loss proxy, and a process harness.
- **Packages:** quinn (with a git patch for quinn-proto),
  rustls/ring/rcgen, socket2 `=0.6.5`, tokio, postcard, blake3, tracing.
  **Size:** 5.6k.
- **Verdict:** Proportionate for real multiplayer.
- **Change:** gate `proxy`/`fault`/`harness` behind `test-util`. Drop the
  quinn-proto `[patch]` as soon as a release ships the fix (the existing
  comment says so). Check periodically, since this patch also triggers the
  sandbox approval friction.

### spall_store: durable save

- **What:** SQLite WAL with `synchronous=FULL`, checkpoint plus CRC journal,
  recovery with previous-checkpoint fallback, crash-point and disk-fault
  injection, a non-authoritative residency brick cache, and a creature
  checkpoint.
- **Packages:** rusqlite (bundled), zstd, blake3, postcard, serde.
  **Size:** 4.6k.
- **Verdict:** Proportionate. This is the right amount of rigor for a
  persistent destructible world.
- **Change:** `synchronous=NORMAL` under WAL is the usual game choice (it can
  lose the last commit on power loss, never corrupt). Consider it if
  checkpoint latency matters. Gate `fault`/`inject` behind a feature. Pattern 2
  applies to `STORE_SCHEMA_VERSION 5`.

### spall_sim: authoritative simulation

- **What:** the edit intent → stage → commit pipeline, split-to-body
  transfer, journal, registry, collider planning, player, dormancy,
  containment, contact damage, buoyancy, column water integration, snow and
  footprints, vegetation, the creature lifecycle (capture, restore, motion,
  owner bridge, bulk publication), the region coordinator, scenario fixtures,
  and profiling.
- **Packages:** glam, vendored rayon, postcard, serde, serde_json (opt), and
  most engine crates. **Size:** 36.9k, the largest crate.
- **Verdict:** **Overbuilt**, mostly from scope creep rather than bad code.
- **Change:**
  1. The pipeline (stage → validate → commit, atomic publish) is essential.
     Keep it, but test whether inline staging removes the retry/serial-queue
     path (pattern 6).
  2. Creature lifecycle code (~5k) is ahead of gameplay (pattern 5).
  3. Snow, footprints, vegetation and weather init are game-flavor systems in
     the engine's authoritative core. Consider a small "world systems" trait
     owned by the sandbox, which README already anticipates ("add a small
     statically linked rules interface when real game behavior first
     requires it").
  4. Delete `RegionCoordinator` until G5.
  5. Split `sim.rs`.

### spall_server: networked host

- **What:** QUIC endpoint plus a sim thread bridge, admission and throttling,
  input scheduling and pacing, per-client replication and interest, baseline
  and late join, column-water replication, residency pass and disk backing,
  the persist pipeline, the creature service, commit latency, and memory
  stats.
- **Packages:** tokio, glam, serde/serde_json, tracing, and engine crates.
  **Size:** 29.0k (`serve.rs` 8.8k).
- **Verdict:** Heavy.
- **Change:** pattern 4 is most visible here: the `Scene` gate-fixture enum,
  36-field `ServeConfig`, bounded-run semantics (`max_ticks`, quiescence,
  drain) and `dev_unvalidated_actions` all sit in the main server. Split them
  into a lean server API plus an xtask-owned harness, and split `serve.rs`
  (pattern 7).

### spall_client: game client host

- **What:** the winit window and frame loop, HUD (yakui), replica world with
  candidate validation, net session, player prediction and reconciliation,
  residency, column water surface, sky, day/night, weather, snow, audio
  (rodio), creature replica, and an ecology demo.
- **Packages:** wgpu, winit, yakui (git rev), rodio, tokio, image, glam,
  bytemuck, pollster, serde/serde_json, and engine crates including sim,
  physics, fluid and ecology. **Size:** 29.1k (`window.rs` 8.2k).
- **Verdict:** Heavy.
- **Change:**
  1. Split `window.rs`.
  2. `ecology_demo.rs` (1k) is demo code inside the engine client; move it to
     the sandbox.
  3. Replica: keep per-brick revision and hash checks, and drop world-hash
     comparisons (pattern 1).
  4. Prediction: memory shows reconcile mismatches were caused by a tick bug,
     not by too little machinery. Resist adding more reconciliation layers.

### sandbox (examples/sandbox): shipped game example

- **What:** materials and content, game rules, progression store (SQLite),
  vegetation, animals and habitat, editor/worldgen/ecology scenes, appearance
  palettes, audio, server/client/capture binaries, and 13 probe/gallery
  examples.
- **Packages:** clap, ron, rusqlite, postcard, serde, tracing, zstd, blake3,
  glam, and nearly every engine crate. **Size:** 26.6k.
- **Verdict:** Heavy.
- **Change:**
  1. Unfreeze `appearance_v3.rs` (pattern 2).
  2. `sandbox-server` (59 args) and `sandbox-capture` (3.3k lines) mirror the
     `ServeConfig` sprawl; trim them alongside it.
  3. Delete finished probes (`animal_collision_probe` 2.6k,
     `water_cut_probe`, `river_idle_probe`, `water_where`,
     `water_tick_cost_probe`) or move them to an `experiments/` folder that is
     not built by default.

### poc_local

- **What:** the original wgpu/winit proof of concept. **Size:** 1.2k.
- **Change:** delete it, since it is superseded by `spall_render` and the
  client.

### spall_editor (tools/)

- **What:** a yakui editor for RON scenes, `.spvox` assets, a worldgen panel,
  and thumbnails. **Packages:** yakui, wgpu, winit, rfd, ron, zstd.
  **Size:** 11.8k.
- **Verdict:** Proportionate. It is correctly a leaf package.

### xtask (tools/)

- **What:** check, smoke, play, netcheck, G4 gate runner, and process
  supervision. **Packages:** clap, ctrlc, serde_json, tokio.
  **Size:** 7.0k (`session.rs` 3.8k, `g4.rs` 1.5k).
- **Verdict:** Proportionate as a harness.
- **Change:** this is the right home for the harness config, scenes and
  bounded-run logic pulled out of the server (pattern 4). Retire `g4.rs` once
  that gate is closed.

---

## Recommended order (best payoff per risk)

1. **Fix the stale docs** (README water section, `dependencies.md` rayon row)
   and stop committing raw evidence. No code risk.
2. **Unvendor rayon** and delete `poc_local` and finished probes. Mechanical.
3. **Collapse versioning** to one protocol version and one save version, and
   unfreeze `appearance_v3`. Removes a lot of ceremony from every future
   change.
4. **Split the god files** (`world.rs`, `serve.rs`, `window.rs`, `sim.rs`) with
   no behavior change. Fewer agent merge conflicts.
5. **Feature-gate test scaffolding**: fixtures, fault injection, net proxy,
   render capture, bench bins. Split `ServeConfig` into server and harness
   configs.
6. **Shelve speculative layers**: region physics and coordinator, corpse
   fracture, creature-owner exact masks. Replace with a minimal creature
   replication record.
7. **Demote the whole-world topology hash** to a debug assertion. Then
   reconsider `EvictedBricks`, chunk stamps, and bit-identical strength math.
8. **Measure inline edit staging.** If it is cheap, remove the optimistic
   retry and serial-queue path and shrink `spall_jobs`.

Items 1–5 change no behavior. Items 6–8 change contracts in `protocol.md`,
`architecture.md` and `structural-strength.md`, so per `AGENTS.md` each needs
the docs and tickets updated together. They are for an explicit decision, not
a drive-by cleanup.

## Not overengineered (keep)

- Server authority, per-brick revisions, atomic tick-boundary commit, and
  split-to-body with inherited mass and velocity.
- Absent/failed/empty/filled sampling distinctions for streaming.
- Durable SQLite WAL journal and recovery.
- QUIC streams plus datagrams with cert pinning.
- Rapier behind a narrow adapter.
- Bounds on client→server and asset ingress.
- Editor as a leaf package.
- The ENG-169 water simplification, which is the model for the rest of this
  list.
