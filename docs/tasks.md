# Implementation task queue

Every task is unstarted. Read architecture.md, protocol.md, and validation.md first. Assign one ID per implementation pass. Dependencies are required unless the task explicitly creates a test double. Test doubles must not count as integrated gate evidence.

Task size is a scope boundary, not a calendar estimate. If an assignment cannot fit in one reviewable change, split it into sequential changes with the same contract and acceptance fixture. Shared schema changes go through the integrator. An agent should not be asked to independently invent networking, physics, and structural semantics in the same assignment.

## Foundation

### T00 — Build and process harness

Dependencies: none. Own: root Cargo files, crates/spall_core, crates/spall_server and crates/spall_client runtime hosts, examples/sandbox executable entry points, tools/xtask, CI, docs/dependencies.md.

Pin compatible released dependencies and compiler; create one window with wgpu/winit, one GPU-free server process, clap flags, JSONL logging, and `cargo xtask check`. Add bounded startup/shutdown and process IDs/readiness records. Server and window may be empty at this stage. Implement check/smoke commands first; later scenario commands must explicitly say unavailable until delivered.

Ship package `sandbox` with thin `sandbox-server` / `sandbox-client` binaries calling engine host functions. Keep engine dependencies free of sandbox/game imports. Only install dependencies exercised by this foundation; full transport protocol is T09. If a minimal Quinn loopback smoke is included to verify compatibility, label it transport plumbing rather than multiplayer implementation.

Accept: clean build; headless server exits after 60 ticks; window resize and close work; `xtask smoke` terminates its children on success, timeout, and failure. No hidden daemon remains. Record platform/backend actually tested. G0 infrastructure complete.

### T01 — IDs, schemas, canonical encoding

Dependencies: T00. Own: spall_core, spall_protocol, small golden fixtures.

Implement coordinate/ID/material types, manifest validation, versioned DTOs, canonical hash encoding, message size limits, and the transaction/baseline model in protocol.md. Document exact brush centre/radius fixed-point units, pose quantization, endianness, tags, and sort orders. Choose u64 session/stream sequencing behavior and test stale sessions.

Accept: golden bytes round-trip; reordered input maps hash identically; invalid sizes/material IDs/non-finite poses reject without uncontrolled allocation; coordinate tests cover -33, -32, -1, 0, 31, 32. Consumers use these types, not copies.

### T02 — Brick storage and revisioned edits

Dependencies: T01. Own: spall_voxel storage.

Implement uniform/dense bricks, sparse volumes, fixed cell sizes, immutable snapshots, checked cell access, copy-on-write edits, and canonical authoritative layer hashing. Track missing versus empty explicitly. Add edit-plan before/after revision records and modified-air tombstones; no live server mutation yet.

Accept: randomized edits match a dense reference model; uniform transitions preserve data; snapshots remain unchanged; out-of-range inputs fail; accounting reports dense/snapshot memory separately.

### T03 — Fixture worlds, rays, and integer brushes

Dependencies: T02. Own: spall_voxel queries, fixture definitions.

Build deterministic small terrain/structure fixtures and DDA ray queries for terrain and transformed volumes. Implement box and integer sphere edit plans. Specify face normal and placement cell at boundaries, zero direction handling, start-inside behavior, and finite ray range. Determinism applies to integer brush results; hit validation stays server-side.

Accept: negative-coordinate and exact-boundary rays; cuts spanning eight bricks; rigid-transform local/world round-trips; fixed seeds/hashes for all foundation fixtures. No graphics dependency.

### T04 — Bounded job scheduling

Dependencies: T02. Own: spall_jobs.

Implement work priorities, byte/job limits, cancellation, immutable read-dependency tokens, result validation, and generation invalidation on world reload. Provide a deterministic scheduler mode for tests that controls completion order.

Accept: out-of-order completion cannot install stale results; unload/reload invalidates old jobs; saturation remains bounded and reports backpressure; all jobs finish/cancel on shutdown.

## Destruction and multiplayer proof

### T05 — Visible voxel baseline

Dependencies: T03, T04. Own: spall_mesh and basic spall_render.

Implement culled faces, then greedy meshing, material colors, brick/volume transforms, depth, free camera, and bounded GPU uploads. Add screenshot output and normals/depth debug captures. Define AO-aware merge rules before adding baked vertex AO.

Accept: cube, tunnel, checkerboard, negative coordinates, adjacent bricks, and a rotated hollow volume render correctly; mesh surface area matches a reference face emitter; stale halo changes remesh; capture exits automatically.

### T06 — Editable voxel collision feasibility

Dependencies: T03, T04. Own: spall_physics and benchmark fixtures.

Compare pinned Rapier voxel colliders against exact merged-cuboid compounds. Test static/dynamic concave solids, contact stability, mass properties, build/update cost, sleeping/waking, and fast objects. Deliver measurements and one selected adapter implementation. No production triangle-mesh workaround for dynamic hollow solids.

Accept: falling hollow building keeps accessible openings; 256 active connected pieces settle without numerical explosions; record p50/p95/p99 physics and rebuild costs plus memory. Test a 64-brick connected body. If targets fail, record the bottleneck and a concrete representation revision; do not declare feasibility passed.

### T07 — Support graph and split plans

Dependencies: T03, T04. Own: spall_structure.

Implement local component labels, cross-brick face graph, boundary anchors, deletion invalidation, incremental support queries, and canonical unsupported-component membership. All-resident G1 first, with an explicit Unknown result for unavailable dependencies. Add cancellation/revision validation.

Accept: sever a bridge across storage boundaries; remove every bottom-plane support; diagonal-only contact does not bond; disconnected solids in one brick stay separate; a giant connected remainder does not allocate one body per cell. Verify planned cell conservation against a dense BFS oracle.

### T08 — Authoritative edit and body transfer

Dependencies: T06, T07. Own: spall_sim and minimal sandbox_game tools.

Integrate accepted intents, staging, atomic commit, collider swap, mass/inertia/velocity transfer, and body IDs. Support cutting terrain and cutting a rotated moving body again. Use the scheduler to retry conflicts and serialize repeatedly contested regions. Include stub journal output using real DTOs; persistence comes in T16.

Accept: no duplicate/lost occupied cells except explicit removals; no stale authoritative collision after commit; split geometry stays in the same world location; no second impulse on retry; two conflicting cuts converge to the documented commit order.

### T09 — Transport and fault harness

Dependencies: T01, T00. Own: spall_net and xtask network harness.

Implement TLS-pinned local sessions, reliable control/bulk streams, motion/input datagrams, limits, readiness, heartbeat/timeouts, and deduplication plumbing. Add deterministic application-message delay/drop/reorder tests and a separate UDP proxy that drops/delays encrypted transport packets without decoding them. Both modes are necessary: application faults do not reproduce QUIC retransmission/congestion behavior.

Accept: server + two headless clients authenticate and exchange records; wrong manifest/token/certificate fails clearly; malformed length/decompression attacks remain bounded; actual transport packet loss recovers reliable records and cannot deadlock shutdown.

### T10 — Replica transactions and motion

Dependencies: T08, T09. Own: server/client replication integration.

Implement deterministic brush/run replication, server-selected split membership, transaction staging, revision/hash checks, tombstones, motion interpolation, and bounded snapshot waiting. Initial fixed-scene baseline can be installed before play; full live late join is T17.

Accept: topology hashes match after concurrent cuts under loss; a snapshot arriving before create is handled; duplicates do not repeat an edit; missing source revisions request repair; a split spanning multiple records never appears partly applied.

### T11 — G1 networked destruction gate

Dependencies: T05, T10. Own: integrated scenarios and gate report.

Run server + two graphical clients and the headless bot equivalent in the 64 x 32 x 64 m fixture. Cut a cross-brick tower, let it fall, cut it in motion, and concurrently excavate terrain. Repeat with packet loss and reordered snapshots. Add exact replay of committed topology events from the fixture baseline.

Accept: validation.md G1 checks pass, including collision and conservation; produce captures, logs, timing/memory/network data, and hashes. Reviewer checks that actual terrain and actual dynamic voxel bodies share the edit path. No game-content expansion before this gate passes.

## Graphics proof

### T12 — Material and direct-light pipeline

Dependencies: T05. Own: spall_render and shaders.

Implement linear HDR material shading, sun/sky, cascaded shadows, AO, fixed-exposure capture and tone mapping. Add debug views for albedo, normals, depth, shadow cascades, and roughness. Keep render passes explicit; a generic render-graph framework is unnecessary.

Accept: stable camera fixtures expose no inverted normals, seam holes, incorrect gamma, or missing moving-body shadows. Record GPU timings at 1080p on the named adapter.

### T13 — Indirect-light feasibility design and prototype

Dependencies: T12. Own: spall_render lighting prototype and `docs/lighting-decision.md`.

Have an experienced graphics integrator specify occupancy/radiance encoding, traversal, sampling, history resources, and pass dependencies. Implement the small 128-cubed lighting volume and one diffuse bounce prototype. Use a colored-room fixture with emissive and occluded regions. Measure upload, tracing, and denoising separately.

Accept: indirect illumination exists with direct contribution disabled; a closed room stays darker than an open one; thin-wall leakage and time cost are quantified. If quality/cost fails, revise the design before extending it. Do not pass using only AO/bloom.

### T14 — Dynamic lighting and temporal stability

Dependencies: T13, T08. Own: dirty lighting updates and temporal passes.

Update old/new bounds of moving volumes and edited terrain; invalidate affected lighting history. Implement reprojection, rejection, clamping, and denoising against the frozen T13 design. Add moving camera and rapid-destruction fixtures.

Accept: no indefinitely stale shadows, erased overlapping objects, or persistent light trails; log latency from edit commit to updated lighting. Compare moving footage and settled captures, not only a favorable still image.

### T15 — G2 graphics gate

Dependencies: T14, T11. Own: visual acceptance report.

Capture exterior, indoor colored-light, emissive, thin-wall, and active-collapse scenes. Report each pass timing and total frame percentiles with fixed settings. Evaluate both geometry detail and lighting; propose cell-size changes now if required.

Accept: validation.md G2 evidence reviewed; freeze terrain/detail sizes and renderer direction before shipping persistent world compatibility. Record quality shortfalls explicitly. No assertion of Teardown-equivalent quality without reviewed evidence.

Delivered in increments (ticket stays open until every G2 bullet has evidence
and a graphics integrator has reviewed it); `docs/reports/G2.md` collects the
evidence and open items.

- **Increment 1 (GPU frame-cost percentiles).** `spall_render::capture_frame_series`
  renders one scene for `warmup + measured` consecutive frames at a fixed size
  and exposure (`Shaded` view only) and reduces each render pass family's
  per-frame device time to nearest-rank percentiles.
  `sandbox-capture --scene g2-frames` (also `cargo xtask capture --scene g2-frames`)
  runs it over the still T13/T14 lighting fixtures at a fixed 1920x1080. Measured
  on the RTX 4080 SUPER / D3D12 reference adapter: frame-total GPU p50 ~12-13 ms,
  p95 ~32-38 ms -- the provisional GPU p95 <= 12 ms is missed ~3x, the full
  128^3 indirect trace dominant. This is the cold full-retrace cost, not the
  amortised client frame. Evidence: `spall_render`
  `capture::tests::frame_stats_*`; `sandbox-capture`
  `tests::g2_frame_scenes_are_distinct_lit_and_framed`; the GPU run is manual.
- **Increment 2 (persistent-resource settled-frame loop).**
  `spall_render::capture_frame_loop` builds every GPU resource once, then renders
  a static scene from a fixed camera for `warmup + measured` frames (warm-up:
  full re-trace; measured: a bounded `retrace_edge_cells` box or nothing, with
  temporal accumulation), reporting per-pass GPU device-time and per-frame CPU
  encode percentiles. Shadows are timed once. `sandbox-capture --scene g2-loop`
  (also `cargo xtask capture --scene g2-loop`) runs it over the still T13/T14
  fixtures in `settled` and `edit` (24^3 re-trace box) modes at a fixed
  1920x1080. Measured on the reference adapter: settled frame ~0.3-0.9 ms GPU /
  ~0.6-0.8 ms CPU, pipelined client-frame p95 estimate <= 2.9 ms in every
  scene/mode -- the provisional GPU/CPU/client p95 targets are all met with
  margin, and increment 1's ~3x miss is confirmed a harness artefact (per-frame
  pipeline rebuild + full-cache re-trace). Evidence: `spall_render`
  `capture::tests::a_zero_edge_retrace_box_has_no_volume` +
  `a_retrace_box_is_centred_and_clamped_to_the_cache`; the GPU run is manual.
- **Increment 3 (moving-frame sequences + quality flags).**
  `spall_render::capture_motion_sequence` drives a scene through a per-frame
  camera + lighting-update path on the increment-2 loop, sampling luminance in
  probe bands every frame; pure `flicker_index` / `max_step_fraction` /
  `settle_index` reduce the traces. `sandbox-capture --scene g2-motion` (also
  `cargo xtask capture --scene g2-motion`) runs three 120-frame sequences on the
  emitter/occluder fixture -- `static-noise` (Shaded, nothing moving),
  `moving-occluder` and `occluder-jump` (IndirectOnly, occluder leaves the light
  path and returns smoothly / in two jumps) -- and flags flicker / ghost
  residual / weak recovery "for review". Measured on the reference adapter: the
  settled indirect frame is bit-stable (flicker 0.00000 / 120 frames); the
  moving occluder's shadow recovers ~26% and returns within 0.8% (no ghost /
  trail); static regions stay quiet; smooth and discrete moves behave the same.
  **No quality flags.** Evidence: `spall_render`
  `capture::tests::{a_steady_trace_has_zero_flicker,
  flicker_index_is_mean_abs_step_over_mean_level,
  settle_index_finds_the_first_lasting_return_to_target}`; the GPU run is manual.
- **Increment 4 (open daylight-terrain scene).** `spall_render::daylight_terrain_scene`
  — the sixth G2 scene category: a sun-lit open exterior (stepped terraces + tall
  pillars casting long shadows, bright sky), built entirely in `spall_render`.
  `sandbox-capture --scene g2-terrain` (also `cargo xtask capture --scene
  g2-terrain`) runs it through the increment-2 loop (`settled` + `edit`) + a
  120-frame `static-noise` stability pass. Measured on the reference adapter:
  settled frame GPU p95 0.24 ms / CPU p95 0.68 ms / pipelined client p95 0.68 ms
  — the cheapest G2 scene (single sky/ground bounce), all provisional targets met
  with the widest margin; static-noise flicker 0.000000; cast shadows +
  direct-light falloff read correctly. **No quality flags.** Completes the G2
  scene matrix except a GI-lit rapid-destruction sequence. Evidence:
  `spall_render` `fixtures::tests::daylight_terrain_is_open_lit_and_shadow_casting`;
  the GPU run is manual.
- **Increment 5 (GI-lit rapid-destruction sequence).** `sandbox-capture --scene
  g2-collapse` (also `cargo xtask capture --scene g2-collapse`) drives the
  authoritative `spall_sim` world on `cross_brick_bridge_scene` through the
  `g1-networked-destruction` cut script; at 13 ticks across the 200-tick collapse
  it meshes the live world **and rebuilds a T13/T14 lighting clipmap from it**
  (`sim_light_volume`: terrain occupancy sampled at the terrain cell size +
  detached body AABBs filled solid) so each frame is lit with indirect GI.
  Measured on the reference adapter: 10/10 cuts commit; terrain solid cells
  304 → 243 monotone non-increasing (no regrowth); the GI-lit destruction
  renders correctly; the settled frame is bit-stable (60-frame band flicker
  0.000000). Per-tick GPU cost is the increment-1 cold full-retrace number
  (~12 ms p50), not a client frame. **No quality flags.** Completes the G2 scene
  matrix. Evidence: `sandbox-capture`
  `tests::sim_light_volume_tracks_terrain_occupancy_and_cuts`; the GPU run is
  manual.
- **Increment 6 (later).** A bounded per-tick destruction cost (sim →
  incremental `LightingUpdate` instead of a cold full re-trace); a pipelined
  (threaded) client frame loop + the broader CPU frame-work budget; a bounded
  denoise/temporal pass if a real scene tightens the budget; cross-GPU capture +
  human review; the cell-size / renderer-direction freeze decision.

## Persistence, scale, and game-ready slice

### T16 — Durable world checkpoint and journal

Dependencies: T08, T01. Own: spall_store and save integration.

Implement versioned SQLite records, compressed bricks, topology/pose journal, ID counters, immutable tick checkpoints, durable acknowledgement, and recovery. Keep schema/data conversion separate from runtime handles. Implement controlled crash points and disk-error injection.

Accept: save/restart preserves excavations, rotated fractured bodies, materials, and sleeping state; crashes around transaction/checkpoint publication never duplicate or lose ownership within the durable prefix; disk failure prevents false save success. Report measured bytes/write rate.

### T17 — Live late join, repair, reconnect

Dependencies: T10, T16. Own: baseline/catch-up integration.

Implement tick/cursor-consistent baselines, bounded bulk transfer, transaction catch-up barriers, current motion keyframes, hash repairs, and session renewal. Retry with a fresh snapshot on bounded catch-up overflow. Keep already connected players running.

Accept: a third client joins during repeated destruction, reconnects, and repairs an intentionally corrupted brick; no replay-from-world-creation dependency; memory/queue limits hold; an expired session cannot act.

### T18 — Streamed voxel and body residency

