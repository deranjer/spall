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
- **Increment 3 (later).** Exterior daylight-terrain scene + moving-debris /
  active-collapse scenes; >= 120 consecutive *moving* frames (camera pan +
  moving debris + rapid destruction) with the settled-vs-moving comparison; a
  pipelined (threaded) client frame loop + the broader CPU frame-work budget;
  a bounded denoise/temporal pass if a real scene tightens the budget; quality
  flags (leakage, ghosting, noise, shadow instability); cross-GPU capture +
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
- **Increment 3 (later).** Body-on-body contact fracture (`apply_contact_damage` currently converts dynamic-vs-terrain contacts only), and waking a dormant body when a *terrain* edit lands adjacent to it ("nearby edits wake affected neighbors").

### T22 — Material-dependent structural strength

Dependencies: T07, T08, T16. Own: strength design and spall_structure extension.

First produce a reviewed specification of cluster/bond formation, capacity/load units, deterministic propagation, failure order, and save/replication fields. Then implement that exact approximation with small analytic fixtures. Do not invent a stress solver inside a renderer or collider task.

Accept: long weak cantilevers fail, comparable strong supports hold within declared limits, removing support cascades predictably, restart preserves damage, and replicated hashes include every structural layer. Document artifacts and performance; no claim of physically exact engineering simulation.

### T23 — G3/G4 integrated engine acceptance

Dependencies: T15, T17, T18, T20, T21, T22. Own: complete gate report and targeted fixes.

Run all correctness, crash, impairment, visual, and eight-client workload scenarios. Include geographically separated players inside the bounded world, a multi-region collapse, prolonged rubble accumulation, and late join after heavy edits.

Accept: validation.md gates pass or remaining failures are clearly recorded as open. Produce reproducible commands and raw evidence. This is the first game-ready engine slice, still without menus/editor/survival content.

### T24 — G5 larger-world feasibility

Dependencies: T23. Own: separate architecture decision and scale prototype.

Measure region streaming, render LOD seams, world generation versions, multiple rebased physics regions, region merge/split, body transfer, and structural graph growth. Set a measured world/player/debris envelope. Do not just enlarge constants.

Accept: separated players retain correct physics precision; approaching regions merge without body duplication; distant edits persist and affect support; LOD changes never alter authoritative geometry. Infinite scale remains unclaimed.

### T25 — Handoff to game systems

Dependencies: T23; T24 only if the game needs the larger-world envelope immediately.

Produce a game-facing API/examples for authoritative tools, placement, material definitions, recipes, damage, entity spawn, and asset loading. Recommend an ordered survival-content backlog separately. Keep game rules in sandbox_game, and preserve the engine's launch/scenario interface.

Accept: one example game tool is added without editing renderer, transport, or storage internals; agents can reproduce all engine gate scenes from a clean checkout. UI/editor work remains unassigned.

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