Dependencies: T07, T16, T17. Own: server/client residency and structural indexes.

Implement cache budgets, dirty persistence, interest hysteresis, complete body ownership, graph metadata residency, structural dependency loading, and safe collision entry. Start with the bounded 256 x 128 x 256 m fixture. Maintain one physics origin at this scale.

Accept: cut support beyond the visible/loaded neighborhood and still release the correct component; unload/reload modified-air bricks without regrowth; a body crossing partitions remains one body; fast movement cannot enter missing collision; steady traversal memory plateaus.

### T19 — Player movement and prediction

Dependencies: T06, T10. Own: capsule/controller and input reconciliation.

Implement walk/jump, grounding, steps/slopes, moving-body contact, bounded history, acknowledged-input replay, and topology-change invalidation. Add non-graphical scripted players. Tool validation remains on server time.

Accept: 100 ms RTT movement is responsive; corrections remain within reported tolerances; removing a floor during replay cannot leave the player hovering; moving debris pushes/crushes only according to server outcomes. Test lost held-button releases.

### T20 — Interest and bandwidth scheduling

Dependencies: T17, T18, T19. Own: replication priorities and load admission.

Implement byte budgets, relevance by body bounds, motion frequency priorities, snapshot supersession, reliable backlog limits, dependency baselines, and explicit busy responses. Record actual transport egress as well as encoded application bytes.

Accept: eight clients pass the configured bandwidth scenario; cross-interest splits remain atomic; topology queues drain after a burst; snapshots do not consume all capacity needed by controls or joins. No dropped committed geometry events.

### T21 — Contact damage and dormant debris

Dependencies: T08, T18. Own: server damage rules and region sleep policy.

Convert contact impulses to bounded server damage intents using documented thresholds/cooldowns; prevent recursive explosions in one tick. Persist/sleep distant settled regions, waking them before interaction. Cosmetic particle limits must not remove solid mass.

Accept: repeated resting contacts do not continuously fracture floors; a falling body can damage terrain; sleeping rubble can be excavated and wake; fragment counts and pending jobs stay bounded with explicit admission behavior.

Delivered in increments (ticket stays open until all acceptance bullets have evidence):

- **Increment 1 (contact → terrain damage).** `spall_physics::PhysicsWorld::contact_impulses` exposes per-pair solved normal impulse / contact point / normal after each step (read-only, no callback mutation). `spall_sim::contact_damage::ContactDamagePolicy` is the pure filter: impulse must exceed `impact_ratio ×` the striking body's `m·g·dt` resting support impulse *and* an absolute floor (a body at rest never qualifies); per-brick cooldown blocks repeats; a world-wide per-tick cap drops (and counts) the excess instead of queueing it; a body born on the same tick is skipped (recursion guard). `Simulation::apply_contact_damage` submits the cuts against the terrain volume with server-authored request ids (`1 << 62` band); they stage off-tick and commit later like a client edit. Covers "repeated resting contacts do not fracture floors", "a falling body can damage terrain", and "bounded fragment counts / pending jobs". Evidence: `spall_physics` `contact_impulses_spike_on_impact_then_decay_to_the_resting_load`; `spall_sim` `contact_damage` integration test.
- **Increment 2 (region dormancy).** `spall_physics::PhysicsWorld::deactivate_body` / `reactivate_body` are a reversible sibling of `retire_body`: a settled body's Rapier rigid body + collider are removed to save step cost, its `BodyId` slot and rebuild parameters kept, and it is rebuilt in the same slot from the caller's grid + stored pose on reactivation. `spall_sim::dormancy::DormancyPolicy` is the pure decision layer: a body asleep and still for `settle_ticks` with no active region (player capsule / awake body) within `wake_margin_m` of its bounding sphere is deactivated; a dormant body is reactivated when an edit targets it (immediately, via `Simulation::submit`) or a player/awake body approaches (after `min_dormant_ticks` hysteresis); deactivations and proximity reactivations are each capped per tick. `Simulation::apply_dormancy` is opt-in (not run by `tick`). Dormancy changes no cells/ownership/damage, so `world_hash`, conservation, and the checkpoint set are unaffected. Covers "sleeping rubble can be excavated and wake" and the `sleep-wake` fixture. Evidence: `spall_physics` `a_dormant_body_leaves_the_step_set_and_reactivates_at_its_pose`; `spall_sim` `dormancy` integration test.
- **Increment 3 (body-on-body fracture + adjacent-edit wake).** `ContactEvent` / `PlannedDamage` carry `target: EditTarget` and the contact point in the **target volume's local cell frame** (`point_cell`), so `ContactDamagePolicy` is unit-agnostic and the brush + per-region cooldown key are one code path for terrain and bodies — a moving body's cooldown spot no longer drifts with its world pose. `Simulation::apply_contact_damage` now also handles `(dynamic, dynamic)` contacts: it damages the body being struck — the slower of the pair, tie-broken to lower mass (`ContactDamageConfig::still_speed_m_s`) — with the world contact point mapped through that body's pose into local cells, and submits the cut through `Simulation::submit`. The fracture is a normal committed transaction, so conservation and the request-id band are unchanged; the existing per-region cooldown, per-tick cap, and born-this-tick guard bound the cut stream (a settling stack does not cascade). `Simulation::apply_dormancy` takes the tick's `TickReport`: a terrain transaction committed this tick within `wake_margin_m` of a dormant body hard-wakes it (bypassing `min_dormant_ticks`) — "nearby edits wake affected neighbors". The `spall_sim` fixtures now set `disable_ccd = true` to match `spall_server::serve` (no authoritative body enables per-body CCD; the CCD broad-phase can otherwise panic on a collider a body-fracture edit rebuilds mid-impact). Covers the remaining `contact-damage` / `sleep-wake` fixture rows. Evidence: `spall_sim` `contact_damage` (`a_heavy_body_dropped_on_a_lighter_one_fractures_the_lighter_one`, `settling_debris_stack_does_not_cascade`) and `dormancy` (`a_terrain_cut_under_dormant_rubble_wakes_it`, `a_terrain_cut_far_from_dormant_rubble_leaves_it_dormant`) integration tests plus the body-target pure-module unit tests. 3c (wiring both opt-in passes into `serve`) is deferred to its own ticket — it needs a networked `sleep-wake` fixture and a `--await-body-settle` gate-interaction review.
- **Increment 4 (3c — server wiring).** `ServeConfig.contact_damage` / `.dormancy` (`Option<...Config>`, default `None`) wire `Simulation::apply_contact_damage` / `apply_dormancy` into `spall_server::serve`'s tick loop, one call each right after the commit they react to; counts land in `ServeSummary` (`contact_damage_cuts_submitted/rejected`, `dormancy_deactivations/reactivations_total`, version 5). `sandbox-server` gains `--contact-damage` / `--dormancy` (each applies the module's `DEFAULT` tuning). Both stay off for every existing gate scenario — a deactivated body leaves the live physics world, which `--await-body-settle`'s settle check reads directly. New `Scene::SleepWake` (`spall_voxel::fixtures::sleep_wake_arena` + `spall_sim::fixtures::sleep_wake_setup`): the walk arena plus a small column-and-beam in the player lane, 15 m from spawn. `fixtures/scenarios/sleep-wake.json` (`cargo xtask scenario --name sleep-wake`): one client cuts the column, the detached beam settles and (with `--dormancy`) deactivates with nobody nearby, the client walks into the wake margin and it reactivates by proximity, and a final body-targeted cut proves it is still destructible — `dormancy_assertions` requires >= 1 deactivation and >= 1 reactivation in the server's own report. Passes clean, under 2% loss, and with `replay_check`. Closes out T21 — every acceptance bullet now has both CPU-side and networked evidence.

### T22 — Material-dependent structural strength

Dependencies: T07, T08, T16. Own: strength design and spall_structure extension.

First produce a reviewed specification of cluster/bond formation, capacity/load units, deterministic propagation, failure order, and save/replication fields. Then implement that exact approximation with small analytic fixtures. Do not invent a stress solver inside a renderer or collider task.

Accept: long weak cantilevers fail, comparable strong supports hold within declared limits, removing support cascades predictably, restart preserves damage, and replicated hashes include every structural layer. Document artifacts and performance; no claim of physically exact engineering simulation.

### T23 — G3/G4 integrated engine acceptance

ENG-76 takeover (2026-09-22): exact spatial indexing replaces the quadratic
dormancy proximity scan without changing sleep/wake policy. See
[measured evidence and remaining gates](reports/ENG-76-proximity-index.md).
The historical worker diagnostic and the integrated G4 workload are reported
separately; ENG-76 and T23 remain open.

Current disposition: **ENG-30 is Done in Loopira (2026-09-23, user-directed); gate evidence remains qualified below.**
This status does not claim every G3/G4 validation target passed. The
[2026-09-18 acceptance audit](reports/T23-acceptance-audit-2026-09-18.md)
records the then-current findings.
Of the audit's five findings, increment 37 (`docs/reports/G3.md`) fixes
finding 1 (a real checkpoint-integrity regression — evicted-brick backing
reads on the incremental-capture cache-miss path had lost their digest
verification across a merge) and confirms finding 2 (baseline/repair
integrity) was already closed by a same-day commit the audit's own stated
verification numbers predate. Findings 3-5 — partial G4 soak evidence, a
workload narrower than the required integrated gate, and incomplete
network/visual evidence — are measurement and workload-coverage gaps, not
correctness defects, and remain recorded as evidence limitations. Full G4
measurement / workload coverage is not claimed. T24 may proceed under the
user-directed ENG-30 status update; this does not waive or rewrite those
remaining measurement findings.

Post-merge follow-ups and evidence limits are recorded in the
[ENG-30 review](reviews/2026-09-10-eng-30-post-merge.md). Historical review
text saying T23 stays open predates the 2026-09-23 Loopira status update.
Those initial atomic-reload and traversal-assertion fixes have landed, as have
bounded checkpoint capture and disk-backed residency with restart evidence.
The current [G3 report](reports/G3.md) records increments through 37: the
passing row 11 impaired join-budget run, row 7's pin-lifetime and
capacity-admission enforcement pass, row 7's incremental (non-full-reload)
checkpoint capture, row 7's client-side dense-byte admission, a durable-ack
audit that found the evict-time boundary already correct but fixed a real gap
in checkpoint capture's verification of evicted-brick backing records, and
(increment 37) the fix for the regression the 2026-09-18 acceptance audit
found in that same checkpoint-capture verification after a later merge
reintroduced the gap on a path the original fix predated. The only item row
7's increment-13 deferred list named that remains unaddressed is the dormant
`ResidencyController`/`ClientResidency` policy-engine merge, which an earlier
coordinator review explicitly rejected as the objective — not something left
to schedule. Whether row 7's addressed gaps constitute full T23 gate
acceptance is still a judgment call for an integrator reviewing the whole
report, not a claim made here. Earlier increment notes below are historical
evidence, not the current remaining-work queue.

**2026-09-20 update (increment 38, `docs/reports/G3.md`).** T23 stays **not accepted**.

**2026-09-20 follow-up (increment 39, `docs/reports/G3.md`).** T23 stays **not accepted**. The checkpoint CI failure was reproduced on main `7d3ca00` and fixed by `8e0cbb2` (not yet on main). The first wake-locality fix (`8a18476`) was unsafe — its forced re-sleep trapped awake neighbours and pushed bodies out of the world (fixed and regression-tested in `ea4f1ae`/`6e55b82`); the corrected fix does not measurably change the workload (~99% of sleeping-body wakes are physics-step island propagation, unattributed). On the reviewed revision the 30-minute soak still **fails** (server cannot hold 60 Hz; awake bodies reach ~5,000) and rubble bodies still escape the world under soak-scale load; containment has no acceptance check. Physics p95, tick p95/p99, impaired-lane send-queue recovery, residency-under-G4 and GPU/visual evidence remain open. No compaction or lifecycle redesign is authorised.

**2026-09-20 update 2 (increment 40, `docs/reports/G3.md`).** T23 stays **not accepted**. The forced re-sleep wake optimisation was withdrawn after an external review reproduced a stability regression; increment 39's origin-based containment claims are retracted (a rubble rod's geometry sits ~10 m from its body origin). Containment is now judged from transformed cells and collider bounds: no solver embeddings on the current revision; rubble crossing the west wall is physically legitimate but leaves the bounded world, and README's dormant external-body set is **declared but unimplemented** (Open items rows 17-18). Whole-world terrain collider replacement on each dig accounts for ~70% of sleeping-body wakes (control run without digs). Checkpoint-integrity fix prepared on local branch `fix/t23-checkpoint-integrity` (`a0b4907`; `cargo xtask check` and `smoke` pass), not pushed. Reviewer demos: `.local/reviews/2026-09-20-wake-locality/demos/wake_demos.html`. Compaction and lifecycle redesign remain unauthorised.
The integrated G4 fixture, a declared terrain+body edit mix, and per-stage /
per-client-timeline instrumentation are in. The three hard gate failures were
root-caused and fixed on this branch: replica divergence (motion datagrams starved
the reliable stream inside the QUIC congestion window -> congestion-aware motion
budget), simultaneous-join failures (serial handshakes in one accept loop ->
concurrent accept), and 35-41 s join readiness (serial per-joiner tick-thread
snapshots -> shared capture; now 27-30 s). Final loopback and both impairment lanes
converge on all 8 replicas with matching replay/restart hashes. **Still failing:**
tick p95/p99 and physics p95 (17/50-60/12 ms vs 12/16.7/6), terrain-commit hashing
(~32 ms per terrain edit) and physics are the identified costs; the 30-minute soak
did not complete (rubble tunnels through the ground and never sleeps; one new body
per edit; journal writes every awake body 20x/s); the overload client-retry gap; the
stress lane is measured-but-unaccepted (its bounds are unratified candidates). GPU
p95 14.8 ms vs 12 ms and human/cross-GPU review are separately open. Four
independent fixes are in PR #136 (merged 2026-09-20 with the checkpoint-integrity fix, PR #137). **Gate blockers remain until the fixes above are reviewed:** the 2-minute physics p95 miss, the failed 30-minute soak, the impaired-convergence failures (fixed on this branch, not yet accepted) and the baseline-readiness miss (27-30 s, thin margin). Acceptance is an integrator decision; ENG-30 is not marked done here.

**Update 2026-09-20 (G3.md increment 41; ENG-30 open, T23 not accepted):** v2 soak: 8,259 of 18,181 requested edits committed, 9,922 rejected (queue-full); convergence of committed work is reported separately from workload completion. Server restart recovery passes; fresh-client reconnect fails (baseline 377 MiB > 256 MiB `TooLarge` cap; the 300-tick lifetime theory is disproved). Blast-recovery rows with zero tail samples are missing evidence, not measured backlog (wall-clock clusters, fail-closed, thresholds unchanged; stress recovery unproven). Per-brick terrain colliders prototyped default-off (`docs/reports/terrain-collider-locality.md`). The 30-minute soak was not re-run.

Dependencies: T15, T17, T18, T20, T21, T22. Own: complete gate report and targeted fixes.

Run all correctness, crash, impairment, visual, and eight-client workload scenarios. Include geographically separated players inside the bounded world, a multi-region collapse, prolonged rubble accumulation, and late join after heavy edits.

Accept: validation.md gates pass or remaining failures are clearly recorded as open. Produce reproducible commands and raw evidence. This is the first game-ready engine slice, still without menus/editor/survival content.

ENG-31 / T24 increment 19 (2026-09-23): the feasibility report contains a
criterion-by-criterion acceptance audit, measured fixture bounds, reproducible
commands, and explicit open failures. The eight-origin debris and separated
player fixtures, adapter split/merge/body-transfer checks, SQLite distant-edit
reload, and render-only seam invariant are demonstrated. Procedural generation
and version coexistence, far/large structural-graph growth, long-session
storage growth, continuous approach-triggered production merge, full server
region routing, and a product-scale envelope remain open; no larger-world gate
pass or maximum-scale claim is made. The audit fulfills T24's reporting
acceptance, which allows unresolved gate failures when they are clearly
recorded. Follow-up work should be assigned before making a G5 feasibility
claim.

Lands in increments against `docs/reports/G3.md`.

- **Increment 1 (separated regions, multi-region collapse, late join).** New `separated-regions` scene (`spall_voxel::fixtures::separated_regions_scene` / `spall_sim::fixtures::separated_regions_setup`, `Scene::SeparatedRegions`): one volume bounded to 256 x 128 x 256 m holding two independent collapsible bridge structures 18 m apart. Every connecting client gets a capsule; `SEPARATED_REGION_SPAWNS` puts even slots west and odd slots east (`Scene::player_spawns` replaces the unconditional `WALK_ARENA_SPAWNS`). Built-in scenario `t23-g3.json`: one server + three live clients + one `--late-join` replica; both regions' columns are severed (multi-region collapse), each floor's ends excavated, and the late client joins after those six edits. Passes headless and at `--loss-percent 3`: all seven cuts commit, all four clients + server converge to one hash, the committed stream replays from baseline to that hash, and both detached beams are reported at rest. Evidence: `cargo test -p spall_sim --test separated_regions`; `cargo xtask scenario --name t23-g3`. Open G3/G4 rows (resident-cache eviction, save/restart + traversal, the full persistence crash matrix, the join-duration budget, the eight-client workload + soak, measured interest/bandwidth separation) are enumerated in `docs/reports/G3.md`.
- **Increment 2 (cold restart + recovery round-trip).** New `restart_check` scenario flag, on for `t23-g3`. After the run and the exact-replay check, the harness stops the server, launches a fresh `sandbox-server --serve --save` over the same `world.db` (cold recovery from the shutdown checkpoint + durable journal, no edit replay from creation) and requires its recovered `final_world_hash` to equal the agreed hash, then connects a fresh `--late-join` client to the restarted server and requires it to converge to that same hash. `summary.json` gains `restart_checked` / `restart_recovered_hash_matches` / `restart_reconnect_hash_matches` / `restart_recovered_world_hash`. Passes headless and with the main run at `--loss-percent 3`. Still open on this row: "traverse away and back" before the save (needs the residency budget knob from the eviction row).
- **Increment 3 (persistence crash matrix).** `cargo xtask crash-test --suite persistence` (`spall_server::persist::run_crash_suite`) goes from 7 to 12 scenarios so "crashes at every persistence transaction boundary plus truncated/corrupt data and disk-full injection" is one command. New: `crash_mid_checkpoint_rows` and `crash_after_checkpoint_commit` (the two previously-unexercised `CrashPoint`s — the second proves recovery resumes from a durable-but-unacked checkpoint), `journal_crc_corruption_*` and `interior_journal_gap_*` (both: recovery reports it and truncates the durable prefix; `RequireClean` fails closed, `AcceptDurablePrefix` resumes from the clean prefix), and `disk_full_on_checkpoint` (`SQLITE_FULL` → no false ack, writer poisoned, fall back to cp0 + journal). `spall_store` gains `FaultPlan::disk_full` and a `#[doc(hidden)]` `spall_store::inject` module (`break_journal_crc` / `remove_journal_row`, direct row mutation on a closed DB). The suite's scripted workload is now three committed transactions so the journal has an interior record. Real abrupt process kill stays delegated to `cargo test -p spall_store --test abrupt_crash`. Evidence: `cargo xtask crash-test --suite persistence`; `cargo test -p spall_store --test durability`.
- **Increment 4 (impaired late join: bounded explicit failure).** The successful-join half of "retry/catch-up stress terminates with either a successful join or a bounded explicit failure while connected clients continue" is `t23-g3` (increment 1). This adds the bounded-failure half: `sandbox-client` writes a structured `{ "result": "join-failed", … }` summary and exits `4` (was: exit with no summary) when a `--late-join` replica cannot obtain a baseline; a new `late_join_may_fail` scenario flag then accepts a late client that *either* converged *or* ended in that bounded explicit failure — a `join-failed` summary plus a real process exit (a deadline kill is `None` and is rejected as a hang) — as long as the live clients + server still converge and replay. `fixtures/scenarios/t23-g3-impaired-join.json` delays the late connect until after the server has shut down, so the failure is deterministic. Evidence: `cargo xtask scenario --name t23-g3-impaired-join`.
- **Increment 5 (residency-aware world hash — design).** Row 7 (live resident-cache eviction in `serve()`) is blocked: `SimWorld::world_hash()` / `ReplicaWorld::world_hash()` fold **resident bricks only** and both ends converge by comparing that value, so evicting a clean terrain brick mid-run breaks convergence and replay. Design only — superseded by increment 6's amendment.
- **Increment 6 (logical topology contract + digest foundation — slice A).** The 2026-09-10 review (`docs/reviews/2026-09-10-g3-residency-handling.md`) rejected a reporting-only hash fix — transaction validation, structural staging, baselines/repair, recovery, and `total_solid_cells` all also read resident-only state. `docs/reports/G3-residency-hash.md` is rewritten as the frozen contract (one logical topology = resident bricks ∪ retained evicted digests, each key once; existing `canonical_topology_hash` records unchanged; digest lifecycle table; known-empty rule; client-scope limits; 6 serve-loop boundaries; memory/observability; slice A–E plan with paired-run acceptance). **Slice A implemented:** `spall_voxel::logical` (`BrickDigest`, `EvictedBricks` + lifecycle, `logical_bricks`, `logical_solid_cells`) and `spall_sim::canonical_logical_volume_for`. Evidence: `cargo test -p spall_voxel --lib logical` (10) + `cargo test -p spall_sim --test logical_hash` (3) — `logical_hash(full, {}) == existing_world_hash(full)` and the same for any evicted subset/order. Residency stays default-off. Slices B (txn/structural), C (backing/capture), D (default-off serve wiring), E (client residency + row 8b) open and gated in order.
- **Increment 7 (row 7 slice B — transaction + structural correctness).** `SimWorld` / `ReplicaWorld` carry an optional per-volume `EvictedBricks` (empty → zero change). The parent transaction `result_hash`, the replica's candidate-hash validator, `total_solid_cells`, and staging `pre_solid` / `post_solid` all fold the logical view, so a server and a replica with **different** bricks evicted still converge. `stage_edit` / `commit` return `EvictedGeometryRequired` (without mutating) when an edit writes, structurally borders, or its collider rebuild would sample an evicted brick; a far edit-irrelevant eviction is left alone. `SimWorld::evict_brick` / `clear_evicted_after_reload` implement the evict/reload transitions. Evidence: `cargo test -p spall_sim --test logical_commit` (4); `cargo test -p spall_client --test logical_residency` (3). Deferred to slice C: dependency-complete reload-and-retry (so a far eviction stops blocking a commit) + the `KnownEmpty` rule.
- **Increment 8 (row 7 slice C — backing + capture).** `spall_sim::BrickBacking` trait + `MemoryBacking`; `SimWorld::set_backing` + `reload_brick` / `reload_bricks` (load → reinstall → `verify_reload` → drop digest; `KnownEmpty` → air brick at the retained revision). The edit pipeline catches `EvictedGeometryRequired` from stage **and** commit, reloads the named bricks, and re-stages next tick; no backing / `Unavailable` → a bounded explicit `evicted geometry unavailable` rejection with nothing mutated. `spall_server::logical_world_baseline` / `logical_capture_transfer` / `logical_brick_repair_patch` fill evicted bricks from the backing so a late joiner / repair still reaches the exact hash (`world_baseline` = the `None` case, byte-unchanged with no evictions). Evidence: `cargo test -p spall_sim --test logical_reload` (3); `cargo test -p spall_server --test logical_baseline` (3). Deferred to slice D: routing `ResidencyController::enforce_budget` through `SimWorld::evict_brick`, the durable-store `BrickBacking` bridge, and checkpoint capture via `ResidencyController::capture_checkpoint` in the serve loop.
- **Increment 9 (row 7 slice D — default-off serve-loop residency wiring).** `ServeConfig.residency: Option<ResidencyLimits>` (`None` → byte-identical to every prior run) + `sandbox-server --residency-budget-bricks` / `--residency-radius-bricks` + `session.rs` scenario passthrough. `spall_server::residency_pass::ResidencyPass` owns an `Arc<MemoryBacking>` seeded from terrain and installed on `SimWorld`; per tick it keeps a brick box around every player capsule resident and evicts out-of-interest terrain after a settle-ticks hysteresis, refreshes the backing on commit, and `reload_all`s before every periodic + shutdown checkpoint. Late-join baseline + brick-repair capture route through `logical_capture_transfer` / `logical_brick_repair_patch` with that backing. `spall_sim::commit` now names every evicted brick in the volume on a collider-rebuild `Unresident` so one reload makes the retry commit next tick. `ServeSummary` → version 4 (`residency_evictions_total` / `residency_reloads_total` / `resident_terrain_bricks_{min,max,final}` / `residency_budget_miss_ticks`). Evidence: `cargo test -p spall_server --test residency_pass` (2 — residency-on reaches the same `world_hash` / conservation / `result_hashes` trace as residency-off with real evictions + pipeline reloads; default-off is a bit-exact no-op); `cargo xtask scenario --name t23-g3-residency` (`t23-g3` + `--residency-budget-bricks 6 --residency-radius-bricks 0` → server + 4 clients + replay + cold restart all converge to the residency-off agreed hash `cac2893c…d18e6de` with 56 evictions / 7 reloads). Deferred to slice E: client-side residency, `ResidencyController` unification, a durable-store `BrickBacking`, incremental checkpoint capture, the row 8b traversal fixture.
- **Increment 10 (row 7 slice E1 — client reload digest lifecycle).** `spall_voxel::EvictedBricks::drop_resident(volume)` drops every retained digest whose brick is resident again (a traversal reload at the same revision or an authoritative repair patch at a newer one). `spall_client::ReplicaWorld::apply_baseline_patch` and the transaction-commit path call it, so the logical view never carries a resident-*and*-evicted brick. `ClientResidency::wanted_reloads(replica, centre, enter_radius, max)` names retained-digest terrain bricks back inside interest for a bounded `RepairRequest`; the digest stays retained until the patch lands. Evidence: `cargo test -p spall_voxel --lib logical` (`drop_resident`); `cargo test -p spall_server --test client_residency` (3 — a replica that evicted a region reloads it from ordinary server repair patches and drops the digests, `world_hash` + `resident_brick_count` match the server; a cut into a fully-evicted replica region gaps → `NeedsRepair` → repair + retry → converges with only the reloaded bricks' digests dropped; `wanted_reloads` targeting). Deferred to slice E2: wiring `ClientResidency` into `spall_client::net` behind `sandbox-client --residency-*`, and the row 8b scripted traversal fixture.
- **Increment 11 (row 7 slice E2 — client residency in the session + row 8b traversal).** `spall_client::ClientResidencyPass` runs once per mover iteration against the predicted player capsule: keep a Chebyshev brick box around the player resident, evict the rest of the terrain after an `EVICT_SETTLE_STEPS` hysteresis (digest retained → `world_hash` exact), send a rate-limited `RepairRequest` for every retained-digest brick back inside the box (slice E1's `drop_resident` supersedes the digest as the patch lands). `ClientNetConfig.client_residency` (`None` → fully resident) + `sandbox-client --residency-budget-bricks` / `--residency-radius-bricks` + `session.rs` `client_residency_*` passthrough (movement-scripted clients only) + `BaselineScene::Walk`. The interest box must cover the predicted-collision region (the mover rebuilds its predicted collider from resident geometry only). Evidence: `cargo xtask scenario --name t23-g3-traversal` (`walk` lane, server + client residency on; client 0 walks ~14.6 m out and part-way back, evicting 6 terrain bricks and reloading 4; client 1 dents the far floor while client 0 has that brick evicted; server + both clients + exact replay + cold restart + reconnect all converge to `2a47eda7…`, deterministic; mover stays grounded). Follow-ups (own tickets): `ResidencyController` unification, durable-store `BrickBacking`, incremental checkpoint capture, logical-terrain predicted collider.
- **Increment 12 (post-merge residency correctness + enforced evidence).** Reload candidates are digest-validated before publication, so wrong revision/content leaves the slot nonresident and logical state unchanged; correct retry still succeeds. `ClientResidencyPass` globally caps reloads at four requests/step and counts completed loads + budget misses. Prediction collision invalidates on resident-cache changes. `ClientSummary` / `SessionSummary` v2 and `session.rs` enforce server/client evictions, completed reloads, an edit gap on evicted geometry, and outbound/return waypoints. Evidence: `cargo test -p spall_sim --test logical_reload`; `cargo test -p spall_server --test client_residency`; `cargo test -p spall_client`; `cargo test -p xtask requirement_tests`; `cargo xtask scenario --name t23-g3-traversal` (23.38 m out → 10.03 m final, server 5/3 evict/reload, client 6/4/4 evict/request/complete, replay/restart/reconnect match). Prediction-safe traversal under `2%` loss remains open.
- **Increment 13 (row 7 — pin lifecycle + capacity admission enforcement).** Coordinator review (`docs/reports/ENG-30-row7-remaining.md`, 2026-09-18) found `ResidencyPass::run`'s interest set had no explicit reservation for pending-edit dependencies, swept-collision paths, or a brick the pipeline had just reactively reloaded, and that `budget_bricks` was a reported ceiling only with no dense-byte cap. `ResidencyPass::run` now takes `pending_edit_bricks` (`Simulation::pending_edit_bricks` / `EditPipeline::pending_dependency_bricks` — a per-axis bounded brush AABB + one-brick halo per queued intent) and internally pins two more sources: a grace window (`note_pipeline_reloads`) for bricks the pipeline itself just reloaded, and the swept path of every player (keyed by stable entity id, not list position) and active body between ticks. The union is never evicted and always reloads regardless of budget. `ResidencyLimits::max_dense_bytes` is a real second ceiling enforced on admission (an interest-driven, non-required reload is deferred, not admitted, past either cap, using a conservative-then-corrected byte estimate); `PassTick::required_over_budget` reports, without evicting required geometry, when the required set alone cannot fit. `ServeSummary` v7 / `SessionSummary` v4 add `residency_pinned_bricks_max`, `residency_admission_deferred_total`, `residency_required_over_budget_ticks`, `residency_digest_bytes_final`, `residency_backing_resident_bytes`, and real OS `process_peak_memory_bytes` (`spall_server::mem_stats`, no new dependency). Evidence: `cargo test -p spall_server --test residency_pass` (10, 3 new — pending-edit pin lifetime, swept-path pin past the settle window, dense-byte admission deferral with paired on/off hash equality); `cargo xtask scenario --name t23-g3-residency` (now also `--residency-budget-dense-bytes`, converges to the residency-off agreed hash `cac2893c…d18e6de`, reports real `residency_pinned_bricks_max`/`residency_required_over_budget_ticks`); `cargo xtask scenario --name t23-g3-traversal` (new `min_pinned_bricks` floor, converges to `2a47eda7…` clean and at `--loss-percent 2`); `t23-g3`, `t23-g3-residency-disk`, `t23-g3-impaired-join` re-verified unchanged. Full `cargo fmt` / `cargo clippy -D warnings` / `cargo test --workspace --all-features` green. Deferred: client-side (`ClientResidencyPass`) dense-byte admission, durable exact-revision acknowledgement (still an in-memory/disk ack-before-evict contract), incremental (non-full-reload) checkpoint capture, and the `ResidencyController`/`ClientResidency` (T18, dormant) policy-engine merge — the coordinator review explicitly rejected literal class consolidation as the objective.
- **Increment 14 (row 7 — client-side dense-byte admission).** Increment 13's deferred item: `ClientResidencyPass` mirrors the server's `ResidencyLimits::max_dense_bytes` admission enforcement for its single predicted player. `ClientResidencyLimits::max_dense_bytes` (`--residency-budget-dense-bytes` on `sandbox-client`, `Scenario.client_residency_budget_dense_bytes` in `session.rs`) is a real ceiling alongside `budget_bricks` — historically a reported-only field — both now enforced on the reload-*admission* path: every `RepairRequest` this pass would send for a box-driven (never pinned — this pass has no pending-edit/swept-collision sources of its own) reload is costed conservatively (candidate as if fully `Dense`, `spall_voxel::MemoryReport::DENSE_BRICK_BYTES`, the same shared estimator the server uses) against a running projection that includes bricks already requested but not yet completed (this pass's reload is asynchronous, unlike the server's synchronous `world.reload_brick`), and deferred rather than admitted past either cap; the projection is corrected to the real measured total every step. Eviction itself is unchanged — still governed unconditionally by the box, never by budget, so a tight cap only ever delays a desired reload, never forces an eviction inside the box. `ClientSummary` v4 adds `client_residency_admission_deferred_total`; `session.rs`'s `ClientRow` / `ResidencyAssertions.min_client_admission_deferred` mirror it. Evidence: `cargo test -p spall_server --test client_residency` (7, 1 new — `a_tight_dense_byte_cap_defers_a_desired_client_reload_instead_of_admitting_over_budget`: a zero-headroom cap defers every desired reload, leaves the brick evicted, never moves `world_hash`; raising the cap admits it); `cargo test -p xtask` (new `residency_assertions_cover_client_admission_deferral`); new fixture `fixtures/scenarios/t23-g3-traversal-dense-cap.json` (`t23-g3-traversal` + a `client_residency_budget_dense_bytes: 196608` tight enough to bind — measured `client_residency_admission_deferred_total: 21`, evictions 6 / requested 4 / completed 4 — converges to the same agreed hash `2a47eda7…` as `t23-g3-traversal`, which itself re-verified unchanged with the cap left at its default-disabled `u64::MAX`); `t23-g3-residency` re-verified unchanged (server-side, no cross-talk). Full `cargo fmt --all --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo test --workspace --all-features` (every crate, 0 failures, the historically flaky `spall_net::separate_process_transport` passed clean) / `cargo xtask check` all green. Deferred (unchanged from increment 13): durable exact-revision acknowledgement, incremental checkpoint capture, and the `ResidencyController`/`ClientResidency` (T18, dormant) policy-engine merge.
- **Increment 15 (row 7 — incremental checkpoint capture).** Increment 22 stopped `ResidencyPass::capture_checkpoint` from `reload_all`-ing every evicted brick back into the live world before a checkpoint, but the capture itself still re-snapshotted / re-loaded / re-encoded **every** logical (resident ∪ evicted) terrain brick on every call — a full logical-world walk, just one that no longer disturbed live residency placement. `ResidencyPass` now keeps a per-brick `checkpoint_cache` (coord → last-captured `(Revision, StoredBrick)`), seeded at install time from every then-resident brick (the same walk that already seeds the backing) so even a run's first real checkpoint skips bricks untouched since world creation. On each `capture_checkpoint` call, a brick whose current revision (from the live snapshot if resident, from the retained digest if evicted) matches its cached revision reuses that prior `StoredBrick` record verbatim — no fresh snapshot/backing-load/zstd-encode; a brick whose revision differs (edited, or evicted/reloaded at a new revision) is re-captured and the cache updated. Every checkpoint's `bricks` list is still the *complete* logical set (checkpoints are pruned — `RETAIN_CHECKPOINTS` — and each one must stand alone for cold recovery); only how that list is assembled changed. `spall_server::persist::capture` is refactored (not behaviourally changed) into a thin wrapper over a new `capture_with_terrain_bricks(sim, cfg, cursor, terrain_bricks)` so `ResidencyPass` can supply its incrementally-computed terrain-brick list while reusing the same body/meta/hash logic; `persist::capture` itself, the crash suite, and the residency-off `serve()` path are untouched and still do the historical full walk. `ServeSummary` / `SessionSummary` add `residency_checkpoint_bricks_captured_total` (cumulative bricks actually re-captured) and `residency_checkpoint_bricks_logical_total` (cumulative complete-logical-set size) — `captured_total < logical_total` is the direct, measured evidence of incrementality. Evidence: `cargo test -p spall_server --test residency_pass` (12, 2 new — `a_second_checkpoint_only_recaptures_bricks_that_changed` proves a second checkpoint after one small edit re-touches only that edit's brick, not the whole resident set; `incremental_and_full_walk_capture_recover_to_the_same_world` proves an incrementally-captured checkpoint and one from a fresh (full-walk-equivalent) pass sharing the same backing produce byte-identical `Checkpoint.bricks`, the same `world_hash`, and recover to the same `restart_recovered_world_hash`); the pre-existing `capture_checkpoint_fails_closed_on_a_missing_durable_record` test now targets a genuinely-edited (cache-miss) evicted brick, since a never-touched brick's cached record is legitimately reused without touching the backing at all — the fail-closed guarantee is unchanged for the case it actually protects. `cargo xtask scenario --name t23-g3-residency`: converges to the agreed hash `cac2893c…d18e6de`, `restart_checked` / `restart_recovered_hash_matches` / `restart_reconnect_hash_matches` all `true`, and reports `residency_checkpoint_bricks_captured_total: 2` of `residency_checkpoint_bricks_logical_total: 9` (only the two edited terrain bricks were re-captured at the single shutdown checkpoint this scenario's `--checkpoint-interval-ticks 0` produces; the other seven reused their install-time cache seed). `cargo xtask scenario --name t23-g3-residency-disk`: same hash, same `2`/`9` split, disk-backed. `cargo xtask scenario --name t23-g3` and `t23-g3-traversal` re-verified unchanged (`cac2893c…d18e6de` and `2a47eda7…` respectively; the traversal scenario also runs server residency and reports `1`/`4` captured/logical). Full `cargo fmt --all --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo test --workspace --all-features` (every crate, 0 failures, including `spall_net::separate_process_transport`) / `cargo xtask check` all green. **Not done in this increment**: durable exact-revision backing acknowledgement (unchanged from increment 13); the `ResidencyController`/`ClientResidency` (T18) policy-engine merge (unchanged, explicitly not the objective); client-side (`ClientResidencyPass`) checkpoint capture is not incremental (the client has no checkpoint concept — this row item is server-only, matching the frozen contract's serve-loop boundary 6).
- **Increment 16 (row 7 — durable exact-revision backing acknowledgement: audit, then a targeted fix).** Closes the item increments 13-15 all carried as deferred, but by auditing the actual call paths first rather than assuming the deferred-list wording meant the feature was missing — per `docs/reports/ENG-30-row7-remaining.md`'s explicit caution that "a boolean synchronous capture result is not by itself proof that exact-revision acknowledgement is missing; any API replacement needs a demonstrated need." The frozen contract's Evict boundary requires a durable ack of the exact revision before dropping geometry; tracing `ResidencyPass::run`'s evict call site found `backing.capture(...)` and `world.evict_brick(...)` already happen on the same tick, same thread, against the same `&mut SimWorld` borrow — the borrow checker itself rules out any interleaving, for both `MemoryBacking` and `DiskBrickBacking`. That boundary was already correct; no change made there, and `residency.db`'s `synchronous=NORMAL` pragma was left alone once tracing `spall_store::recover` confirmed crash recovery never reads `residency.db` at all (not load-bearing for any acknowledgement the contract requires). The real gap was found by tracing the checkpoint path instead: `capture_checkpoint` read evicted-brick records straight from the backing into the durable (`synchronous=FULL`) checkpoint with no verification against the retained digest — unlike `SimWorld::reload_brick`, which validates every candidate via `EvictedBricks::verify_candidate` first — so a stale or corrupted backing record could silently reach the durable save file while `checkpoint.world_hash` kept reporting the correct resident-only value. Fixed by reusing that same `verify_candidate` machinery inside `capture_checkpoint`: a mismatch now returns a new `PersistError::EvictedBrickDigestMismatch { volume, coord, retained_revision, backing_revision, .. }` and fails the whole capture closed, no new API shape or two-phase commit protocol invented. Evidence: new fault-injection test `capture_checkpoint_fails_closed_on_a_backing_record_that_disagrees_with_the_retained_digest`, confirmed failing against the pre-fix code (a forged record was silently accepted, forged bytes reaching `checkpoint.bricks`) and passing post-fix; `cargo test -p spall_server --test residency_pass` (12/12); `cargo xtask scenario --name t23-g3-residency` / `t23-g3-residency-disk` converge to the agreed hash `cac2893c…d18e6de`, unchanged; `cargo xtask scenario --name t23-g3-traversal` converges to `2a47eda7…3b51d8`, unchanged; `cargo xtask crash-test --suite persistence` (12/12); `cargo test -p spall_store --test abrupt_crash`. Full `cargo fmt --all --check` / `cargo clippy --workspace --all-targets --all-features -- -D warnings` / `cargo test --workspace --all-features` (every crate, 0 failures) / `cargo xtask check` all green. **Flagged, not fixed** (kept out of scope): `spall_server::baseline.rs`'s `baseline_volume` / `snapshot_world` / `logical_brick_repair_patch` have the same unverified-read-from-backing pattern on the client-facing late-join-baseline and repair-patch path — lower severity (corrupts one client's network resync, not the durable save file), real, own future item. **Not done in this increment**: the `ResidencyController`/`ClientResidency` (T18) policy-engine merge — unchanged, per the coordinator review's explicit guidance that literal class consolidation is not the objective; this is the only item increment 13's original deferred list still names as unaddressed. Also unchanged/out of scope: the join-duration budget follow-ups (row 11, already closed separately — see PR #129), live catch-up exhaustion (row 10), the G4 eight-client workload + soak (rows 12–14), full-envelope player separation + region-to-region traversal (row 2) — all closed by earlier increments, not reopened here.

### T24 — G5 larger-world feasibility

Procedural generation follow-up (ENG-113/114/115): `spall_worldgen` crate, sandbox/xtask wiring and editor panel, tracked separately from the G5 feasibility gate. See `docs/worldgen.md`; ENG-113 implements the crate only.

Dependencies: T23. Own: separate architecture decision and scale prototype.

Measure region streaming, render LOD seams, world generation versions, multiple rebased physics regions, region merge/split, body transfer, and structural graph growth. Set a measured world/player/debris envelope. Do not just enlarge constants.

Accept: separated players retain correct physics precision; approaching regions merge without body duplication; distant edits persist and affect support; LOD changes never alter authoritative geometry. Infinite scale remains unclaimed.

### T25 — Handoff to game systems

Dependencies: T23; T24 only if the game needs the larger-world envelope immediately.

Produce a game-facing API/examples for authoritative tools, placement, material definitions, recipes, damage, entity spawn, and asset loading. Recommend an ordered survival-content backlog separately. Keep game rules in sandbox_game, and preserve the engine's launch/scenario interface.

Acceptance amendment for the requested multiplayer progression increments:
keep rendering and engine world-storage internals unchanged; allow explicit,
versioned protocol records and thin client/server adapters for authenticated
game progression, persisted by the sandbox-owned store. Provide the requested
game-facing content APIs and examples, plus an ordered follow-on content
backlog. Engine gate scenarios remain reproducible from a clean checkout as
documented in Increment 5 below. UI/editor work remains unassigned.

Delivered in increments. **Increment 1 (game-owned wood placement):**
`sandbox_game::game::Tool::PLACE_WOOD` now creates the same validated
`EditIntent` shape as the existing dig and stone-placement examples, targeting
the stable game material ID `materials::WOOD`. The engine receives only the
normal placement intent; the material catalog and tool choice remain in the
example package. This is an API/content slice, not yet wired to a separate
server-approved network tool ID: `spall_server` still owns its built-in tool
whitelist. Renderer, transport, and storage internals were not changed.

Recommended survival-content backlog, in dependency order: (1) define the
versioned game rules/tool catalog and connect server-authorized tool IDs to
game-owned rules; (2) finish placement and material interaction examples with
server-side range/permission validation; (3) extend the versioned impact rule
with material-specific damage profiles; (4) add versioned recipe definitions
and crafting transactions; (5) add asset IDs/loading and
persisted content manifests; (6) build survival inventory, gathering,
crafting, and progression scenarios. Keep these as separate content work; do
not fold UI/editor work into T25.

**Increment 2 (versioned server action catalog):** `spall_server::ToolCatalog`
stores validated unique tool IDs, an explicit catalog version, allowed edit
kind/material, maximum brush radius, and aim reach. The ordinary `serve`
entry point retains its legacy catalog; `serve_with_catalog` accepts the
game-owned catalog. `sandbox-server` now supplies `sandbox_game::tool_catalog`
with stable IDs for dig, stone, wood, and dirt placement. The server continues
to derive hits and edit centers from authoritative geometry and rejects a
place rule whose material is not registered in that world's manifest. Dirt
is available in the current built-in manifest and is the immediately usable
placement example; wood requires the later game-manifest/world-recovery wiring.
The rules version is logged at startup, but is not yet negotiated in the client
  handshake; clients with unknown/mismatched IDs receive normal action rejection.
  No renderer, transport, or storage internals changed.

  **Increment 3 (sandbox content identity and recovery):** The sandbox server
  now creates fresh worlds and restores saved worlds with the sandbox material
  manifest, including the established playground palette, and replay uses that
  same manifest. Headless and interactive sandbox clients pass the matching
  manifest into handshake validation; the advertised content hash now comes
  from the canonical material manifest instead of a fixed engine tag. Existing
  engine-only entry points retain the built-in stone manifest. This makes the
  wood placement rule valid in sandbox worlds and prevents clients with a
    different material catalog from joining. The action catalog version is still
    logged but is not separately negotiated by the handshake.

  **Increment 4 (sandbox client tool selection):** `spall_client::tool_request`
  builds an action request with an explicit tool ID and operation; the existing
  `cut_request` remains a compatibility wrapper for tool 0. `sandbox-client`
  accepts `--tool dig|place-stone|place-wood|place-dirt` and writes the matching
  stable game tool ID and action into scripted action schedules. The server
  still resolves each ID through its catalog and checks action type, reach,
  radius, target, and material availability before creating an edit intent.
  This adds request selection, not a UI hotbar or live mouse aiming.

  **Increment 5 (clean-checkout gate reproducibility audit):** Ran the
  documented scenarios from an isolated clean worktree at commit
  `e87ceac7fd3e089328c22b21765321d7275d1764`, with raw evidence retained under
  `.local/runs/t25-gates/`. G1 passed for `g1-networked-destruction`, its 2%
  loss variant (31 out-of-order motion snapshots), `g1-full-envelope`, and
  `body-rest-on-structure`. G3 `t23-g3-traversal` passed with replay, recovery,
  and reconnect hashes matching. The eight-client release-profile
  `t23-g4-workload` passed with 4,352 bodies, 20 committed edits, replay, cold
  restart, and reconnect at the same hash. G2 `g2-motion`, `g2-frames`,
  `g2-loop`, `g2-terrain`, and `g2-collapse` all ran on RTX 4080 SUPER / DX12;
  motion/terrain/collapse quality flags were empty, the persistent loop met its
  client-frame target in all four scenes, and the cold full-cache frame-cost
  capture remained above its explicitly provisional GPU target (as documented).
  The `t23-g3` collapse scenario reproduced but did not pass its
  `require_body_settled` gate: server/client/replay/recovery/reconnect hashes
  all matched, but one detached body was still marked awake, so
  `all_hashes_match` was false for the overall requirement summary. This is an
  existing T23 evidence gap, not hidden as a T25 pass. The full-duration G4
  soak, remaining impaired-join variants, and cross-GPU visual review were not
  run; T23's docs already keep those measurements open.

  **Increment 6 (game-owned impact-damage policy):** Added a versioned
  sandbox-owned `IMPACT_DAMAGE_RULES` configuration and passed it to the
  existing authoritative `Simulation::apply_contact_damage` path when
  `sandbox-server --contact-damage` is enabled. The server logs the game damage
  rules version. The engine still resolves collisions, caps work, creates
  normal cut intents, and commits the resulting transactions. No material-
  specific resistance table or client authority was introduced; entity spawn
  rules remain open.

  **Increment 7 (game-owned spawn rule):** Added
  `sandbox::game::spawn_demo_wood_crate`, which constructs a stable game-owned
  wood volume and delegates entity/physics allocation to
  `SimWorld::spawn_body`. `sandbox-server --spawn-wood-crate` invokes the new
  server setup hook only for a fresh world, before publishing its first
  checkpoint. Existing saves restore without adding a duplicate crate. The
  hook is game-provided but executes on the authoritative simulation thread;
  clients cannot request arbitrary body spawns.

  **Increment 8 (material-specific impact profiles):** Contact events now carry
  the material sampled just inside the struck voxel surface. The authoritative
  simulation resolves terrain and body-local contacts against their respective
  volumes and drops events whose material cannot be read. `ContactDamagePolicy`
  accepts game-provided per-material threshold, brush-radius, and detachment
  profiles while retaining shared cooldown and per-tick safety caps. The
  sandbox supplies distinct stone, dirt, and wood values, increments its damage
  rules version, and passes profiles through the server's game policy setup.
  **Checks:** `cargo fmt --all`; `cargo check -p spall_sim --all-targets
  --all-features`; `cargo check -p spall_server --all-targets --all-features`;
  `cargo check -p sandbox --bins --all-features`; `git diff --check` passed.
  No tests were run. Values are initial sandbox tuning, not measured balance.
  Next T25 item: versioned recipe definitions and authoritative crafting
  transactions; then asset IDs/loading and progression examples.

  **Increment 9 (versioned recipes and crafting transactions):** The sandbox
  now defines stable item and recipe IDs plus an immutable recipe catalog with
  an explicit catalog version. `RecipeCatalog::stage` validates catalog and
  inventory revisions, scales ingredient/output quantities with checked
  arithmetic, and builds a replacement inventory without mutating the source.
  `Inventory::commit` applies that transaction only against its original
  revision, so stale requests cannot double-spend; `game::craft` provides the
  stage-and-commit path for the authoritative owner. Starter recipes convert
  wood logs to planks and stone chunks to stone blocks, and the server logs the
  catalog version. This is an in-memory game API: no client craft message,
  player inventory ownership/persistence, or UI was added because those
  systems do not yet exist in the assigned sandbox path. **Checks:** `cargo
  fmt --all`; `cargo check -p sandbox --all-targets --all-features`;
  `git diff --check` passed. No tests were run. This increment is followed by
  the asset-loading slice below.

  **Increment 10 (stable content asset manifest and loader):** Added
  `sandbox::content` with stable game-owned `AssetId`s, versioned RON manifests,
  and a canonical BLAKE3 manifest hash over explicit ID/version/path/hash
  fields. Versioned manifests are immutable on write; `AssetStore` loads by ID,
  confines relative paths to the manifest root, bounds file sizes, checks each
  asset's recorded content digest, and verifies the SPVX major version, chunk bounds, required
  chunk structure, decompression lengths, and embedded logical HASH before
  returning file bytes. The editor's project-local asset IDs stay separate;
  the sandbox runtime does not depend on the editor/UI package. The next
  increment connects decoded static voxels to authoritative body spawn and
  negotiates the manifest hash during client connection. **Checks:** `cargo fmt --all`; `cargo check -p
  sandbox --all-targets --all-features`; `git diff --check` passed. No tests
  were run.

  **Increment 11 (asset/network integration and progression scenario):** The
  loader now imports supported static single-root SPVX voxel runs into stable
  material IDs and the sandbox maps those cells into a game-owned rigid body.
  `sandbox-server --content-manifest` verifies all listed assets before
  serving and incorporates the canonical asset manifest hash into the
  handshake; clients pass the same manifest with `--content-manifest`, so a
  different or missing asset catalog fails the existing compatibility check.
  `--spawn-content-asset ID` places an imported asset in a new world. The
  optional `--progression-demo` scenario grants one wood harvest drop to a
  fresh authoritative inventory, crafts four planks using the versioned
  recipe transaction, then places the selected imported asset. Example
  sequence: start the server with `--content-manifest content-v1.ron
  --spawn-content-asset 1 --progression-demo`, then connect either client mode
  with the same `--content-manifest content-v1.ron`. The scenario is a
  deterministic fresh-world integration fixture. At this stage harvest drops
  are scenario-seeded rather than connected to normal committed world edits.
  Animated/assembled/tinted SPVX
  assets remain rejected by the static importer. **Checks:** `cargo fmt
  --all`; `cargo check -p spall_client --all-targets --all-features`; `cargo
  check -p spall_server --all-targets --all-features`; `cargo check -p sandbox
  --bins --all-features`; `git diff --check` passed. No tests were run.

  **Increment 12 (committed harvest drops and player-slot inventories):** The
  staged cut now records per-material cell counts from its immutable input
  snapshot, and the successful `Committed` result carries those counts. The
  server invokes a game-owned callback only after commit, with the initiating
  session and request ID; the sandbox awards wood logs and stone chunks to the
  inventory keyed by that player's stable connection slot. Sixteen removed
  cells yield one item. Crafting remains an in-memory API and is not yet
  exposed as a client protocol request; connection slots are run-local, not
  account identities, and inventories are not persisted. **Checks:**
  `cargo fmt --all`; `cargo check -p spall_sim --all-targets --all-features`;
  `cargo check -p spall_server --all-targets --all-features`; `cargo check -p
  sandbox --bins --all-features` passed. No tests were run. Next: decide the
  durable player identity and inventory storage boundary.

  **Increment 13 (versioned crafting control protocol):** Added progression
  request/response records on new reliable control tags. Requests carry a
  nonzero request ID, recipe catalog version, expected inventory revision, and
  inspect/craft operation; bounded responses carry current revisions, result
  code, and a complete sorted inventory snapshot. The sandbox server routes
  requests through its game-owned catalog and slot-owned inventory handler,
  checks catalog and inventory revisions, and caches recent outcomes per
  player slot to prevent duplicate craft application. Server admission is
  capped at eight requests per slot per tick. Headless and interactive clients
  can send progression records and surface responses; sandbox-client exposes
  `--inspect-inventory` and `--craft RECIPE_ID:BATCH_COUNT` with an explicit
  `--inventory-revision`. **Checks:** `cargo fmt --all`; `cargo check -p
  spall_protocol --all-targets --all-features`; `cargo check -p spall_net
  --all-targets --all-features`; `cargo check -p spall_client --all-targets
  --all-features`; `cargo check -p spall_server --all-targets --all-features`;
  `cargo check -p sandbox --bins --all-features`; `git diff --check` passed.
  No tests were run. Limitation: player slots and inventories are server-run
  local and in-memory; stable account identity and durable storage remain open.

  **Increment 14 (identity/storage boundary review):** Do not persist
  progression under `SlotId` or `SessionId`. `SessionId` changes on reconnect,
  and connection slots are allocated by the live transport and can be reused
  after restart. The current `JoinToken` authenticates membership in a server
  run but is shared by clients; it does not identify an individual player.
  The existing `spall_store` schema belongs to authoritative world saves and
  does not define game-owned player records. Therefore durable inventory work
  is blocked on an explicit authenticated player-principal contract and a
  game-owned identity-to-inventory storage schema. Add that prerequisite
  before associating a reconnecting client with a persisted inventory; do not
  introduce client-asserted IDs or treat a connection slot as ownership.

  **Increment 15 (server-authenticated player principals):** Added a stable
  128-bit `PlayerId` and server-provisioned per-player bearer credentials.
  `ClientHello` sends only the credential; `ServerAccept` returns the
  server-assigned principal, which `Connection` and the authoritative game
  callbacks expose. The sandbox accepts a bounded credential file with one
  `<player-id-hex> <token-hex>` pair per line and keys in-memory inventories
  and duplicate-request ledgers by `PlayerId` in this mode. The legacy shared
  join token remains available for ephemeral sessions and has no stable
  principal. The protocol/ALPN version was bumped to 2 because the auth reply
  changed. This does not add durable inventory storage, account recovery,
  credential revocation, or an external identity provider. **Checks:**
  `cargo fmt --all`; `cargo check -p spall_protocol --all-targets
  --all-features`; `cargo check -p spall_net --all-targets --all-features`;
  `cargo check -p spall_server --all-targets --all-features`; `cargo check -p
  sandbox --bins --all-features`; `git diff --check` passed. No tests were run.

  **Increment 16 (durable per-player progression):** Added a game-owned,
  versioned SQLite store under the sandbox world directory (or
  `--progression-db`) for per-player inventory revisions/stacks and craft
  request receipts, keyed only by authenticated `PlayerId`. It uses WAL plus
  `synchronous=FULL`, a single bounded writer thread, and one transaction for
  each craft's updated inventory, exact replay response, and request cursor.
  The server restores progression on startup; harvest awards are persisted
  idempotently by action request ID. At this increment's baseline, the
  player's database was separate from the authoritative world database; see
  Increment 18 for the durable outbox follow-up. The progression callback
  waits for the writer result, so slow storage can delay a simulation tick.
  **Checks:** `cargo fmt --all`; `cargo check -p sandbox --bins
  --all-features`; `git diff --check` passed. No tests were run.

  **Increment 17 (durable progression invariants):** Added focused SQLite
  behavior tests for player isolation, committed-harvest idempotency, inventory
  recovery after reopening the database, atomic craft/retry replay after
  reopening, and rejection of an old request after its cached response has
  aged out. **Checks:** `cargo fmt --all`; `cargo test -p sandbox --lib
  progression_store::tests -- --nocapture` passed (3 tests);
  `cargo xtask scenario --name t23-g3 --output .local/runs/t25-final-g3`
  passed (7 committed edits, replay, cold restart, and reconnect converged to
  `cac2893c…d18e6de`); `cargo xtask scenario --name
  g1-networked-destruction --loss-percent 0 --output
  .local/runs/t25-final-g1` passed (10 committed edits and replay converged to
  `62164eee…d7a4b`); `cargo xtask scenario --name t23-g4-workload
  --timeout-ms 240000 --output .local/runs/t25-final-g4` passed with all eight
  clients, all 20 edits, replay, cold restart, and late reconnect converging to
  `dafe0e5e…9547afa`; checks for spall_protocol, spall_net, spall_client,
  spall_server and sandbox across all targets/features; `cargo fmt --all
  --check`; and `git diff --check` passed. These scenario runs used the current
  working tree, not a clean checkout. They do not establish atomicity with the
  separate world journal or measure writer latency under server load. The
  checks used the current working tree; the separate clean-checkout audit
  remains documented in Increment 5.

  **Increment 18 (review follow-up: contact normals and harvest durability):**
  Contact material sampling now orients the physics pair normal from the
  selected target toward its striker for both terrain/body and body/body
  contacts; a pair-order regression test covers either target slot. For
  authenticated harvests with world persistence enabled, sandbox emits a
  versioned reward event into the world's durable outbox. The journal row and
  outbox row share one SQLite transaction; startup and live delivery apply the
  event to the progression database using its existing player/request
  idempotency key and acknowledge it only after success. A crash before commit
  leaves neither record, while a crash after commit or before acknowledgement
  replays without losing or duplicating inventory. Ephemeral runs retain their
  in-memory behavior and have no crash recovery promise. Exact checks and any
  remaining integration limitations are recorded in the session work log.

  **Increment 19 (bounded asynchronous progression and outbox delivery):**
  progression requests, committed-cut callbacks, and live harvest outbox
  delivery now execute on dedicated bounded worker queues. The request worker
  is a single FIFO, so operations for each player stay serialized; the sim
  thread enqueues with `try_send`, drains a bounded completion slice, and emits
  craft replies only after the handler's durable transaction completes. A full
  request queue returns the new explicit `RetryableCapacity` rejection and
  clients must retry the same request ID. The world outbox remains unacknowledged
  until the award worker reports success; only then does the sim submit the
  world-database acknowledgement. Recovery and clean shutdown may wait outside
  the active simulation tick. Wire schema, handshake protocol, and ALPN advance
  to version 3 for the new rejection code. **Checks:** queue saturation/order
  test with an injected 75 ms slow handler (enqueue stays below 50 ms; 64 queued
  plus one active accepted, next rejected); `cargo test -p spall_protocol --lib`
  (33 passed); eight-client `t23-g4-workload` (3,000 ticks, 20 committed edits,
  replay/restart/reconnect hashes converged to `dafe0e5e…9547afa`). Measured
  server tick busy p95 0.9893 ms / p99 2.7976 ms against 12 / 16.7 ms targets;
  physics p95 0.2251 ms; process peak 2,732,171,264 bytes. The slow handler is
  a deterministic queue-level injected delay, not an instrumented SQLite stall
  inside the eight-client scenario. No claim is made for full G4 soak.

  **Increment 20 (credential lifecycle):** Added
  [credential operations](credential-operations.md) for protected server/client
  secret files, provisioning, rotation with the same PlayerId, and revocation.
  The server polls the registry every 500 ms; an atomic valid update replaces
  the accepted credentials and closes active sessions, while malformed or
  unreadable content fails closed by revoking all credentials. The stable
  PlayerId remains independent from the bearer token, so progression ownership
  survives rotation and revocation. Tests verify rotated-token acceptance,
  old-token rejection, revocation, stable identity, redacted Debug output, and
  parser errors that do not echo token bytes. **Checks:** `cargo check -p
  spall_net --all-targets --all-features`; `cargo check -p spall_server --lib
  --all-features`; `cargo check -p sandbox --bins --all-features`; focused
  `spall_net` rotation/replacement tests and `spall_server` parser-redaction
  test passed. Windows ACL commands are documented but were not exercised on
  this run.

  **Increment 21 (authenticated network crafting recovery):** Added a real
  QUIC integration test with two credential-authenticated players and the
  sandbox's durable progression database. It sends duplicate craft IDs and
  verifies identical recorded replies, rejects a stale inventory revision,
  isolates a player without ingredients, disconnects after admission but
  before an injected slow durable reply, reconnects with the same request ID,
  restarts the server and reopens the progression database, then verifies
  exact inventory contents/revisions. The same test rotates one token while
  retaining its PlayerId and revokes another, proving old tokens cannot
  reconnect and that token strings do not appear in JSONL logs. **Check:**
  `cargo test -p sandbox --all-features --test progression_network` passed
  (1 integration scenario; ~20 s).

  Remaining ordered completion checks and audit:
  (1) the supplied acceptance key is `(WorldId, TransactionId)`, while the
  current harvest store deduplicates `(PlayerId, RequestId)`; see
  [T25 remaining checks](reports/T25-remaining-checks.md). (5) no authored
  sandbox asset/manifest fixture is checked in, so assembly/animation/tint
  runtime expansion cannot yet be validated against game-authored meaning;
  SPVX v1.1 already specifies those format semantics and unsupported runtime
  forms fail explicitly. (6) D3D12 and Vulkan G2 still/motion captures have
  been run on the RTX 4080 SUPER, with Vulkan output recorded, but a distinct
  physical adapter remains unavailable/unrun. See the report for exact
  measurements and limits.

### ENG-74 — Editor MVP (user-authorized follow-up)

Original completion used egui; the UI toolkit was migrated to Yakui in ENG-90.

Dependencies: current engine workspace. Own: `tools/spall_editor`, editor-owned RON schemas, and editor documentation. This is intentionally outside the G0–G5 engine gate sequence and must not alter those gate claims.

Build a separate native editor crate using Rust, winit, wgpu, and Yakui. It may depend on Spall rendering/voxel libraries, but no engine/runtime crate may depend on it or on Yakui. Add versioned, human-readable RON project and scene documents, plus portable `.spvox` voxel assets according to `docs/spvox-format.md`. Project/scene references must use a stable `AssetId`, never an authored absolute or raw asset path.

Implement an AssetDatabase and an EditorCommand-based undo/redo layer before UI mutations. The MVP launcher supports recent/open/new projects. Keep **Scenes** (placed objects and transforms) distinct from **Assets** (reusable authored voxel objects): the scene view has a collapsible hierarchy/inspector and a collapsible bottom toolbox that searches named assets, previews them, and places an asset as a new scene object. The asset view supports single-cell and bounded box painting/removal, selected material and RGB color, deterministic per-cell color jitter within a selected margin, and save. Keep advanced docking, procedural tools, animation, material authoring, and engine-management UI out of scope.

Accept: commands are the sole mutation route and undo/redo restores entity and voxel edits; saving/reloading preserves stable references; a created voxel asset can be assigned to an entity and persisted with a scene; the editor opens as a native window and can launch the real sandbox runtime. CPU tests cover document round-trips and command invariants; graphical interaction is a separate hands-on check.

### ENG-89 — wgpu 30 and Yakui HUD migration (user-authorized follow-up)

Dependencies: current renderer and ENG-74 editor. Own: workspace GPU/UI dependency pins, `spall_render`, `spall_client`, `tools/spall_editor`, the local rendering proof of concept, and dependency/validation documentation. Keep Yakui crates on one exact upstream revision and keep editor UI crates compatible with the workspace wgpu types.

Port instance/device/surface setup, pipelines, passes, polling, shader tooling, editor integration, and local proof of concept to wgpu 30. Integrate Yakui into the existing live client device and frame, including input/DPI/resize handling and CPU/GPU HUD timing where the adapter supports timestamps. Preserve rendering across surface loss and resize. Do not use Yakui's standalone application window as the client integration.

Accept: `cargo tree -d` shows one wgpu/Naga line for application rendering; `cargo xtask check`, `cargo xtask smoke --graphical`, the G2 captures, the release G1 bounded run, and `cargo run -p spall_editor` complete. Inspect captures and the live HUD on D3D12 and Vulkan hardware; record frame/HUD CPU/GPU measurements against the pre-migration baseline. Hardware checks unavailable in the current environment must be recorded as unrun rather than inferred from compilation.

### ENG-90 — Editor Yakui migration (user-authorized follow-up)

Dependencies: ENG-74 and ENG-89. Port the complete editor launcher, scene and
asset workspaces, hierarchy, inspector, toolbox, voxel painting controls, and
viewport interaction to Yakui using the pinned workspace revision and the
existing wgpu 30 surface/device. Preserve editor commands as the sole document
mutation route, including undo/redo. Remove editor egui dependencies, update
dependency and validation records, and inspect native interaction on hardware.
Accept: `cargo check -p spall_editor --all-targets --all-features`, editor CPU
tests, and `cargo run -p spall_editor` pass; no egui references remain in the
workspace dependency graph; scene and voxel editing behavior matches ENG-74.

### ENG-91 — Scene and built-in asset workspace layout

Dependencies: ENG-74 and ENG-90. Own: `tools/spall_editor`, the built-in voxel
asset catalog in `crates/spall_voxel`, and editor/asset documentation. Add File,
Edit, and Help menus; a center scene/asset view; and collapsible, user-resizable
left, right, and bottom panels. The viewport fills the space between side
panels; the bottom panel always spans the workspace width. Keep the hierarchy,
inspector, asset search, preview, painting, and EditorCommand-only project edits
available in this layout. The starting screen opens Engine Assets directly
without opening a scene. In the Assets workspace, keep the live 3D viewport in
the center, voxel authoring controls in the right panel, and a searchable grid of
small-preview assets across the resizable full-width bottom panel. Provide
viewport cursor feedback and selectable lighting environments. Ship immutable built-in terrain assets under
`crates/spall_voxel/assets/builtin/voxel/`, expose their canonical SPVX bytes
through `spall_voxel` for generation consumers, and make the editor catalog
searchable and previewable. Editing a built-in imports an undoable project copy
under that project's `AssetDatabase`; it never overwrites the engine bundle.
The initial catalog contains `palm_tree.spvox` and `weeping_willow.spvox`.
Use readable, padded buttons and align row descriptions left with their
actions on the right; labels and controls must remain readable at the default
window size and when the side panels are resized.

Accept: both built-ins decode with their stable portable material keys; the
editor can browse/search/preview them and make editable project copies; the
copy saves/reloads using a project `AssetId` and leaves bundled source bytes
unchanged; scene and asset workspaces expose the three resize/collapse panels;
`cargo test -p spall_voxel` and editor checks pass. Native drag/layout behavior
is recorded as a hands-on check when a desktop session is available.

### ENG-92 — SPVX 1.1 authoring layers

User-authorized follow-up to ENG-91. Preserve the current editor work while
extending static SPVX assets with optional editable layers. The canonical v1.1
`LAYR` chunk stores stable layer IDs, order, names, visibility, and source cells;
visible layers compose deterministically to the authoritative `VOXL` stream.
Layer source is distinct from `PART` assembly and never enters world saves or
network records. The editor saves, loads, selects, edits, and hides layers through
undoable commands. Flat v1.0 assets remain readable and can be converted when a
layer is added. The sandbox's static importer validates the optional feature
marker and whole-file hash and continues to consume `VOXL` only. Update format
and validation documentation without changing engine/editor dependency direction.

Accept: layered SPVX round-trips retain hidden and overlapping source cells;
the editor rejects a source/`VOXL` mismatch; layer edits and visibility undo
exactly; the static runtime loads a valid layered asset; editor and sandbox
checks pass. Record native layer interaction separately when a desktop session
is available.

### ENG-94..99 — Natural-lighting renderer programme (R1–R6)

User-authorized 2026-09-26: a Teardown-inspired lighting direction shared by the
editor and the game, rasterised visibility plus software voxel tracing for
indirect light, with hardware ray tracing only as a separately-evaluated option.
One bounded ticket per pass; `docs/reports/ENG-94.md` holds the audit that
scoped it (what ran in the interactive game versus captures/editor).

- **ENG-94 R1** — game window on the shared `spall_render` direct-light
  pipeline (materials from the manifest, shadows for terrain and bodies, HDR,
  tone map, resident buffers, debug views). First pass done; see the report.
- **ENG-95 R2** — authored tint through edits, bodies, replication and
  persistence; manifest-palette linearisation decision (changes the manifest
  hash). Depends on R1. Implemented as material variants with explicit,
  append-only variant ids (frozen v3 rows + generated extensions), an additive
  manifest extension that refuses recolouring, and real client/late-join
  evidence; validated on this machine, **not accepted** (release measurements and
  editor/runtime parity metrics are ENG-99). See `docs/reports/ENG-95.md`.
- **ENG-96 R3** — visibility-aware skylight, dark enclosed rooms, documented
  shadow technique, geometry-submission decision. Depends on R1. First pass
  done; see `docs/reports/ENG-96.md` (PCSS shadows, sky-visibility occupancy in
  the game and editor, measured cube-vs-mesh decision). Enclosed rooms are black
  until R4 adds bounce.
- **ENG-97 R4** — the T13/T14 lighting cache in the interactive runtime.
  Depends on R1, R3. The frozen cache surface changes only with new evidence.
  First pass done (terrain, lit one-bounce sources, directional radiance, thin-wall
  evaluation); see `docs/reports/ENG-97.md`. Bodies, dirty regions, scroll,
  temporal and streamed boundaries are ENG-101 (R4b).
- **ENG-100 R7** — move terrain (and body templates) from instanced cubes to
  incremental greedy meshes (measured 23x fewer triangles, ~8x cheaper frame,
  baked AO); depends on R3's evidence. First pass done; see
  `docs/reports/ENG-100.md` (incremental and edit-to-mesh latency still need a
  live measurement).
- **ENG-101 R4b** — bodies as occluders/bounce sources, incremental sweeps.
  First pass done; see `docs/reports/ENG-101.md`.
- **ENG-102 R4c** — hitch-free scroll (double-buffered re-centre), low-angle
  sampling, emissive manifest content, correlated edit-to-presented latency
  instrumentation (real network + real renderer, one clock; commit → receipt →
  rebuild → upload → sweep → presented), body-motion latency measured
  separately. Debug-build only; release measurement and the streamed-world run
  remain, and the dirty-bounds cull was deliberately not pursued (§2d). See
  `docs/reports/ENG-102.md`.
- **ENG-98 R5** — optional hardware ray tracing feasibility report. Independent.
- **ENG-99 R6** — acceptance evidence and metrics. Depends on R1–R4.

## Assignment template

```text
Implement task Txx from docs/tasks.md.
Read AGENTS.md and the four linked specification documents first.
Dependencies already completed: [IDs + evidence].
Owned paths: [task paths]. Shared interfaces: [schema/module names].
Required outputs: [task deliverables].
Acceptance scenarios/checks: [exact fixture/command names].
Do not change: [relevant frozen contracts].
If a prerequisite or feasibility claim fails, show a minimal reproduction,
finish independent work inside this assignment, and report the exact blocker.
Return changed files, commands/results, evidence paths, and remaining risks.
```

Work that can proceed independently after prerequisites: T09 alongside CPU geometry work; T12/T13 alongside replication integration; T16 alongside the graphics gate. This is a dependency observation, not a request to launch agents automatically. One integrator owns shared schemas and final gates.

### ENG-103 — Water feasibility prototype: dam, canal, and flooded tunnel

User-authorized follow-up scoped by `docs/water-agent-handoff.md`. This is the
first bounded water assignment, not the complete water feature roadmap.

**Dependencies:** Existing voxel and simulation foundations. First audit
`spall_voxel`, simulation tick ownership, physics, jobs, and validation seams;
do not assume a water API exists. **Owns:** a CPU-only `spall_*` fluid
subsystem and its bounded fixture, meaningful invariant/scenario tests,
reproducible metrics, solver decision, and evidence report. Compare custom
sparse 3D grid and Salva capabilities/compatibility/licensing without changing
shared dependency versions. Use actual voxel boundary data, a finite pair of
reservoirs, editable dam, excavatable canal/tunnel, and one water type.

Keep water state separate from solid occupancy and ECS entities. Preserve
Rapier as the only rigid-body solver; no GPU/window/network dependency enters
the fluid algorithm. Account for initial water, explicit sources/sinks and
boundary outflow across flow and geometry edits. Unknown residency must never
be silently treated as air, a drain, or a wall. Specify committed-edit and
fluid tick ordering, fixed-tick substeps/stability limits, bounded overload
behavior, and pressure-region consistency before integrated use.

**Required evidence:** stable water and no leakage through intact walls,
including a brick seam; canal transfer toward equilibrium and closure without
water loss; a moving dam-break surge with downstream accumulation; a flooded
tunnel under a second pool; absolute/relative accounting error and explicit
outflow; fluid/total step timings, active cells, memory, pressure convergence,
larger workload response, and a resolution or timestep sensitivity comparison.
Record dimensions, water resolution, timestep/substeps, initial volume,
boundaries, hardware, and numerical/performance targets before measuring.
Targets remain targets until measured. Deliver exact commands and limitations;
do not shrink the named workload or substitute visuals when a feasibility
target fails. Current prototype measurements and failed gates:
`docs/reports/ENG-103.md`.

**Grid comparison increment (user-directed, 2026-09-27):** Preserve the Salva
backend and its raw evidence. Add a separate fully resident, dense CPU 3D MAC
grid with one water cell per terrain cell and fractional VOF state. The
pre-coding numerical method, equations, units, boundary conditions, step order,
and deliberate simplifications are recorded in
`docs/reports/ENG-103-grid-method.md`. The acceptance extension is the full
grid-prototype brief recorded in Loopira ENG-103: bounded conservative
fraction transport, staggered velocity/pressure projection, explicit
disconnected-region/nullspace handling, staged voxel boundaries, fixed outer
tick with bounded stability substeps, analytical projection/hydrostatic check,
and the listed physical fixtures, sensitivity runs, and base/scale-2 evidence.
This static aligned-grid experiment does not establish support for coarse
fluid cells, moving/rotating hulls, or production integration.

  **Bounded correctness/profile follow-up (user-directed, 2026-09-27):** Correct
the speed and face-weighted kinetic/potential energy diagnostics; reconcile the
upper-pool region from the actually applied FCT face fluxes; and keep the scale-2
pressure solve profile. Pressure matrix and Krylov work now iterate wet cells
only. Equal-duration basin runs compare 0.25 m/60 Hz, 0.25 m/120 Hz, and
0.125 m/60 Hz; equal-duration tunnel runs compare both spatial resolutions.
The scale-2 pool discrepancy was caused by a region mask omitting the first
cavity row and now balances to 1.8e-15 m³. Base sealed/canal/breach fit the
  proposed 2 ms allocation after the bounded optimization. **Pressure-cost and
  residual-motion follow-up (user-directed, 2026-09-27):** Correct the harness's
  per-1/60-second normalization (half-dt work is nearly 2x, not cheaper), add
  detailed PCG residual/component/timing traces, and remove redundant
  enclosed-nullspace projection passes. Repeated scale-2 tunnel pressure cost
  falls from 52.05 to 36.80 ms/tick with the same iterations; the complete
  workload remains about 24x above the 2 ms proposal. The basin maximum is
  retained at 0.854 m/s but occurs in a fraction 3.16e-18 interface cell; the
  volume-weighted p95 is 0.075 m/s. Neither finding clears the failed
  unfiltered speed gate. Keep ENG-103 in progress and production integration
  blocked; the next bounded comparison is IC(0) preconditioning, including
  setup cost and enclosed-component nullspaces. See
  `docs/reports/ENG-103.md`, `docs/reports/ENG-103-grid-method.md`, and the
  uniquely named evidence captures.

**IC(0) comparison and corrected basin diagnostic (2026-09-27):** Add a
selectable deterministic zero-fill incomplete-Cholesky PCG preconditioner while
preserving Jacobi as the default baseline. Rebuild and measure the factor at
every pressure projection; use a factor-only pinned gauge for enclosed
components; reject bad pivots explicitly. The matched base basin/canal/breach
and scale-2 tunnel fixtures preserve pressure convergence and flow results.
Three release timing repeats show normalized mean-cost medians of Jacobi→IC(0):
basin 1.941→1.870 ms, canal 1.854→1.748 ms, breach 1.820→1.742 ms, scale-2
tunnel 49.002→37.598 ms. IC(0) cuts scale-2 pressure iterations from 229.6 to
86.2 mean while adding factor storage and ~3 MiB peak process memory. Revise the
basin gate to occupancy-weighted p95/high-speed share at C>=1e-3, retain the
raw maximum, and publish 0 through 1e-2 threshold sensitivity. This clears the
speed-only interpretation but finds that upper-bound basin kinetic-energy rise
still misses by 0.101 J. Tiny fractions also cause 7 extra diagnostic-estimated
CFL substeps in the basin and remain pressure rows; solver filtering was not
applied. Recommendation: keep IC(0) for further isolated candidate work and
Jacobi as the reproducible baseline. Scale 2 remains 18.8x over the proposed
2 ms allocation; the Salva tunnel roof crossing remains. Production promotion,
moving-body force coupling, boats, and networking stay blocked. See
`docs/reports/ENG-103.md`, `docs/reports/ENG-103-grid-method.md`,
`docs/validation.md`, and the `ic0-final-*`/`ic0-plain-*` JSONL captures.

**Two-phase candidate, reference comparison, cost, and sealed air
(2026-09-27):** The opt-in two-phase variable-density MAC/PLIC model
(`--ambient-density 1.2`) passes the 30-second physical acceptance. The
acceptance output is now v2. The level `equilibrium` basin keeps the at-rest
KE gate. The historical `basin` is a small dam break, so it gates on total
mechanical energy never exceeding its initial value. The legacy single-phase
closure fails because it genuinely gains 15–20 kJ.

An independent Basilisk C reference (the `reference-*.c` sources and
`basilisk-*.jsonl` output in `docs/reports/ENG-103-evidence/`) agrees on
conservation, energy released, dissipation, and final level. A new
standing-wave benchmark shows resolved waves are not over-damped: 0.991
amplitude ratio per period against Basilisk's 0.959. The remaining basin KE
gap is under-resolved collapse. MacCormack advection was rejected because it
grows waves 4.6–11% per period.

Cost: a Galerkin-aggregation multigrid PCG (`--preconditioner mg`), exact
transport savings, and rayon data-parallel loops that stay bit-identical for
any thread count. Every scale-1 fixture now costs 1.1–1.3 ms per 1/60 s; the
closed scale-2 tunnel costs 19 ms.

Sealed air is an isothermal compressible gas by default. A diving-bell test
lands within 7% of the analytic equilibrium.

Recommendation: adopt the two-phase MAC grid as the water solver. Production
integration (tick ordering with committed edits, overlap/displacement policy,
replication, persistence, dormancy) is the next scoped assignment. See the
dated sections of `docs/reports/ENG-103.md`.

The Salva particle backend was removed after selection. The grid scenarios now
build their scene and bit-identical reference volume in
`fixtures::ReservoirScene`. Production integration is tracked as ENG-105.

### ENG-105 — Authoritative water integration

Integration now includes owner-thread boundary displacement and validated
immutable worker results, the user-approved trapped-volume ledger for sealed
placements, changed-brick presentation deltas, budgeted full repair and late
join, exact checkpoint/journal amounts with reset-to-rest recovery, quiet-water
sleep and conservative residency suspension, and several independently bounded
pressure domains. Connected water stays within one authored domain; exchange
between streaming grids is a later scaling extension.

Evidence, exact checks, sustained timings/bandwidth and remaining limits are in
[`docs/reports/ENG-105.md`](reports/ENG-105.md). Earlier partial implementation
reports are historical: [`ENG-105-increment-1`](reports/ENG-105-increment-1.md)
and [`replication validation`](reports/ENG-105-replication-validation.md).

Rigid-body coupling, swimming, boats, sailing and appearance remain separate
assignments. The measured three-reservoir fluid cost exceeds the earlier 2 ms
proposal; this integration does not resolve the larger ENG-103 pressure-cost
limits or establish a whole-game/GPU performance gate.

### ENG-104 — Interactive sandbox water demo

User-requested local visualization follow-up to ENG-103. Add a dedicated mode
to `sandbox-client` using the shared Spall renderer and the bounded,
voxel-backed water fixture. Show the actual solid walls/dam and live fluid
particle positions. Provide direct controls for canal excavation, dam breach,
pause/resume, scene reset, and camera movement. Keep this explicit
client-local debug mode separate from normal network replication: it does not
establish server-authoritative water, multiplayer water state, or completion of
ENG-103's failed feasibility gates. Launch instructions and implementation
evidence are recorded in `docs/reports/ENG-104.md`; tracked in Loopira ENG-104.
The original particle view (`--fluid-demo`) was removed with the Salva backend;
the MAC grid viewer (`--grid-fluid-demo`) is the remaining water view.


## ENG-116 - Ecology A: deterministic grass and trees

Build `spall_ecology` as a CPU-only engine library over immutable terrain snapshots. Keep the vegetation plan separate from `spall_worldgen` output and keep initial species/material choices in `sandbox`. Use versioned, bounded, canonical state for grass patches, plants, persistent branch skeletons, and regional seed availability. Persist ecological time, interval remainder, random progress, and pending work in the ecology encoding; world checkpoint and journal integration is a later task.

Implement terrain-validated initial placement, grass harvest/regrowth, staged tree growth, deterministic seed dispersal/expiry/germination, crowding, and unsuitable-condition behavior. Use only authored soil suitability, explicit moisture snapshots, CPU sky exposure, and spacing in this increment. Generate incremental connected wood proposals from retained branch records. Cut branches stay removed; root destruction stops growth/reproduction; detached geometry stays under existing voxel destruction ownership.

Read dependencies carry exact brick revisions plus the ecology input revision. The caller applies proposals at an owner-thread boundary and acknowledges only accepted edits; rejection and staleness leave committed geometry progress unchanged. Bound and fairly resume work; Unknown suspends decisions and an unloaded region pauses ecological time. Add a real-terrain headless clearing example and measurements. Production server ownership, durable checkpoint/journal integration, and vegetation rendering are explicitly deferred.

Validation: `cargo test -p spall_ecology`, `cargo run -p sandbox --example ecology-clearing`, `cargo run -p sandbox --example ecology-clearing -- --large`, and `cargo xtask check`. See [ENG-116 evidence](reports/ENG-116.md). ENG-116 does not close ENG-113 or ENG-114; those Loopira tickets remain `in_progress` despite the implementation notes in `docs/worldgen.md`.
## ENG-117 - Ecology B: interactive growth showcase

Add a client-local rendered showcase for ENG-116's grass/tree lifecycle. Keep the ecology clock independently controllable: pause/resume, one-interval stepping, and bounded speed presets. Make accepted tree growth visible in the actual sandbox voxel renderer; show seeds, seedlings and the grass biomass patch; allow grass harvest, branch cutting, and root destruction through the showcase. It is a private local Simulation fixture, not a production world, server persistence, or network feature.

Validation: `cargo test -p spall_client ecology_demo::tests`, `cargo check -p sandbox --features client --bin sandbox-client`, and `cargo xtask check`. Manual GPU presentation is separate from CPU checks. See `docs/validation.md` for the command and keys.

### ENG-117 correction pass — 2026-10-01

User-requested audit after viewing the interactive result: replace the misleading
thin generated-terrain crop with a completely resident 16 m clearing; show every
live terrain/body cell through the shared renderer; ground seeds, seedlings and
grass on current voxel surfaces; reject germination through rock roofs; use
horizontal crowding; require committed growth before maturity; improve new
connected tree skeletons and attach soft foliage to live growth tips. Correct
side-branch cutting, root destruction and partial single-cell growth acknowledgment.
Bump ecology plan version to 2 without regenerating persisted skeletons or changing
terrain generation/manifest versions. Capture seedling, juvenile, mature,
dispersal and damage states using the interactive scene's actual geometry builder.
Production server/network/persistence integration remains a separate assignment.
See `docs/reports/ENG-117-corrections.md` for checks and visual evidence.


## ENG-118 - Seasonal vegetation in generated worlds

User-requested production follow-up to completed ENG-116/117. Define ten procedural trees and ten grasses/ground plants in sandbox, mapped to Meadow/Alpine/Swamp/Desert. Seed generated playable worlds through the common worldgen scene factory, including editor Run in game. Retain terrain-validated placement/dispersal, connected wood growth through server-authoritative edits, bounded fair work and stable IDs. Add plant-only seasons, autumn colour and deciduous winter leaf loss. Persist complete state and clock atomically and replicate soft plants to all clients and late joiners. Base terrain version 3 stays unchanged; vegetation owns its own version-1 schema. Earlier saves are not regenerated. ENG-113/114's broader statuses/performance gates remain separate; their available worldgen implementation is the input to this increment.

Validation: catalogue/biome/grounding/offspring tests, four-season geometry, committed simulation growth, exact checkpoint/journal recovery, two network clients, legacy save upgrade/recovery, eight GPU specimen galleries plus actual generated-world captures at 512/default 1024 cells, cargo xtask check. Rules and measured evidence: docs/reports/ENG-118-vegetation.md.


### ENG-118 loading and walking correction pass � 2026-10-01

User reported ground/trunks arriving minutes after foliage, jerking during walking,
and roughly 50 FPS. Exercise the actual live greedy terrain path, publish nearby
mesh batches immediately, cache empty/enclosed results with complete halo revision
validation, and preserve voxel/AO/unknown semantics while reducing meshing lookup
cost. Move soft-plant generation off the render thread, upload only changed
instances, omit enclosed foliage cells and compact adjacent quarter-metre cells
without changing their occupied surface. Cull terrain against each camera/shadow
pass separately. Gate soft plants on resident supporting meshes and smooth the
camera's response to applied prediction corrections without altering authority,
collision/replay, or raw correction telemetry. Add bounded real-input traversal
capture support and regression tests. Full-world performance gates remain open.
See `reports/ENG-118-loading-performance.md` for measurements and checks.

## ENG-120 - Water domain growth

Opt-in authoritative growth after committed nearby terrain edits. Preserve
exact amounts, trapped ledger, sources and frame continuity; validate residency,
overlap and caps. Velocity/pressure restart at rest. Replace superseded client
regions and recover grown dimensions. Implemented on main through PR182;
interactive acceptance remains open. See reports/ENG-120.md and ENG-121 for
subsequent capture/cap corrections and adaptive growth budget.

## ENG-121 - Adaptive water resolution and bounded active work

Choose the finest generated-world grid fitting 50,000 fluid cells, restrict
transport limiting to active cells, capture boundaries brick-wise, and avoid
duplicate capture/repeated cap-limited growth. Looser tolerance, CFL/dt changes
and surface-only sleep were rejected. Thin-wall leaks and dropped seed volume
are reported limitations. See reports/ENG-121.md for 512/1024 measurements and
reports/ENG-121-rest-state.md for integration checks and rest-state diagnosis.

## ENG-122 - Preserve submerged solid capacity during water coarsening

Dependencies: implemented ENG-105/120/121 interfaces. First design/test a
conservative coarse solid-capacity and face-boundary representation. Unaligned
submerged stairs create water/air interfaces in omitted solid space and drive
currents, while resolved stairs remain still. Preserve volume, thin walls and
openings, displacement/trapped water, sealed air and durable recovery. Make
wire/save contract changes explicit before integration. Validate hydrostatic
rest and real trench/dam-break response, then measure the same generated
512/1024 release workload. No sleep heuristic that stalls flow, body coupling
or GPU solver. Evidence: reports/ENG-121-rest-state.md.

### ENG-122 increment 1 - exact coarse open-space geometry (2026-10-02)

Experimental cut_cell geometry retains every six-connected open component within
coarse cells, integer solid-excluding capacity, vertical layer counts and matched
open fine-face portals. Seven geometry tests cover thin internal walls/openings,
misaligned apertures, seed-volume preservation, exact horizontal capacity,
256-pattern connectivity against fine flood fill, and bounded build/input errors.
Actual seed-1 autumn worlds retain all authored water (512: 674 m3; 1024:
2400.546875 m3). Geometry build medians: 8.39 / 39.42 ms; no fluid steps measured.
Integration decision: keep experimental until component-aware pressure/transport,
conservative edit remapping and an explicitly versioned component-amount/ledger
canonical state pass acceptance. Current one-amount-per-coarse-cell state cannot
recover independent spaces. No production solver or save/wire change in this
increment. Full design, checks and remaining risks: reports/ENG-122.md.

ENG-122 increment 2 (2026-10-02): experimental component pressure/donor transport and candidate LE component-amount codec added after integrating merged main 7278376. Complete fluid suite: 67 passed; protocol: 52 existing plus four candidate tests passed; strict affected-package Clippy passed. Both showcase seed-1 autumn generated-world operator gates failed on first transport: residual-scale capacity overshoots, plus artificial rest currents 0.1341 m/s (512) and 1.0236 m/s (1024). Pressure samples 10.60/61.55 ms; zero accepted operator iterations, no production fluid steps. Keep experimental: no production/save/wire activation. See docs/reports/ENG-122.md, ENG-122-operators.jsonl and ENG-122-state-format.md. Next unblocked work is ENG-122 balanced free-surface projection and conservative bounded transport; momentum, sealed air, edit remapping and complete durable recovery remain gates. ENG-122 stays in progress.

ENG-122 increment 3 (2026-10-02): experimental hydrostatic reference split and paired capacity limiter. Fourteen operator tests include one-minute irregular-surface rest at 0.5/0.3 fine-cell fractions, thirty-second unequal-level flow/conservation, saturated throughflow/cycles and atomic bounded-limiter failure. Both actual showcase seed-1 autumn generated worlds advance 1,200/1,200 operator iterations (60 s) with zero measured current, divergence and accounting error. Projection/transport medians 0.218/0.058 ms (512) and 0.588/0.156 ms (1024); these are rest-only operator costs excluding clone/build, not production/trench performance. The long-run sum-then-subtract density roundoff failure and correction are retained with raw evidence. See docs/reports/ENG-122.md and ENG-122-balanced-rest.jsonl. Keep experimental/in_progress: next ENG-122 work is velocity advection and dynamic dam/trench/interface acceptance; sealed air, edits/growth remapping, owner validation and durable recovery remain required before activation.
ENG-122 increment-3 combined-tree verification: cargo xtask check passed formatting, workspace all-targets/all-features strict Clippy and workspace all-features tests: 1123 passed, 0 failed, 63 ignored. Ignored gates are not accepted.

ENG-122 increment 4 (2026-10-02): experimental shared water/momentum donor transport and matched dual-mass pressure projection added; complete fluid suite 82 passed and affected-package strict all-targets/all-features Clippy passed. Both generated 512/1024 momentum rest probes advance 60 s with zero current/accounting error. Six-second 1.5 m channel surge and full-height thin-wall probes pass. Low dam with a dry high opening FAILS: 0.000272616 m3 crosses above-water crest; all 600 steps accepted, conservation holds but physical gate does not. Evidence and exact checks in docs/reports/ENG-122.md and separate momentum JSONL files. Keep experimental/in_progress; no production/save/wire activation. Next ENG-122 prerequisite: phase-aware basin/interface state and wet-face apertures for pools joined through air, then dynamic acceptance, sealed air, edit/remap/trapped ledgers and canonical owner/recovery integration. Full workspace xtask check not rerun for this increment.

ENG-122 increment 5 (2026-10-02): bounded experimental fine phase state retains sub-coarse basin provenance and exposes actual wet face overlap plus directional donor apertures. Standalone prescribed-fine-flux transport conserves water with paired limits and atomic CFL/allocation/fragmentation rejection. Twelve new phase tests; final full fluid suite 94 passed/0 failed, strict affected-package all-targets/all-features Clippy passed. Generated 512/1024 seed accounting preserved exactly; construction medians 6.332/36.550 ms; retained phase arrays 13,722,864/76,076,688 bytes excluding shared geometry/scratch. Zero coupled steps claimed. Original coupled low-dam 600-step gate rerun and still FAILS at 0.000272616 m3 crossing (exit 1); standalone dry-crest prescribed-air test is not coupled acceptance. SCWA v1 component totals cannot recover the new fine phase placement; format limitation updated explicitly, no save/wire conversion. Keep experimental/in_progress. Next unblocked ENG-122: phase/basin-aware pressure and momentum coupling (including internal basin DOFs), then unchanged dynamic/interface gates, sealed air, edit/remap/trapped ledgers and complete canonical owner/recovery integration. See reports/ENG-122.md and separate phase construction/failure JSONL evidence. Full workspace xtask check and interactive acceptance unrun this increment.

ENG-122 increment 6 (2026-10-02): user-requested packed fine faces and smaller scratch buffers implemented. Derived face record is u32 lower-index/axis; decoded endpoints/order unchanged, explicit 2^30 fine-cell ceiling prevents aliasing. Transport reuses two cell arrays as sums/scales/candidate, retains one transfer array, drops numeric scratch before basin validation. Retained phase arrays 512: 13,722,864 -> 7,240,744 bytes; 1024: 76,076,688 -> 32,314,648 bytes. Active-limiter numeric scratch 512: 32,314,448 -> 14,481,488; 1024: 135,316,016 -> 64,629,296 bytes (old scratch baseline calculated from exact old arrays, new capacities measured; excludes caller/geometry/basin memory). Fifteen phase tests, including 512 bit-identical comparisons to original transport and matched atomic 108-cell limiter exhaustion; final full fluid suite 98 passed/0 failed, strict affected-package Clippy and formatting passed. Both generated-world memory probes preserve seed amount bits/counts/accounting; prescribed full-neighbour memory exercises are not coupled dynamic acceptance. Keep experimental/in_progress; no save/wire activation. Full workspace xtask/game-window/trench acceptance unrun. Next ENG-122 remains phase/basin-aware pressure/momentum coupling and the unresolved low-dam gate, then sealed air, edit/remap/trapped ledgers and canonical owner/recovery. See reports/ENG-122.md and ENG-122-packed-phase-memory.jsonl.

ENG-122 increment 7 (2026-10-02): experimental phase_pressure adapter couples canonical fine fractions to the actual 0.25 m two-phase MAC/PLIC reference; pressure converges in every accepted substep and failed candidates are discarded atomically. Packed phase topology is shared, retained arrays capped explicitly (not peak/RSS). Opt-in strict paired-transfer limiter preserves amount bounds without clipping; default MAC behavior unchanged. Full fluid suite 103 passed/0 failed and affected-package strict Clippy passed. Static low-dam and full-wall fine-reference probes accept 600/600 steps with zero downstream water. Channel FAILS after 340/600 steps at maximum fraction 1.0000000000000002; earlier arithmetic failure after 406 steps retained separately. No conservative momentum acceptance: reference velocity advection remains semi-Lagrangian. Keep experimental/in_progress, no production/save/wire activation. Next ENG-122: strict feasible paired-flux update for saturated channel paths, compressed phase/basin pressure plus conservative momentum and unchanged dynamic/interface gates, then sealed air, edit/remap/trapped ledgers and owner/recovery. Generated coupled cost/rest/trench and interactive gates unrun. See reports/ENG-122.md and phase-pressure JSONL evidence.
ENG-122 increment-7 combined-tree verification: cargo xtask check passed formatting, workspace all-targets/all-features strict Clippy and workspace all-features tests: 1152 passed, 0 failed, 63 ignored. Ignored gates remain unaccepted; the separate reference channel probe still fails.

ENG-122 increment 8 (2026-10-02): strict fine-reference water transport now applies paired sequential face corrections and, after unchanged 64 local passes, at most 64 connected-path augmentations to actual available capacity. Strict fraction bounds, exact geometry and water accounting are retained; no amount clipping or game/save/wire activation. Balanced cycles, both saturated-chain directions, ulp-scale throughflow, an interleaved 512-cell path, signed exterior accounting and bounded/no-progress rejection are covered by seven tests. Six-second low-dam/full-wall/channel probes pass 600/600 steps. Extended small channel surge passes 6000/6000 (60 s), accounting error -1.41e-12 m3, all-in median/p99/max 5.230/6.659/9.640 ms; 16 global repairs, additional active-indexed CSR/BFS scratch peak 189488 bytes. Three earlier failed attempts are retained separately. This is the actual fine 0.25 m reference, not compressed or generated-world acceptance; conservative momentum remains unaccepted. Next ENG-122: compressed phase/basin pressure and conservative momentum coupling, unchanged dynamic/interface and actual generated cost gates, then sealed air, edits/remapping/trapped ledgers and owner/recovery. See reports/ENG-122.md and strict-transfer JSONL evidence; keep in_progress/experimental.
ENG-122 increment-8 verification: cargo xtask check passed at the repair checkpoint (before final 64-path cap/budget test), 1156 passed/0 failed/63 ignored. Final-tree formatting, strict workspace all-targets/all-features Clippy and full fluid suite passed (108 passed/0 failed); final capped 6000-step release channel replay passes with the same numerical results, median/p99/max 5.284/6.944/11.026 ms. Workspace tests were not repeated after the cap-only change; affected fluid and strict workspace checks were repeated. Ignored gates remain unaccepted.

ENG-122 increment 9 (2026-10-02): experimental phase_graph builds separate wet patches and dry rows within each exact coarse open component, preserving below-crest pools connected only through air. A bounded snapshot-local piecewise-constant pressure basis aggregates actual fine faces and physical coefficients as P^T A P; no compressed pressure timestep is claimed. The candidate routing helper uses the actual final limited standalone phase transfers for both water and three-axis liquid momentum ledgers, with immutable atomic failure and stale-snapshot rejection. Phase clones share immutable fine fraction arrays without relocation copying; ordinary transport remains bit-identical. Six new graph tests; fluid suite 114 passed/0 failed; affected strict Clippy passed. Generated 512/1024 graph construction: 5879/17379 rows versus 116813/768526 fine open cells, medians 7.688/42.045 ms, additional graph arrays 3659504/14313328 bytes, exact row-water accounting; zero pressure/momentum/coupled/production steps. Existing 6000-step fine-reference channel replay passes with unchanged accounting and 16 repairs. Keep experimental/in_progress: actual conservative staggered momentum, pressure interpolation/hydrostatic balance, unchanged dynamic and generated-world cost gates, sealed air, edit remapping/trapped ledgers and phase-capable canonical owner/recovery integration remain. Next unblocked ENG-122: physical compressed pressure and staggered mass/momentum timestep on this basis. See docs/reports/ENG-122.md and phase-graph JSONL evidence.
ENG-122 increment-9 combined-tree verification: cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests: 1163 passed, 0 failed, 63 ignored. Final formatting and diff checks also passed; ignored gates remain unaccepted.

ENG-122 increment 10 (2026-10-02): opt-in phase-aware pressure predictor now uses actual fine MAC density/top/closed-gas diagonal coefficients, restricts the fine residual, applies eight symmetric row sweeps and lifts pressure correction before the existing fine solve. Fine pressure unknowns/face forces, gauge treatment and pre-predictor stopping tolerance remain; no compressed final timestep or conservative momentum claim. Exact geometry/membership/vertical wet-edge predicates permit shared topology refresh; changed predicates conservatively rebuild, changed solids reject stale geometry, coefficients always rebuild. Generated unchanged-layout refresh medians 0.758/4.318 ms versus full builds 7.787/42.695 ms (512/1024). Low-dam/full-wall/channel 600-step gates pass; predictor channel 6000 steps passes, water error -1.40e-12 m3. Moving channel reuses only once and rebuilds 5999 times; six-second predictor median 6.242 ms versus baseline 5.557 ms despite fewer pressure iterations. Two actual generated startup steps pass for each size/mode, but 1024 all-in cost remains about 6 seconds per 50 ms step, with retained predictor upper bound 218213336 bytes. This is expensive fine-reference startup, not generated minute/trench acceptance or production real-time evidence. Four new tests; complete fluid suite 118 passed/0 failed; affected strict Clippy passed. See docs/reports/ENG-122.md and ENG-122-phase-predictor.jsonl. Keep in_progress/experimental, no production/save/wire activation. Next unblocked ENG-122: stronger phase-aware multilevel correction, incremental patch updates and conservative staggered momentum using final strict transfers, then unchanged dynamic/generated/interface/owner-recovery gates.
ENG-122 increment-10 final combined-tree verification: cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests: 1167 passed, 0 failed, 63 ignored. Ignored gates remain unaccepted; predictor generated performance remains far above the step budget.

ENG-122 increment 11 (2026-10-02/03): opt-in balanced two-level phase-aware pressure preconditioning, B=Q+(I-QA)S(I-AQ), now applies cached physical row SGS during every fine Krylov iteration. Fine unknowns/convergence, component gauges, strict transport and default MAC arithmetic remain. Two new symmetry/positive-energy, scratch-reuse, matched face-solution and atomic unequal-head tests pass; complete fluid suite 120 passed/0 failed. cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests: 1169 passed, 0 failed, 63 ignored. Sequential channel comparison: fine iterations 10634 -> 7305, all-in median 5.486 -> 6.738 ms (slower). Actual two-step generated startup: 1024 iterations 324 -> 176, all-in 5845.744/6752.365 -> 5121.031/5391.947 ms; 512 gets slower. Additional balanced numerical scratch 12,720,208/49,397,104 bytes (512/1024), not whole peak/RSS; retained cap excludes transient arrays. All dam/wall/channel six-second probes and startup probes pass; 6000-step small channel passes with water error -1.371e-12 m3, 18 path repairs, 5999 topology rebuilds. Long-run timings overlap workspace checks, not comparative. No generated minute/trench/game-window or conservative momentum acceptance; no production/save/wire activation. See reports/ENG-122.md and phase-preconditioner JSONL files. Keep in_progress. Next unblocked ENG-122: conservative staggered momentum from final strict transfers and unchanged dynamic/interface gates; pressure hierarchy/incremental patch performance, sealed air, edits/remapping/trapped ledgers and canonical phase owner/recovery remain.

ENG-122 increment 12 (2026-10-03): opt-in first-order staggered mixture momentum advection uses final strict accepted PLIC transfers plus full-volume flux, two half-cell dual lanes, bounded donor subcycles, wall/exterior reaction ledgers and a second fine projection on the new phase. Semi-Lagrangian advection remains the default. Relative pressure accuracy tightens to 1e-12; original 1e-8 absolute contract and unchanged 1e-8 relative dual-mass guard retained. Five new tests cover conservation/uniform transverse velocity/KE, final fluxes, budget and atomic pressure/mass failure, separate heads and explicit sealed-pocket rejection. cargo xtask check: formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests passed, 1174 passed/0 failed/63 ignored; full fluid suite 125 passed. Default low-dam/full-wall pass 600/600 with zero downstream water. Default compressible-air channel FAILS at step 27 (26/600 accepted), dual-mass defect 8.748e-8 kg. Actual generated 512/1024 startup both FAIL first step (0/2), mass defects 2.225e-6/2.446e-6 kg. Existing sealed-air compliance compresses volume but has no conservative gas-mass state; do not bypass this prerequisite. Explicitly labeled incompressible-air comparison passes 600 and 6000 steps; 60-s water error -6.75e-14 m3, advection-only mixture momentum errors below 7.6e-10 kg m/s, additional momentum scratch 2777248 bytes and at most13 subcycles. Six-second comparison median16.146ms; long-run timings overlap checks. This altered air model is separate evidence, not default sealed-air/generated-trench acceptance. Earlier failures retained. No production/save/wire activation; keep in_progress. Next unblocked ENG-122: conservative gas-mass/density state coupled to sealed-air pressure and staggered momentum, then unchanged default channel/generated startup gates. General interface/accuracy, cost/scratch, generated minute/trench/game-window, edits/remapping/trapped ledgers and canonical owner/recovery remain. Exact files/checks/raw evidence in reports/ENG-122.md and momentum JSONL files.

ENG-122 increment 13 (2026-10-03): added sandbox-client --phase-fluid-demo
for a real window on the same channel/low-dam/full-wall headless fixtures.
M switches velocity sampling/conservative momentum; G switches air model;
either resets and pauses while retaining the same-scene camera. Rejected steps
remain visible without applying the candidate. Default is explicitly labeled
incompressible-air comparison. Two reset/rejection invariant tests pass;
affected strict Clippy and release builds pass. Three bounded GPU window runs
and inspected PNGs show both methods and the unchanged compressible step-27
failure. Shared-fixture 600-step replay matches ten prior numerical fields
exactly. No production/game-world/save/wire activation and no physics-gate
acceptance. Keep in_progress. Next unblocked ENG-122: conservative gas-mass/
density coupled to sealed-air pressure and staggered momentum, then unchanged
default channel/generated startup gates. Usage/checks in validation.md and
reports/ENG-122.md; raw replay in ENG-122-viewer-fixture.jsonl.

ENG-122 increment-13 combined-tree check: cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests: 1176 passed, 0 failed, 63 ignored. Renderer fixes additionally passed affected strict Clippy, viewer tests, final formatting/diff checks and release graphical captures.

ENG-122 viewer follow-up (2026-10-03): replaced older debug water boxes with the game's shared smoothed surface/underwater tint/fog/caustics renderer for both methods. Presentation-only byte snapshot leaves solver fractions unchanged. Three viewer tests and four water-look tests pass; affected strict Clippy, release build, formatting/diff checks and two real GPU captures pass. Full workspace checks not repeated for this presentation fix. Details in reports/ENG-122.md. Keep in_progress; next unblocked ENG-122 remains conservative gas-mass/density coupling.

ENG-122 default decision (2026-10-03): user selected best practical large-world performance over exact fluid detail. Retain existing adaptive coarse production MAC/original velocity sampling/multigrid/conservative water-volume transport on a 20 Hz worker with current game water look. Initial 50k-cell target, seed-1 showcase 0.75m at512 and1m at1024; growth twice target. Historical ENG-121 trench medians15.0/25.5ms and0.91/0.88x realtime. Fine-phase/balanced/conservative momentum remain experimental owing to seconds-per-step cost and failed mass gates. Existing production setup already selects this path; no numerical, save/wire or UI switch change. Accept thin-wall leaks/diffuse flow/reported seed loss/rest currents as current default tradeoffs, not full ENG-122 gate acceptance. Four existing worldgen water tests, formatting/diff checks pass. Full checks/GPU/performance not rerun for docs/comment-only change. README, validation, report and worldgen policy comment record decision. Keep in_progress; next unblocked ENG-122 research remains gas-mass/density coupling, required for eventual conservative-momentum activation.

ENG-122 water-only increment (2026-10-03): user explicitly chose freely displaced air. This supersedes earlier gas-mass/density prerequisite references for gameplay. Authoritative construction/growth/recovery and the local demo now select water-only MAC/PLIC with reconstructed atmospheric free surfaces; no gas mass/compression/back-pressure, including enclosed pockets. Adaptive coarse resolution, conservative water amounts, owner/revision checks and canonical ledgers remain; no save/wire format change. Original velocity sampling remains, conservative momentum and two-phase models remain explicit experiments. New inverted-bell flooding/conservation and partial-column hydrostatic tests pass, as do viewer model/reset checks. Seed-1 autumn generated 512/1024 matched expanded-domain 60-second runs accept 1200/1200 steps in each model: water-only median5.028/6.993ms vs priorair11.981/23.717ms, water error<6e-12m3. Flow differs (1024 downstream5.467 vs15.424m3), so no equivalent quality or paced hammer/trench/worker-realtime gate claim. Retained arrays unchanged; transient scratch/RSS not measured. Six-second channel/low-dam/full-wall water-only motion probes pass unchanged gates; resolved wall and low dam cross zero water. Graphical default runs with current water look. See reports/ENG-122.md and water-only JSONL evidence for exact checks and limitations. Keep in_progress. Next unblocked ENG-122: actual paced512/1024 trench and generated rest/shoreline quality withinbudget, then exact-wall/capacity and edit/recovery geometry work. Historical air/conservative momentum failures remain separate, unaccepted experiments.
ENG-122 water-only combined-tree checks: cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy and workspace all-features tests: 1180 passed, 0 failed, 63 ignored. Final fixture-probe CLI additionally passed affected strict Clippy, formatting and three release motion probes. Ignored gates remain unaccepted.

ENG-122 paced large-world trench validation (2026-10-03): reconstructed documented ENG-121 workload in water_trench_probe, actual Simulation/vegetation/20Hz water worker, radius3 cuts every6ticks then3000pacedticks. Final512 commits46cuts, flow45.844534m3past old edge,0.9015x realtime,solver median10.249ms. Final1024 commits45cuts,0.8967x realtime,7.901ms median but ZERO water past old edge: flow gate FAIL, executable exits1. All pressure substeps converge, exact[0,1]fractions and mass errors<1.1e-11m3. Live vegetation adds94/14otheredits;512grows to96100cells. Largest1024owner tick409.633ms is not a growth tick and unattributed. Coarse centre sections implicate shallow shore fractions0.13-0.21 over majority-solid cells, adjoining open/dry trench; partial-cell pressure/velocity support is a hypothesis requiring a small reproduction. Repeated final runs sequential without builds/owned demo; initial evidence retained separately. Strict probe Clippy/release build/fmt/diff pass. No runtime changes; parent fullcheck1180/0/63not repeated. No graphical/network or equivalent matched historical benchmark acceptance. See reports/ENG-122.md and paced-trench JSONL. Keep in_progress; next unblocked ENG-122: small shallow-lip invariant reproduction and physically valid fix, then unchanged paced512/1024 reruns. Do not promote the default as large-world flow accepted.

ENG-122 small shallow-shore reproduction (2026-10-03): new spall_fluid example shallow_shore_probe captures exactly resolved aligned shelf/trench geometry, 12x6x2 one-metre cells, shelf top2m/lip6m/trench floor1m. After600x50ms,0.20m water-only layer (2.4m3) has zero pressure rows, zero velocities, zero downstream water and bit-identical fractions; mass error0. First-step trace shows gravity0.4905m/s wiped to0 by pressure/velocity extension. All reconstructed cell centres lie in air, so no liquid pressure or extrapolation donors remain. This reproduces the mechanism without workers, growth, vegetation or partial solids; large-world geometry can still contribute. Explicit controls:0.75m water-only moves3.978048m3of9;0.20m prior air model moves1.709760m3of2.4. All600steps accepted and pressure converged; speed/conservation is not flow acceptance. Release reproduction exits1; deeper-flow invariant passes; ignored desired shallow-drain invariant was explicitly run and FAILS(exit101). Strict example Clippy/fmt/diff pass; no numerical runtime change or full workspace/large-world rerun. Evidence and exact checks in reports/ENG-122.md, shallow-shore JSONL and first-step trace. Keep in_progress. Next unblocked ENG-122: physical pressure/velocity support for shallow reconstructed liquid with no interior cell-centre samples, preserve rest/atmospheric surfaces/conservative mass/walls, then enable invariant and rerun unchanged paced512/1024. No hidden anchors, clipping, air switch or widened workload.


### ENG-122 shallow-film trial checkpoint (2026-10-03)
Diagnostic hydrostatic-film support plus zero-air accepted-transfer dual momentum remains opt-in and FAILED acceptance. The unchanged 144-cell fixture stops at 333/600 shallow and 265/600 deep steps (nine substeps required, budget eight); peak mechanical energy grows to 39.3x / 5.18x initial despite conserved water. A new wider partial-layer rest gate also exposes a baseline failure at step 3, 0.035919 m/s versus 1e-7 target; prototype fails at step 1, 0.4905 m/s. Ordinary example tests explicitly ignore these two unresolved gates, both actually run to failure. Zero-air momentum invariant tests and flat shallow pool/full-wall rest tests pass. Gameplay policy, canonical records and worker authority are unchanged. Next ENG-122 assignment: match subcell liquid geometry across pressure, face inertia, gravity and accepted mass/momentum transfers; pass drainage/rest/energy before rerunning unchanged paced 512/1024 trench. See docs/reports/ENG-122.md and ENG-122-film-{baseline,prototype}.jsonl. Status remains in progress.


### ENG-122 atmospheric surface-distance correction (2026-10-03)
Repaired the wider partial-layer resting-pool gate without changing its workload or thresholds: choose validated air-side PLIC pressure boundary first, then validated wet-side fallback, shared by pressure matrix and velocity correction. Nearly saturated bulk-cell planes no longer override the free surface. Fractions/canonical ledgers remain unchanged. Four600-step analytical cases at0.5/1m and25/50ms pass pressure<2.40e-9Pa error, speed<2.75e-13m/s and volume error<1.25e-13m3; targets1e-5Pa/1e-7m/s/1e-10m3. Original lookup makes new regression fail immediately at0.016892m/s. Previously failed12x6x2 rest gate is enabled and passes600steps; ordinary example3passed/1ignored. Unchanged0.20m shallow drainage stillFAILS(zero rows/zero downstream);0.75m control4.376055m3of9 flows. Rejected film prototype remains disabled. See reports/ENG-122.md and surface-rest evidence. Next ENG-122: matched subcell pressure/velocity/face inertia for liquid below all centre samples, preserving rest/energy/conservation/walls before unchanged paced512/1024. Keep in_progress.

ENG-122 surface-distance final checks: cargo xtask check passed formatting, strict workspace all-targets/all-features Clippy, and tests: 1183 passed, 0 failed, 63 ignored. Ordinary shallow-shore example tests: 3 passed, 1 unresolved drainage gate ignored; that ignored gate was explicitly run and failed. The three water-only reference motion probes (channel, low-dam, full-wall) each passed 600 steps at 10 ms; timings overlapped workspace checks and are not benchmark evidence. No paced large-world or GPU rerun. Details in reports/ENG-122.md.

ENG-122 support checkpoint (2026-10-03): added an opt-in reconstructed-liquid
pressure support diagnostic with actual wetted faces, embedded free surfaces
and accepted-transfer water-only momentum. Unchanged 600-step shallow/deep
shelf drainage, energy, conservation, full-wall and ordinary rest gates pass.
Nearly saturated bulk-water rest FAILS immediately (0.0083013904 m/s).
Gameplay remains on the existing water-only backend; ENG-122 stays in progress.
Next unblocked ENG-122 item is consistent physical free-surface ownership
across neighbouring reconstructions, then reference and original 512/1024
paced trench gates. No speed, whole-memory or viewer acceptance is claimed.
Exact evidence and regularization details: docs/reports/ENG-122.md.

### ENG-122 surface reconstruction checkpoint (2026-10-04)

Repaired closed-wall PLIC ghost sampling, nearly full complementary air geometry,
and tiny exposed-polygon centroids. Added one-sided closing-interface pressure
support on embedded and Cartesian boundaries, preserving amounts and limits.
The previously ignored rest gate is enabled: eight 600-step cases at 0.5/1 m
and 25/50 ms pass, peak speed 8.9182e-12 m/s (limit 1e-7), maximum volume error
2.9133e-13 m3 (limit 1e-10). Shallow/deep drainage, energy and walls pass;
76 fluid library tests pass. Low-dam/full-wall references pass 600 x 10 ms.
Dynamic channel FAILS at 228/600: 12 required substeps versus 8; peak face speed
125.0097 m/s, water error 7.11e-15 m3, no integral energy growth. Keep diagnostic
and ENG-122 in_progress. Next unblocked ENG-122: dynamic interface velocity
and accepted momentum support near vanishing fragments; pass unchanged channel
and current rest/drainage/energy gates, then original paced 512/1024 trench
before gameplay promotion. Exact checks, evidence and full-check result:
docs/reports/ENG-122.md and ENG-122-surface-reconstruction-* files. No viewer,
GPU, whole-memory or generated-performance acceptance.

### ENG-122 dynamic interface velocity checkpoint (2026-10-04)
Retain embedded pressure corrections as derived liquid-centroid velocity,
transport them with exact final accepted water transfers, run existing paired
path repair before low-order validation, and warm-start reduced-potential CG
within unchanged budgets. No air mass, cutoffs, clamping or topology changes.
Channel now passes600/600 (peak5.127m/s), low-dam/full-wall pass, eight nearly
saturated rest cases and shallow energy/drainage gates pass. Full xtask check:
1192passed/0failed/63ignored, including strict Clippy. Original final gated
paced1024 trench passes:5.130528m3 beyond old edge,0.8908x realtime. 512 flows
20.156031m3 but FAILS final50ms step (10substeps vs8); fluid time stalls at26s.
Keep diagnostic/in_progress and gameplay default unchanged. Next unblocked
ENG-122: reproduce/fix edited-worker dry-face velocity and boundary invalidation,
then rerun unchanged gates. Exact files/checks/risks in reports/ENG-122.md.
### ENG-122 unchanged-boundary and subnormal reconstruction checkpoint (2026-10-04)
No-op boundary commits/refreshes preserve corrected predictor and pressure;
actual geometry changes still invalidate them, trapped-water release and owner
revision validation remain intact. Small shelf/trench compares600steps exactly
against uninterrupted dynamics. Extracted512fragment C=f64::from_bits(7) exposed
quadratic-offset and centroid determinant underflow; scaled analytic offset and
per-axis tetrahedron normalization fix it without deletion/cutoffs. Diagnostic
pressure now uses symmetric Gauss-Seidel CG on the same graph and verifies
apparent convergence against the true residual within the original400budget.
81fluid tests, shallow6gates and channel600steps pass (peak5.2316m/s). Final-source
1024trench passes5.129372m3past old edge at0.8904x realtime;512stillFAILS final50ms
step (10substeps vs8), stalls27.1s despite retained predictor being present.
Keep diagnostic/in_progress and default unchanged. Next unblockedENG-122:
extract/reproduce actual wet-face donor and pressure correction driving the
512Cartesian/extension velocity spike. Full checks/risks/evidence in reports/ENG-122.md.
Full cargo xtask check passed:1195passed/0failed/63ignored, strict Clippy and formatting included.

### ENG-122 local pressure accuracy checkpoint (2026-10-04)
Wet-face trace and a force-free5x5x5 reproduction confirm stale warm pressure
can pass global flux convergence in tiny rows and inject66.7m/s motion. Every
non-null positive row now also needs diagonal-scaled potential accuracy;
symmetric local corrections use the original total400iteration budget. New
regression passes at1e-40/1e-120/1e-300 fractions without dropping/clipping water.
82fluid tests and channel600, shallow6 gates pass. Original512trench nowPASSES
final50msstep in two runs (4substeps within8),24.77/24.62m3past old edge at
0.5407/0.5370x realtime. 1024PASSES5.051671m3past old edge at0.8884x. Keep
in_progress/diagnostic:512performance, full memory and local momentumaccuracy
remain open. Next unblockedENG-122: profile grown512worker/pressure/transport
cost and classify skips; reducecost without weakening newly passing gates.
Exact checks/evidence/contracts:reports/ENG-122.md and ENG-122-local-pressure-*.
Full cargo xtask check passed:1196passed/0failed/63ignored, strict Clippy and formatting included.

### ENG-122 worker profile and closed gauge checkpoint (2026-10-04)
Separated busy/stale/stability/residency skip reasons and accepted-step stage timing; preserved counters across domain growth, which previously reset them. Scheduling/revisions/rates/numerical budgets/default backend unchanged. Closed-component row lists now replace repeated per-gauge scratch allocations/scans without changing component means or positive-liquid participation. Matched 512 pressure time is essentially unchanged; no meaningful speedup or whole-memory reduction claimed. Final unchanged sequential512/1024 trenches pass flow/conservation/final50msadvance; observed stability skips zero. 512 remains about0.54x real time, dominated by similar pressure/transport cost. Exact final figures/evidence/checks in reports/ENG-122.md and worker-profile files. Release fluid83/0, owner9/0, opted-in shallow6/0; referencechannel/low-dam/full-wall600/600 each; cargo xtask check fmt/strictworkspaceClippy/allfeaturetests1198passed/0failed/63ignored. Keep diagnostic/in_progress; graphical/network/fullmemory/localmomentumaccuracy remain open. Next unblockedENG-122: split transport geometry/FCT/path-repair/momentum costs and optimize largest measured part without weakening existing gates.