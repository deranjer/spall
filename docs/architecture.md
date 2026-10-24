# Architecture and invariants

Status: proposed implementation contract. Numerical defaults are frozen after G1/G2. Multiplayer and full-world destruction are mandatory throughout.

## Workspace and dependency direction

Arrows below mean "depends on". Create crates when their first task needs them; avoid empty scaffold packages.

```text
spall_core       IDs, coordinates, ticks, units, material definitions
spall_asset      CPU SPVX assemblies and transform clips; no GPU/network/physics
spall_voxel   -> spall_core                     storage, sampling, edits, queries
spall_jobs    -> spall_core                     bounded scheduling, result tokens
spall_mesh    -> spall_voxel, spall_jobs        surface generation, no GPU
spall_structure -> spall_voxel, spall_jobs      connectivity, support, split plans
spall_physics -> spall_voxel                    Rapier adapter, collision builds
spall_fluid  -> spall_core, spall_voxel          CPU fluid state and solid-boundary snapshots
spall_sim     -> spall_structure, spall_physics, spall_fluid, spall_jobs, spall_protocol   authoritative state and tick order
spall_protocol -> spall_core                   explicit DTOs and codecs only
spall_net     -> spall_protocol                 Quinn transport adapter (Tokio; T09)
spall_store   -> spall_protocol                 checkpoint/journal bytes and indexes
spall_render  -> spall_mesh, spall_core            wgpu resources and render passes
spall_server  -> spall_sim, spall_net, spall_store, spall_jobs
spall_client  -> spall_net, spall_voxel, spall_physics, spall_render, spall_jobs, spall_fluid
sandbox (example) -> spall_server, spall_client   game rules and executable entry points
xtask          -> spall_store                   process/scenario/build orchestration and save migration
```

spall_sim owns conversion between authoritative state and protocol records; persistence does not own simulation objects. The graph edge is refined from `spall_sim -> spall_structure, spall_physics` to add `-> spall_jobs, spall_protocol` (T08): staging is submitted to a `spall_jobs::Scheduler` and re-validated through a `JobToken` like any other off-tick result, and every commit emits a `spall_protocol::TopologyTransaction`. Both new targets are foundation crates (`-> spall_core`); no cycle is introduced. The client maintains a replica and prediction state; it never runs server-only structural decisions. Render input is an extracted immutable view of the replica, never a reference into a running server.

**Water direction (2026-10-10): stacked columns with virtual pipes.** The
`grid_mac` MAC/VOF solver described below was retired (ENG-169, 2026-10-11) and
its code removed; git history keeps it. Its replacement is the column model in
`spall_fluid::columns`: one column per
terrain cell (0.25 m), each split into vertical open spans holding a depth;
4-neighbour spans whose open intervals overlap are joined by persistent
virtual-pipe flows that give momentum, exact hydrostatic rest and exact volume
conservation. Storage is sparse per 32 x 32 column region with sleep/wake, so
cost follows moving water rather than world size. Navier-Stokes accuracy,
vortices and sealed-air pressure are dropped. Design, work items W0-W5
(ENG-164..ENG-169) and acceptance tests: `docs/water-agent-handoff.md`. The
column model is the only water in the engine. The ENG-103/105/122 text below
describes the removed grid solver and is kept as a record. The reproducible
failing baseline is `docs/reports/ENG-164.md`; the removal is
`docs/reports/ENG-169.md`.

History of the removed grid water solver (ENG-103/105/122, ENG-104 viewer): see [reports/history-architecture.md](reports/history-architecture.md).


Engine libraries live in `crates/spall_*`. The `sandbox` package lives in `examples/sandbox`, with game-specific rules/material catalogs and the `sandbox-server` / `sandbox-client` binaries. `sandbox_game` below denotes that package's game-rules module, not another engine dependency. Hosts receive game configuration and, when needed, a small statically linked rules interface; engine libraries never import the example. T00 only needs host configurations/run functions and thin binaries, not speculative gameplay hooks. `tools/xtask` owns orchestration; as of T09 it also links `spall_net` for the
in-process `cargo xtask net-check` transport harness. Add `games/survival` only
when actual survival work begins.

## Coordinates, identity, and materials

Right-handed world, +Y up. Physics units: metres, seconds, kilograms. Camera looks along local -Z. Centralize projection/depth conventions; wgpu clip-space depth is handled by the renderer. Verify winding and ray reconstruction with fixtures.

- `WorldId`: persistent 128-bit world identity.
- `EntityId`, `VolumeId`, `TransactionId`: server-assigned monotonically increasing u64 values within a WorldId; never reuse. Persist next-ID counters. Zero is reserved.
- `Tick`, `Revision`, `JournalSeq`: u64, checked arithmetic. On exhaustion fail explicitly, never wrap.
- `BrickCoord`: three signed i64 cell-brick indices. `LocalCell`: three u8 values in 0..32.
- Linear cell index: `x + 32 * (y + 32 * z)`.
- Global cell conversion uses Euclidean division and remainder. Cell -1 maps to brick -1/local 31.
- World-space authority positions: f64 metres for transforms; integer cells for geometry. Physics and rendering convert to f32 relative to an explicit nearby origin. The first bounded world uses one physics origin.
- A volume has a rigid transform, a fixed cell-size code, and sparse local bricks. Terrain uses an identity-oriented world grid. Dynamic bodies refer to volumes; a volume can contain many bricks.

Terrain cells are 0.25 m initially: one brick is 8 m wide. A one-metre placement tool modifies 4 x 4 x 4 cells in a transaction. Later detailed objects may use 0.0625 m cells in their own volumes; never silently mix cell sizes inside one brick or resample a falling building.

`MaterialId` is u16, with 0 = air. Registry entries have stable string names and fixed numeric IDs in the world manifest. Never assign IDs by file enumeration order. Rendering fields: albedo, roughness, metalness, emissive radiance. Simulation fields: density, friction, restitution, hardness, bond strength, and collision/opacity flags. Units and allowed ranges belong to the registry schema. Rendering uses linear color; asset colors are converted from sRGB at import.

Unknown material IDs or incompatible manifests fail loading/handshake with an actionable error. Do not reinterpret saved IDs. Runtime material hot reload initially permits visual fields only; structural properties change through an explicit authoritative content revision.

## Sparse voxel storage

Start with `Uniform(MaterialId)` or `Dense(Box<[MaterialId; 32768]>)` per brick. Uniform air needs only metadata. Materialize dense storage on edit; collapse it back to uniform when appropriate. Add palette packing only after profiling shows a need.

A dense brick is 64 KiB of material IDs alone. The 256 x 128 x 256 m world at 25 cm resolution contains 16,384 bricks and would use 1 GiB if all were dense. This excludes replicas, collision, meshes, topology, jobs, and GPU caches. A sparse map does not itself make dense terrain cheap. Enforce separate cache budgets and measure actual occupancy.

Auxiliary fields such as damage and future temperature are optional layers, not padding in every voxel. All authoritative layers participate in revision/hash/checkpoint rules. Initial edits directly remove/change cells; detailed damage accumulation arrives with material strength.

Immutable brick payloads use reference counting for job snapshots. Copy only touched bricks on edit; bound the number and bytes of old snapshots retained by pending tasks. Empty space, unloaded data, and failed data are distinct states.

Each voxel exists in exactly one authoritative owner: terrain or one detached volume. Derived meshes, colliders, lighting volumes, and topology labels are caches. Never reconstruct saved gameplay geometry from a render mesh.

## Tick and jobs

Server simulation is fixed at 60 Hz; publish motion snapshots at 20 Hz initially. Headless test mode advances a requested tick count without wall-clock sleeps. Interactive mode uses a monotonic clock, bounded catch-up, and explicit overload reporting; never vary physics dt to hide load.

At a tick boundary:

1. Drain bounded input queues; validate client sequence numbers, permissions, and action limits.
2. Validate completed job results and prepare complete topology transactions.
3. Apply ready transactions in server-assigned order, with matching collision and body ownership updates.
4. Apply player input; advance character movement and server physics by one fixed step.
5. Convert qualifying contacts/damage into intents for a later tick; no mutation inside physics callbacks.
6. Extract topology events, motion snapshots, and persistence records from one committed state.
7. Schedule bounded dirty work and update replication interest sets.

A job token contains world/session generation, volume ID, affected brick revisions, topology epoch, and the exact read dependency set. Meshing includes a one-cell halo with all needed face/edge/corner neighbors for ambient occlusion. Missing-neighbor sentinels have revisions invalidated when data arrives. A stale result is discarded/requeued, including after unload/reload of the same coordinates.

CPU jobs operate outside the tick on immutable data. GPU submission stays on the render owner thread. Network and disk runtimes cannot hold simulation locks or await inside a tick. Separate priority and byte budgets for generation, edits, collision, topology, and visuals prevent background work starving player actions. Terrain generation is specified in `docs/worldgen.md` (`spall_worldgen`: deterministic, versioned, never regenerated over modified bricks).

## Editing and cross-brick atomicity

Every player/bot/console action becomes an intent with a request ID. The server determines the hit against its current authoritative geometry. A client-supplied hit point, material, target transform, or body ID is only a claim to validate.

Quantize approved cutting operations into the target volume's local integer coordinates. Version each integer brush algorithm; use checked arithmetic and explicit inclusion rules. A sphere removes cells whose centre has squared distance <= radius squared in specified fixed-point units. Never derive client geometry changes from independently evaluated floating-point world rays.

An edit plan records before/after brick data, revisions, ownership changes, support consequences, colliders, and body mass/transform changes. Multi-brick/multi-volume edits are one transaction. Prepare all required authoritative results, then swap at a tick boundary. Until ready, the old geometry/collision remains authoritative and the request is pending; local effects can preview the action without claiming a committed hole.

Conflict rule: jobs may overlap in preparation, but commits revalidate every read dependency. Conflicting older work retries or is recomputed in request order. After repeated conflicts, serialize the affected region through a bounded queue to guarantee progress. Moving-body edits are defined in the body-local frame sampled when the server accepts the action; at commit they apply to that same material location if the topology preconditions still hold.

Physics must never collide with a deleted wall or fall through a newly built wall after the authoritative transaction commits. Renderer uploads may lag briefly, but staging keeps an old consistent replica visible until its transaction can be presented. Expose revision lag and prioritize local collision/render updates.

T08 outcome (`spall_sim`): the whole path above is implemented for the all-resident G1 case. An accepted `EditIntent` is staged off-tick against an immutable snapshot (`stage_edit`) — deterministic brush plan, dry-run `EditOutcome`, `spall_structure` re-classification, and a `JobToken` over every brick read — then committed atomically at a tick boundary (`commit`). A stale token at commit means an earlier commit that tick touched a shared brick; the intent is recomputed and retried in request order, and a region that keeps losing that race is routed through a bounded serial queue (one commit per tick) so it still progresses. On commit the brush is applied to the live volume, every unsupported component (terrain) or every non-largest component (a free dynamic body) is copied into a new body **at its exact split-instant world location** with mass from the fine voxel grid and velocity `v_parent + omega_parent x (r_child_com - r_parent_com)` plus a one-time explosion impulse, and every affected collider is rebuilt in the same tick. A repeated `RequestId` returns the existing `ActionStatus` and can never cut twice or apply a second impulse. The collider budget from `docs/collision-decision.md` (`B = 4096` merged boxes, then a conservative same-resolution coarsen) is enforced here. Terrain-to-body and body-to-body transfer do **not** re-centre the child onto its COM: keeping the child's cells and transform identical to the parent's is the exact preservation the architecture requires, and `spall_physics` carries the off-origin COM. No replication, persistence, player movement, or contact-to-intent conversion is in T08.

T10 outcome (`spall_sim` replication surface + `spall_server` / `spall_client`): a committed `TopologyTransaction` is now **self-describing**. `spall_sim::commit` appends, after the `IntegerBrush` op and each `SplitOff`, explicit canonical `+X` `TopologyOp::CellRun`s — one group filling each child volume with its detached cells' materials, then the runs that set those cells to air in the source — so a replica reconstructs an exact split with no `spall_structure` (`docs/protocol.md`: "canonical source-cell ranges ... not a client rerun of ... support heuristics"). A split whose inline `CellRun`s would overflow one reliable control record is re-encoded (T17 increment 1 / ENG-64) as `[IntegerBrush, SplitOffBaseline*, SourcePatchBaseline]` — the child geometry and the source's post-cut affected bricks travel as compressed `spall_protocol::baseline::BaselineVolume` blobs inside the op list, so the durable journal and exact-replay reconstruct it unchanged; a split whose blob is still over `MAX_SPLIT_BASELINE_BLOB` (the G1 64-brick collapse) commits with marker-only `SplitOffBulkBaseline` / `SourcePatchBulkBaseline` ops (T17 increment 2 / ENG-64) plus one out-of-band `BaselineWorld` delivered to replicas on a bulk stream and journalled as a Geometry entry in a `TopologyAttachments` payload; the replica holds the marker transaction (`AwaitingBulkSplit`) until the blob assembles. `spall_sim` also exposes the per-tick server feed (committed transactions, `ActionStatus`, a 20 Hz `MotionPublisher`, a `RepairRequest` → `CellRun` responder). `spall_client::replica::ReplicaWorld` holds the last consistent view as plain `spall_voxel` volumes and applies each transaction **candidate-first**: it checks every `before` brick revision against the live replica (a `Revision::ZERO` entry means "was absent"), replays the ops into an isolated clone, verifies every declared `result_hash`, and only then swaps it in — a `before` gap yields `RepairRequest`s and no mutation, any op or hash failure is rejected whole. The applied-`TransactionId` set is the replay guard; the control `SequenceGate` only tracks the high-water mark so a catch-up delivery of an earlier transaction is still evaluated. An emptied body (source or child) is tombstoned and a late motion packet can never resurrect it. `MotionTrack` holds the two newest accepted states for interpolation and one newest pending snapshot for a body the replica has not created yet (or whose topology revision it has not reached). `spall_server::serve` runs the authoritative `Simulation` on a blocking thread behind a `spall_net` QUIC endpoint and fans committed transactions + `ActionStatus` to every client's reliable control stream in commit order, with motion as datagrams; `spall_client::net` is the headless replica client. Live late join and hash repair land in T17; player prediction remains a later task (T19).

## Structural connectivity and collapse

Use six-face connectivity; corner/edge contact does not create a bond. Do not assume that all solid cells in a brick share one connected component.

1. Label local components in each changed brick. Generate boundary face connectivity records.
2. Build a higher-level graph of `(brick, local-component)` nodes joined across matching occupied boundary cells.
3. Propagate support from occupied cells on the declared lower world plane.
4. For deletions, invalidate affected labels and graph edges, and search affected components. Union-find alone cannot handle disconnections. Initial implementation may use bounded BFS plus relabeling; optimize only with adversarial tests.
5. Stage every unsupported component for conversion into a dynamic voxel volume. A component may cross many bricks; do not make one body per brick.

Graph metadata for unloaded regions must remain available or be loaded before resolving support. Geometry requests follow graph dependencies. Unknown support means pending analysis. It never means air or a permanent anchor. Searches are incremental, time/byte budgeted, and resumable. Large edits wait for analysis rather than publishing a partly evaluated structure.

`spall_structure` (T07) implements the all-resident G1 case. It depends on `spall_jobs` so a completed analysis carries a `JobToken` over the exact brick revisions it read (plus *absent* sentinels for unresident dependencies), and a consumer discards a stale analysis exactly as it would any other off-tick job result. A build takes an explicit residency mode: `AllResident` treats an absent neighbour brick as empty space and only a *failed* brick load as Unknown; `Streamed` (T18) treats any absent neighbour as Unknown. A brick outside the volume's declared bounds is always a hard world edge, never Unknown.

Splitting has a conservation ledger: source occupied cells = retained cells + child cells + explicitly destroyed cells. Use sorted canonical cell ranges for membership. For mixed materials compute mass, centre of mass, and inertia from voxel density and size, including each cube's own inertia and offsets. Child linear velocity inherits parent velocity plus angular velocity cross the centre offset; angular velocity inherits the parent value before impulses. Apply declared explosion impulses once on the server.

Terrain-to-body transfer removes source cells and creates child geometry, stable IDs, transforms, colliders, and revision records atomically. Initial child orientation is identity for terrain; a moving parent's orientation is retained. Re-centering a volume for its centre of mass must preserve world-space geometry exactly.

Dynamic bodies use the same edit/split path. A second cut can create additional bodies. Ground contact does not automatically weld bodies into terrain. Sleeping bodies retain geometry, pose, identity, and future destructibility; nearby edits wake affected neighbors. Initially do not bake rotated rubble back into the grid because that changes geometry and ownership semantics.

### Material strength stage

Connectivity-only collapse is G1. For G4, partition connected material into structural clusters and explicit bonds; calculate a deterministic approximate gravity load/support score and compare against material capacities. Use fixed integer inputs, stable traversal/tie-breaking, and server-selected break outcomes. Store damage/broken bonds as authoritative state so restart cannot heal a failing beam.

This is a gameplay approximation, with fixtures for weak cantilevers, strong arches, undermined ground, and damaged joints. An experienced reviewer must freeze the actual equations and units in T22 before an implementation agent writes them. Finite-element simulation is out of scope.

The frozen equations, integer units, propagation order, failure order, and save/replication field shapes are in `docs/structural-strength.md` (`strength_algo_version = 1`, awaiting integrator sign-off). `spall_structure::strength` implements exactly that: a support forest rooted at the anchor plane, saturating post-order load accumulation, per-bond demand-vs-capacity with a moment arm on lateral joints, and a one-bond-per-iteration deterministic cascade producing an authoritative `DamageState`. Wiring `DamageState` into the `spall_protocol` / `spall_store` DTOs is the second increment of T22.

### Large collapses and load policy

Do not solve overload by turning every voxel into a body. Use connected compound bodies and bounded fracture granularity. A huge connected object can remain one multi-brick body if the collider representation supports it. If it cannot, T06 must establish a documented deterministic coarse fracture policy before the scale gate.

Pending work is allowed within a declared latency budget. Once exceeded, throttle new expensive intents with explicit retry responses. Never keep a completed unsupported component permanently static because a body cap was reached. Cosmetic dust can be capped/dropped freely; solid gameplay matter cannot disappear. Settled distant bodies may be persisted and deactivated only when their whole interaction region is dormant, and must reactivate before contact or edits.

T21 outcome — contact damage, increments 1 & 3 (`spall_physics` + `spall_sim::contact_damage`): after each `step_physics`, `PhysicsWorld::contact_impulses` reports every solver contact pair's accumulated normal impulse (N·s), world contact point, and normal — a read-only view, no mutation in a physics callback. `spall_sim::contact_damage::ContactDamagePolicy` is the pure filter from those impulses to bounded [`EditIntent`]s: a contact qualifies only when its impulse exceeds `impact_ratio ×` the struck body's own resting support impulse `m·g·dt` *and* an absolute floor, so a body at rest (impulse ≈ its weight) never fractures the surface under it; a per-region (brick) cooldown blocks repeat damage for `cooldown_ticks`; at most `max_intents_per_tick` cuts are emitted world-wide per tick with the rest dropped and counted (never queued); and a body created by a topology commit on the same tick is skipped (recursion guard) so a cut cannot cascade into further cuts in one tick. The policy works purely in the **target volume's local cell frame** — `ContactEvent` carries `target: EditTarget` and `point_cell` already resolved to that frame — so the brush and the cooldown key are the same code for terrain and for a body, and a moving body's cooldown spot does not drift with its world pose. `Simulation::apply_contact_damage` classifies each contact: a dynamic/terrain pair damages the terrain volume (increment 1); a **dynamic/dynamic** pair damages the body being struck — the slower of the pair, tie-broken to the lower mass (`ContactDamageConfig::still_speed_m_s`) — with the contact point mapped through that body's pose into its local cells (increment 3). Cuts are submitted through `Simulation::submit` (so a body-targeted cut also passes the dormancy-wake guard) with server-authored request ids in a reserved band (`1 << 62`); admitted cuts stage off-tick and commit on a later tick like any client edit. Body-on-body fracture is a normal committed transaction, so conservation (`terrain + child + destroyed == before`) and the request-id band are unchanged.

T21 outcome — region dormancy, increment 2 (`spall_physics` + `spall_sim::dormancy`): a settled detached body whose whole interaction region has gone quiet is *deactivated* — `PhysicsWorld::deactivate_body` removes its Rapier rigid body and collider (a reversible sibling of `retire_body`, keeping the `BodyId` slot and every rebuild parameter), and `SimWorld` keeps the authoritative `Body` record frozen (`sleeping = true`, zero velocity, pose held). Dormancy touches no cells, ownership, or damage, so `world_hash`, conservation, and the checkpoint set are unchanged — it is purely a step-cost optimisation. `spall_sim::dormancy::DormancyPolicy` is the pure decision layer: a body asleep and still for `settle_ticks` consecutive ticks with no active region (a player capsule or an awake body) within `wake_margin_m` of its bounding sphere is deactivated; a dormant body is reactivated when an edit targets it (immediately, through `Simulation::submit`) or a player/awake body comes within the margin (after `min_dormant_ticks` of hysteresis); deactivations and proximity reactivations are each capped per tick. `Simulation::apply_dormancy` is opt-in like `apply_contact_damage` and is not run by `tick`; it takes the tick's `TickReport` so a **terrain** transaction committed this tick that lands within `wake_margin_m` of a dormant body hard-wakes it (increment 3) — the ground a settled body rests on just changed, so it does not wait out the hysteresis window. `PhysicsWorld::reactivate_body` rebuilds the body in its original slot from the caller's grid and stored pose before any contact or edit, so a dormant body stays fully destructible.

T21 outcome — server wiring, increment 4 / 3c (`spall_server::serve`): both passes stay opt-in — `ServeConfig.contact_damage` / `.dormancy` default to `None`, so an ordinary run is byte-identical to before this increment. When set, `serve`'s tick loop calls `Simulation::apply_contact_damage` / `apply_dormancy` once per tick, right after the commit they react to, and folds the counts into `ServeSummary` (`contact_damage_cuts_submitted/rejected`, `dormancy_deactivations/reactivations_total`, version 5). The two passes are never combined with `--await-body-settle` on an existing gate fixture: a deactivated body leaves the live physics world, and `await_body_settle`'s "every detached body settled" read (`max_penetration_m`, sleep state) is a physics read, so the G1 `body-rest-on-structure` gate keeps both off. Evidence lives on a dedicated `Scene::SleepWake` (`spall_sim::fixtures::sleep_wake_setup` — the walk arena plus a small column-and-beam in the player lane, `docs/reports/ENG-28-increment-4-sleep-wake.md`): a client cuts the column, the detached beam settles and (with `--dormancy` on) deactivates with nobody nearby, the client walks down the lane into the wake margin and the beam reactivates by proximity, and a final body-targeted cut proves it is still destructible — real OS processes over QUIC, exact-replay verified, deterministic under 2% loss.

T23 outcome — G3/G4 integrated acceptance, increment 1 (`spall_voxel` + `spall_sim` + `spall_server` + `spall_client`): the `separated-regions` scene (`Scene::SeparatedRegions`) is one volume bounded to 256 x 128 x 256 m — the G3 operating envelope — holding two independent collapsible bridge structures, a "west" region at the origin and an "east" region offset by `+72,+0,+72` cells. Only the two regions plus the ground between them are resident, so the terrain occupancy grid and collider rebuilds are bounded by the structures, not the world box. `Scene::player_spawns` replaces the previously unconditional `WALK_ARENA_SPAWNS`: each player scene now supplies its own per-slot spawn table, and `SEPARATED_REGION_SPAWNS` alternates connecting clients between the two regions so they start geographically separated. The `t23-g3` scenario drives a multi-region collapse (one column cut per region detaches that region's beam and no other) plus a `--late-join` replica that converges after six edits, headless and at `--loss-percent 3`, with committed-stream replay and body-at-rest checks. Increment 2 adds the harness `restart_check`: after the run the server is stopped and a fresh `sandbox-server --serve --save` is cold-started over the same `world.db` — recovery from the shutdown checkpoint plus the durable journal, no edit replay from world creation — and both the recovered server and a fresh `--late-join` client against it must reach the agreed topology hash. This is the first T23 slice; resident-cache eviction, traverse-away/back, the full persistence crash-injection matrix, the join-duration budget, and the whole G4 eight-client workload + soak are enumerated open in `docs/reports/G3.md`.

T23 increment 12 tightens the live residency contract after integration review.
Backing bricks are digest-validated before insertion, preserving atomic logical
membership on reload failure. Client reload requests have a global per-step
ceiling and completed-load/budget-miss accounting. The authoritative terrain
hash remains cache-placement-independent, while a separate resident-terrain
dirty hash invalidates the client's derived prediction collider whenever
geometry is evicted or restored. The traversal harness now gates on real
eviction, completed reload, an edit dependency gap while evicted, and measured
outbound/return crossings; a fully resident fallback cannot pass that fixture.

## Collision and character physics

T06 compares native voxel colliders with deterministic merged-cuboid compounds for static and dynamic volumes. Verify concave contacts, edit/rebuild cost, and mass handling on the pinned Rapier version. Avoid dynamic concave triangle-mesh colliders; the Rapier documentation cautions against them. Terrain contains tunnels/overhangs, so heightfields are insufficient. [Rapier collider guidance](https://rapier.rs/docs/user_guides/rust/colliders/).

Exact occupied-space compounds are the correctness baseline. A single convex hull over a hollow building would block its rooms and is unacceptable. Approximate far collision is permitted only where it cannot affect gameplay and before promotion to a fully simulated area. Keep collision materials separate from visual palettes where the adapter requires it.

T06 outcome (`spall_physics`, pinned `rapier3d 0.35.3`): the **merged-cuboid compound** is the selected representation for both terrain and dynamic bodies. Versus a native `parry` `Voxels` shape it rebuilds ~150x faster after an edit, stops a fast CCD projectile the voxel shape lets tunnel, and matches the analytic mass/COM/inertia exactly, while a deterministic greedy box decomposition keeps the occupied set exact. Its failure mode — one box per cell on highly fragmented occupancy — is bounded by a per-body primitive budget with a conservative coarse-downsample fallback (never a hull, never dropped mass). Full rationale, measurements, and the coarse-fracture policy are in [`docs/collision-decision.md`](collision-decision.md). `spall_physics` depends only on `spall_voxel`; Rapier types stay inside its `world` / `collider` modules.

Use a capsule character controller with grounded state, gravity, jump, slope/step constraints, swept movement, and server-authoritative interaction with dynamic bodies. Ground input selects horizontal velocity; takeoff preserves that horizontal momentum until the capsule regains ground, with no mid-air steering or braking. The shipped arc uses a `5.7 m/s` launch and `16 m/s²` character gravity: about a `1.0 m` apex with roughly `0.71 s` airtime. The authoritative capsule has an `80 kg` effective contact mass; its sweep transfers impulses to dynamic bodies, whose exact mass / COM / inertia come from the fine material voxel grid, so a larger same-density block is harder to push. Replica colliders remain query-only and never apply client-side impulses. Physics queries need up-to-date collider revisions. Enable CCD selectively for fast damaging bodies; test tunneling and contact-energy thresholds before enabling impact-triggered destruction.

Other players' capsule snapshots are kept per entity in `ReplicaWorld::remote_players` (interpolated like far bodies, dropped after 2 s of silence) and drawn by the game's `PlayerView` (sandbox: `player.spvox`, clip chosen from speed and swimming). Shift sets `BUTTON_SPRINT`, which the shared character kernel turns into run speed on both server and client. The same kernel step advances `CharacterState::endurance` (`0..=1`): sprinting on the ground drains it (about 13.5 s from full), resting regains it (10 s), walking regains it at half that rate, steady swimming drains it slowly and treading water does not. At empty the player is exhausted and falls back to walk speed until endurance passes 30%. The player's `MotionSnapshot` carries `endurance` and `exhausted`, so the predictor reconciles against them; the client draws a bar while it is below full and plays breathing (`spall_client::breathing`: calm / exerted below 60% or while sprinting / exhausted, one crossfaded recording per state, `SPALL_BREATHING_LOG` prints transitions). Client player movement is predicted and reconciled. Remote bodies initially use snapshot interpolation plus kinematic collision proxies. They never gain authority from a client's local collision. Later local dynamic extrapolation is optional only if correction tests show a benefit.

T19 outcome — headless core (`spall_physics::character` + `spall_sim::player` + `spall_client::predict`): a player is a **kinematic capsule**, not a `spall_sim` body — it has no voxel volume, never splits, and never enters the dynamic-body set, so it does not touch the edit / split / collider path. It nevertheless carries an explicit effective contact mass for server-side momentum transfer. `spall_physics::character::step_character` is the one **pure** movement kernel (ground input → world-frame wish velocity, preserved airborne horizontal momentum, gravity, rising-edge jump, post-move vertical clamp) and both sides run it: the server uses `PhysicsWorld::sweep_character_pushing_excluding`, which wraps Rapier's `KinematicCharacterController` and solves contact impulses against dynamic bodies; `spall_client::predict::ClientPhysics` runs the non-pushing sweep against replica terrain plus query-only, server-pose-driven mirrors of detached-body voxel colliders. These mirrors affect only the character query; they never simulate body motion or gain client authority. Physics is not lockstep (`docs/protocol.md`), so this buys only bounded agreement — `PredictedPlayer` keeps a bounded input history, snaps to each authoritative `MotionSnapshot` at its `acked_input`, and replays the still-unacknowledged inputs; the residual is reported as a bounded correction. The player capsule advances in `Simulation::tick` **after** `step_physics` (so the sweep sees the broad-phase BVH that tick's collider rebuilds refreshed) and before snapshot extraction. A committed edit within `INVALIDATION_MARGIN_M` of a player bumps its `movement_epoch` and depenetrates it server-side; the client, on any terrain-hash change near its path, rebuilds `ClientPhysics` and rebases prediction on the last authoritative state rather than replaying through geometry that no longer exists. Held input is reused for a 250 ms window then cleared to neutral, so a lost "button up" cannot leave the player walking or holding jump. The player id is a reserved band (`spall_core::player_entity_for(slot)` — `1 << 48 + slot`) both ends derive from the session slot, so no wire record was added; player state rides the existing 20 Hz `MotionSnapshot` with `body =` the player entity and `acked_input =` the last consumed input sequence. `spall_server::serve` gives each connecting client a capsule and reads `InputFrame` datagrams (one applied per player per tick, redundant `recent` copies recover a dropped frame). Moving-body **crush** outcomes are handled by the separate T21 contact-damage path.

The interactive launcher also has an explicit testing-only escape hatch:
`--client-authoritative`. In that mode `ClientPhysics` promotes replicated body
colliders to locally stepped dynamic bodies, computes mass/COM/inertia from the
occupied fine voxels (using the playground's current uniform `2000 kg/m³`
density), applies the 80 kg player push impulses locally, owns playground
emitter timing, and supplies body poses directly to rendering. Server player
and body motion continues as a shadow simulation but cannot correct the local
runtime poses. Replicated topology remains the source of terrain and body voxel
shapes; on a server topology revision, local mass/COM/inertia are recomputed
from the new voxel grid as well. This mode does not transfer topology/edit
authority and is never a multiplayer protocol mode. With the flag absent, the
normal server-authoritative path above is unchanged.

Multi-region physics (several rebased physics worlds with body transfer) is not implemented: the prototype was removed 2026-10-20 (see git history). Only the single-world origin shift (`PhysicsOrigin`) remains; a larger world with widely separated players needs a new design under G5.

## Renderer and visual target

Separate geometry visibility from lighting. The initial renderer rasterizes greedy-meshed surfaces for both terrain and transformed voxel bodies. Merge faces only when material, face direction, and relevant lighting/AO attributes match; verify AO interpolation and triangle diagonal choices. Generate faces using neighbor samples across bricks, including local-volume brick seams. Use material colors and procedural world/local-coordinate detail initially; texture atlases are unnecessary.

Implement in this order:

1. Camera, depth, correct face winding, opaque materials, frustum culling, bounded GPU mesh uploads, reusable buffers, resize/device failure handling. **(T05)** `spall_mesh` owns culled/greedy face generation with an AO-aware merge rule and a `spall_jobs` halo token; `spall_render` owns one opaque WGSL pipeline (CCW front faces, `Depth32Float`, back-cull), a free-fly `Camera` with Gribb–Hartmann frustum culling, a growing reusable vertex/index buffer with a byte budget, and an offscreen `capture_scene` writing shaded plus normal/depth PNGs. Cell size stays 0.25 m; no revision proposed.
2. HDR linear-light rendering, physically based roughness/metalness shading, sun and sky, cascaded shadows, contact AO, tone mapping with fixed test exposure. **(T12)** `spall_render` owns four stable 2048² sun cascades, a `Rgba16Float` opaque pass with GGX direct lighting and mesh-derived contact AO, and an explicit ACES-fitted tone-map pass into a single sRGB target. Captures emit shaded, albedo, normals, linear depth, cascade/shadow visibility, and roughness views. Moving bodies use their world-transformed vertices in both opaque and shadow passes. On Windows the default is D3D12; `SPALL_WGPU_BACKEND=vulkan` is also an accepted backend (ENG-60, fixed): a naga/NVIDIA-driver defect in how a vertex shader's dynamically-indexed `array<vec2<f32>, N>` fullscreen-triangle constant table interacted with a fragment-stage uniform-buffer read crashed `create_tone_pipeline` on pipeline creation, not the comparison-depth-array shadow path; `shaders/tonemap.wgsl` generates that position with index arithmetic instead, and the full `capture_gpu` suite plus a 1920x1080 six-view capture pass on both backends with pixel-identical output (see `docs/reports/ENG-60.md`).
3. Early lighting prototype: camera-local occupancy/material clipmap in a fixed-size 3D texture/brick atlas; compute voxel ray traversal for low-resolution diffuse indirect light and soft visibility. Initial near volume: 128 cubed at 0.5 m, covering 64 m. Coarse occupancy can leak or over-occlude thin walls: this is a measured risk, not accepted correctness for collision.
4. Dirty-region updates for terrain edits and both old/new AABBs of moving objects. Clear/rebuild overlapping occupancy correctly; removing one object must not erase another. Emissive sources contribute to lighting. Limit bounce count initially to one diffuse bounce.
5. Temporal reprojection with depth/normal rejection, neighborhood clamping, disocclusion handling, and history invalidation after edits. Denoise and composite; reserve full-resolution raster silhouettes even when lighting is lower resolution.
6. Expand quality/range only after G2: multiple clipmap levels, better diffuse visibility, local lights, reflections, and distant LOD.

R3 (ENG-96) adds two direct-light contracts to the shared renderer (`docs/reports/ENG-96.md`): sun shadows are cascaded maps with normal-offset bias and physically-scaled PCSS penumbrae (the sun's angular size is an `Environment` parameter), and skylight is *visibility-aware* -- a derived, client-local occupancy grid in the frozen 128 cubed / 0.5 m layout feeds a six-direction sky-visibility pass, so enclosed rooms receive no outdoor ambient; unknown (non-resident) space blocks rays rather than counting as sky.

ENG-127 adds a procedural camera-oriented sky background in both the interactive game and editor viewport: environment-colored horizon/zenith, a sun disc aligned opposite `Environment.sun_dir`, and quantized procedural clouds. It is a render-only effect; it does not create world objects, affect sky-visibility lighting, or persist state. GPU capture evidence is recorded under `.local/runs/eng-94-parity/skybox-daylight.png`.

The clipmap is a derived GPU lighting cache, not the world format. Do not require experimental hardware ray tracing or a sparse voxel octree for the initial renderer. Optional future hardware acceleration must preserve a supported baseline and be justified by captured evidence.

Check indoor light leakage, emissive bounce, moving debris shadows, rapid edits, and motion ghosting. AO plus bloom alone does not pass G2. The exact indirect-light sampling/denoising implementation is a graphics integration decision in T13, not an invitation for separate agents to invent incompatible render pipelines.

## Streaming and lifetime

Track three distinct ranges: render residency, physics interaction, and structural dependencies. Structural dependency range is not capped to camera visibility. Server active regions are the union of player neighborhoods and active body trajectories, with hysteresis and safety margins. Predictively load collision ahead of fast bodies. Stop/defer entry into unknown collision rather than treating it as empty.

Brick states: absent -> requested -> resident -> dirty -> checkpointed -> evictable. Mesh, collider, and lighting readiness are separate versioned states. Persist edits before eviction. Do not regenerate over modified bricks; tombstones are needed even when the result is entirely air.

Storage partitions index data; they do not own indivisible physical objects. A body spanning partitions has one authoritative identity and geometry owner, plus spatial index references. Relevance uses its bounds, not just its centre.

Begin G1/G2 with all scene geometry resident. G3 introduces eviction against the same invariants. Distant render LOD never changes authoritative voxels, collision, support, or replicated destruction outcomes.

Per-brick terrain collision is the current provisional default: each resident
solid terrain brick owns one derived fixed physics collider from its exact
voxel revision, even when residency eviction is disabled. The authoritative
terrain remains one volume; this creates no per-voxel entities or bodies.
Eviction retires only the evicted brick's collider, and validated reload or a
committed edit republishes current collision at the owning tick boundary.
The legacy whole-terrain collider remains an explicit server comparison mode
(`--whole-terrain-collider`), and it switches to per-brick before any terrain
eviction. Collision mode is rebuilt from voxel state on recovery, not stored
as a runtime handle. See ENG-80 and the G4 rubble-lane report for the
user-directed decision and measured timing limits.

T18 implements the shared policy in `spall_voxel::residency`: server and client
hosts account resident brick count and dense material bytes against explicit
ceilings, apply an enter/retain hysteresis band, and choose only clean,
durable, unpinned LRU entries for eviction. Pin counts cover overlapping users
without transferring ownership. `CollisionReadiness::admit_sweep` checks the
complete swept local-space AABB against versioned ready bricks and returns a
bounded missing set; non-finite or oversized sweeps fail closed.

`spall_server::residency` binds that policy to the authoritative world and the
existing `StoredBrick` format. Dirty candidates are encoded and durably
acknowledged by a `ResidencyBacking` before removal; a failed write leaves them
resident. Explicit `KnownEmpty` loads become resident air, while `Unavailable`
stays absent. Compact per-brick face/anchor metadata survives voxel eviction,
but never decides support alone: `resolve_structure` repeatedly builds the T07
graph in `ResidencyMode::Streamed`, loads every `Support::Unknown` dependency,
and either returns a fully resolved index or the exact pending keys. Dynamic
body bricks are pinned complete, and `BodySpatialIndex` maps every intersected
world partition to the same stable body ID using its rotated bounds. The client
uses the same cache policy through `ClientResidency`; an evicted dependency
re-enters through the existing baseline/repair path.

## Persistence and recovery (T16)

`spall_store` (`-> spall_protocol`) owns the durable save schema and SQLite I/O
only, never simulation objects. It exposes: a versioned save schema
(`STORE_SCHEMA_VERSION`, the one save version shared with the ecology
formats; a `PRAGMA user_version` guard rejects any other database untouched, with an
"incompatible save" error), zstd-compressed brick payloads with one-brick-bounded decode, a
single WAL `Writer` that verifies `journal_mode=WAL` + `synchronous=FULL` on
open and returns a `DurableThrough` only after a successful `COMMIT`, checkpoint
publication (all body/brick rows + the journal cursor + a `complete=1` marker
in one transaction), and recovery (latest complete checkpoint + contiguous
CRC-verified journal suffix; interior corruption truncates the replay, is
reported, and the previous checkpoint is offered as a fallback).
`fault::{CrashPoint, FaultPlan}` inject controlled crashes around the commit
boundaries and disk errors that roll the transaction back so the API never
reports a save it did not make durable.

The authoritative-state to save-record conversion lives in the integrator:
`spall_server::persist` provides `capture` (`SimWorld` to `Checkpoint`),
`restore` (`Checkpoint` + durable journal suffix to a fresh `Simulation`, with
the material-manifest hash checked), `journal_records`, and the 30 s /
clean-shutdown checkpoint cadence wired into `sandbox-server --serve --save`.
It sits in `spall_server` — which already depends on both `spall_sim` and
`spall_store` — rather than adding a `spall_sim -> spall_store` edge that would
pull the vendored SQLite C build into the otherwise pure simulation crate.
`spall_sim` already emits the `spall_protocol` records the journal stores
(`journal.rs`); T16 adds only additive restore hooks there
(`SimWorld::resume_registry` / `insert_restored_body` / `replay_transaction` /
`apply_pose_batch`, `Simulation::from_restored`) and
`spall_voxel::Brick::restored`, so its "owns conversion between authoritative
state and protocol records" responsibility is unchanged.

Only topology transactions are journalled (seq = the simulation's
`JournalSeq`), each carrying its participant body snapshots; periodic 20 Hz
pose-batch journaling is deferred, so a crash rewinds body motion to the last
checkpoint / last topology-record participant state, which the
`docs/protocol.md` durability model permits. `restore` replays a `SplitOff`
from the transaction's own canonical fill runs and the participant snapshot (no
`spall_structure` re-run) and resumes the id counters past everything the
suffix consumed, so a post-restart edit still commits with fresh ids.

## Live late join, repair, reconnect (T17)

`spall_protocol::baseline` adds `BaselineWorld` — a postcard payload carrying
**authoritative geometry only**: every resident brick of every volume with its
real revision and material layer, plus the owner (terrain / body). Motion is
excluded; a `MotionSnapshot` keyframe follows the catch-up barrier. Because the
payload carries real revisions, a late-join replica reaches the exact canonical
topology hash with no follow-up edit replay and no repair.

`spall_server::baseline` captures a `BaselineWorld` from the live `SimWorld` at
the current tick, chunks it into hashed `BaselinePart`s under the 1 MiB
bulk-part / 64 MiB assembled ceilings, and builds `BaselineBegin` /
`BaselineEnd` with `journal_cursor = J`. `brick_repair_patch` builds a
one-brick authoritative patch from a `RepairRequest`.

`spall_server::serve` runs a per-client `LateJoin` bridge alongside the tick
loop. A joining replica sends a `BaselineAck` sentinel (`transfer_id == 0`); the
server captures a baseline, streams `BaselineBegin` (control) + parts (a bulk
stream) + `BaselineEnd` (control), and moves that client to a `Joining` phase
whose committed transactions are buffered in a bounded catch-up queue instead
of broadcast. On the client's real `BaselineAck` the queue is flushed in commit
order, a current motion keyframe for every body follows, and the client goes
`Live`. Already-connected clients never pause. A catch-up queue past
`catch_up_cap` cancels the transfer and re-captures a fresher one; after
`max_join_retries` the joiner is dropped with an explicit goodbye while everyone
else keeps running. A brick `RepairRequest` is now answered with a one-brick
authoritative baseline patch (full parity, revision included) — the T10
`CellRun` reply healed cell content but not revision. Reconnect: `Inbound::Joined`
tracks the highest session generation per connection slot, and an
`ActionRequest` / `RepairRequest` from a superseded generation is rejected
("expired session"). `spall_client::net --late-join` pulls the baseline over
the bulk transfer before touching the replication stream and applies a
mid-session `BaselineBegin` as a repair patch. No new external dependency; no
change to the frozen wire record set (the sentinel reuses `BaselineAck`).


### ENG-118 implemented vegetation ownership

The sandbox worldgen factory adds game-authored procedural vegetation after base terrain generation, before authoritative collision/world creation. CPU-only spall_ecology::living owns stable-ID plants, biome/soil eligibility, retained connected wood paths and a plant season clock. Simulation owns it; growth is acknowledged only through the normal edit pipeline. Wood remains ordinary destructible terrain/body cells. Non-solid leaf/ground geometry is derived from server keyframes and shares opaque lighting/shadows. Atomic persistence includes full state after topology plus small clock rows. No hidden supports, per-voxel entities or alternate physics. Rules, budgets and remaining global-season/moisture work: docs/reports/ENG-118-vegetation.md.

### Ground cover, underwater plants and lighting scroll (ENG-189, ENG-191)

Vegetation has two owners. **Living** plants (trees, reeds, ferns, flowers, underwater kelp and corals) stay `LivingState` organisms owned by simulation as above. **Ground cover** is stateless: `spall_ecology::cover::ground_cover` is a pure function of seed, cell, soil, moisture, temperature, biome, season and water depth, read from a coarse climate raster (`CoverField`) that `LivingState` carries and every `VisualFrame` repeats. No per-blade record exists, is saved or is sent. The only cover state is the destroyed-cover record (`LivingState.cleared`: column, time of destruction), which the server writes when an edit reaches topsoil and the client uses to hold back and regrow cover (bare, then small, medium, full). The client derives cover geometry on a worker thread, two-pass LOD, and never mutates terrain.

The water solver may keep deep still ocean regions dormant (`ColumnConfig::lazy_ocean`): a per-column record revived with exact volume when touched. Dormant regions are an allocation detail; the replicated and saved water state is unchanged.

### Sun shadow cascades are cached (renderer)

Each of the four sun cascades is a light-space box around a sphere centred on the camera, sized to hold everything its slice of the view frustum can show in any direction. The box therefore depends on the camera's position and the sun alone: turning does not change it, and moving changes it only when the centre crosses a snap step of about 8% of the cascade's reach (a multiple of its texel). Each cascade keeps its terrain-only depth between frames (`shadow_static` layers) and redraws the terrain only when its box moved or the terrain meshes changed; every frame the kept depth is copied into the sampled layer and the moving casters (bodies, instanced cubes, vegetation) are drawn over it. Measured on a 4096-cell natural world (about 3,000 resident chunks, 4.4 M triangles, one development GPU): terrain draws in the cascades were 10.3 ms of a 16.4 ms frame; standing 16.4 -> 8.1 ms (61 -> 123 fps), walking a scripted loop 19.5 -> 14.1 ms (53 -> 73 fps). Resolution is about 30% coarser than the frustum-fitted cascades it replaced; far cascades redraw every few metres of travel, the nearest about every metre.

The renderer's camera-local lighting cache relights in slices: sky visibility once, then the bounce in interleaved strides sized from measured GPU cost, and a camera recentre scrolls the cache and relights only the entered strip. This changes cost, not the cache contract above (derived, camera-local, never authoritative). Editor world-generation previews are drawn from the same `ColumnMap` the server builds; layers are diagnostics, not a second generator (`docs/worldgen.md`).

### Occlusion culling, distance fog and the frame limiter (client renderer)

About four in five terrain chunks inside the camera's frustum show no pixel (underground shells, the far side of hills). The camera pass skips chunks judged hidden by GPU occlusion queries (`spall_render::occlusion`): after the opaque pass, the bounding boxes of up to 192 chunks a frame are drawn against that pass's depth, each inside an occlusion query; the counts are read back asynchronously over three slots, and each chunk keeps a verdict. A chunk with no verdict (new, edited, back in the frustum, or after the camera jumps) is drawn; a visible chunk is drawn and re-tested every 16 frames; a hidden chunk is skipped and re-tested every few frames, and drawn anyway once its verdict is 90 frames old. The test is exact (the box covers the chunk), so it can only draw too much, never leave a hole, apart from the few frames a newly uncovered chunk waits for its next test. The sun cascades are not culled this way. Measured on the 4096-cell natural world at 1928x931: 61% of chunks culled, standing frame 10.6 -> 7.3 ms, scripted walk median 10.5 -> 9.6 ms. `SPALL_NO_OCCLUSION=1` turns it off.

Distance fog fades the outer third of the view radius (62% to 97%) into the sky's horizon colour, in terrain, water and foliage, so chunks that stop being drawn at the edge do not pop (`SPALL_NO_FOG=1` off). The uncapped frame loop is limited to `SPALL_MAX_FPS` (default 240, 0 for none): without vsync the loading screen ran at 3,000+ frames a second and starved the rest of the PC. Terrain level of detail beyond the near chunks is not implemented.

Lighting sweeps (`spall_render::sky_visibility`) were found to spend most of their GPU time in thin slices. A sweep is cut into 128/N-layer slices so no frame carries it all; a slice of one layer has too few active threads to hide memory latency and costs about what a four-layer slice does, but the cost model (cost proportional to layers) kept asking for more slices, so a walk's recentre sweeps ran at 128 slices, about four times the GPU work of 32. The slice count is now capped at 32 (four layers, a full workgroup row): in a dense-forest walk at 1928x931 the median frame went from 16.8 to 14.2 ms (three runs each) and sweep steps fell by two thirds. The bounce also casts fewer rays for cells far from the camera (at most 42 beyond 16 m, 18 beyond 32 m): 90th-percentile frame 18.9 -> 17.9 ms, the median within noise; a lamp 45 m away lights the floor at about 70% of its near brightness.

Lighting quality (`spall_render::sky_visibility::LightingQuality`, client key `F7`, menu button, `SPALL_LIGHTING=off|low|medium|high`, scripted step `lighting:low`) sets how much of the camera-local lighting cache is computed; it changes the cache, never the world. Off: flat ambient light, no cache. Low: skylight visibility only (no bounce), re-centred after 16 m of drift, sun relit every 12 degrees. Medium: bounce with 42 rays per cell, 12 m drift, 8 degrees. High (default): bounce with all 66 rays, 8 m drift, 5 degrees. Measured on a scripted walk through the dense forest of the 4096-cell natural world at 1928x931, where a lighting sweep step ran in about two frames out of three: Off 8.8-11.7 ms a frame, High 12.9-14.8, Low 12.6-13.4, Medium 12.0-17.1 across runs (run-to-run noise is about 1.5 ms, so Low and Medium are not clearly separated from High; Off and Low's missing bounce are). The bounce slice cost did not fall with the ray count; the cost per sweep step is dominated by the sky pass and the slice dispatch, not the rays.

ENG-122 reconstructed support diagnostic (2026-10-04, removed grid solver): see [reports/history-architecture.md](reports/history-architecture.md).

### Terrain level of detail (client renderer)

Chunks far from the camera are meshed from a strided sample of their bricks
(`spall_mesh::build_brick_mesh_lod`): blocks of 2 cells beyond 40 m, 4 cells
beyond 72 m (3D distance from the camera to the chunk centre; 6 m hysteresis so
a chunk standing on a line does not flip). A block is solid when at least a
quarter of its cells are, so a two-cell trunk survives however it is aligned
and flat ground rises by at most a quarter block; the fog (62%-97% of the view
radius) hides the change. The coarse mesh reads exactly the bricks the fine one
does, so its staleness token is unchanged. A chunk whose neighbour is meshed at
another level closes that side (the neighbour reads as open air there), so each
chunk carries its own skin and no crack opens at the seam.

The client keeps each chunk's level and closed sides with its cached mesh and
rebuilds when either changes. A walk crosses a shell of hundreds of chunks at
once, and each change is a rebuild, an upload and (through the closed sides)
rebuilds of the neighbours, so at most 24 chunks whose bricks are unchanged
change level per rebuild pass, nearest first; the rest keep the level they
have and catch up on the next passes. Such an upload is flagged detail-only and
the renderer neither invalidates the cached shadow cascades for it, nor does a
change that misses a cascade's box (the cascades are invalidated per change
bounds now, not by one global generation). `SPALL_NO_TERRAIN_LOD=1` turns the
levels off.

Measured on the natural 4096 world, forest walk at 1928x931 (release, one GPU,
same session): 104 fps median with levels against 87 without (frame 9.7 ms
against 11.1 ms), terrain triangles 4.1M to 1.1M. Without the per-pass budget
and the shadow change the same walk ran 51-56 fps against 62-78: every level
change forced a shadow redraw and the worker rebuilt about 1000 chunks per 8 m
walked. Standing: 8.1 ms to 7.5 ms in the open and 8.1 ms to 7.1 ms in forest.
The runs are noisy by a few ms; GPU capture and other hardware are unrun.

The shadow cascades follow the sun in steps of half a degree (`SHADOW_SUN_STEP_DEG`
in `pipeline.rs`): the day clock moves it a tenth of a degree a second, and
reframing the maps for each of those slid the texel grid across the terrain and
made shadows on slopes shimmer once a second. Held for several seconds they stay
still and their cached terrain depth stays valid.

Foliage shadows are cached like the terrain's: each cascade keeps a merged
layer (the terrain depth plus the foliage drawn over a copy of it) that is
redrawn only when the terrain depth was redrawn or the foliage instances
changed, and the moving casters are drawn over it each frame. Before, every
frame redrew all the foliage in each cascade, about 1.8 ms of GPU time at a
forest lake. Measured on that walk (natural 4096, seed 4, 1928x931, release):
median frame 6.6 ms to 5.4 ms, shadow GPU 1.9 ms to 0.1 ms, p90 and the worst
frame per second unchanged. Costs 3 more 2048x2048 depth layers.

Water frame spikes (found with `SPALL_WATER_TRACE=1`, which logs client water
work over half a millisecond on the frame trace's clock, and `SPALL_NO_WATER=1`,
which generates the world dry). On the lake walk a dry world had 2-8 frames over
20 ms and the wet one 250-270, with the median unchanged. The cause was not the
client's water upload (2 of 46 slow frames contained 2 ms of it) but the
server's water step: it ran on rayon's global pool, taking every core for its
3-10 ms, twenty times a second, and the client's render and driver threads lost
their cores (acquire and submit stalls). The water worker now has a pool of its
own, half the cores (at most 8, `SPALL_WATER_THREADS` overrides): 50-60 slow
frames at 4 or 8 threads, 11 at 1 thread but then the water falls minutes
behind real time. Separately the client eased the water surface every frame
(about 0.8 ms of the main thread at 200 fps, 8.5 s over the walk): it now eases
at 60 Hz (1.8 s). What remains over a dry world is not in the water traces; the
frames show acquire and submit stalls.

Slow frames at the lake (`SPALL_FRAME_CSV=1` prints one line per client frame:
clock, handler, acquire and submit milliseconds, water rectangles uploaded;
`SPALL_FRAME_TRACE=1` now also prints GPU pass times over 5 ms and terrain
uploads over 1 ms). Over about 20,000 frames of the lake walk, 5% took over 8 ms
and about 1.2% over 16 ms (a dry world: 2.9% and 0.14%). Two causes, neither the
client's water or terrain uploads: (1) the bounce lighting sweep, in 2.7% of
frames, each carrying a slice of median 5 ms GPU (p90 10, worst 66) that makes
the next frame wait 7 ms for the swapchain: 41% of the frames over 8 ms and
25% of those over 16; (2) the server's water step competing for cores with the
render and driver threads: the frames over 16 ms with no sweep fell from 347 to
133 to 27 to 1 going from 8 water threads to 4, 2 and a dry world, but runs
vary by about half a percentage point, and fewer threads let the water fall
behind (2 threads: 48 s behind real time in 130 s). Not established: lowering the
server's process priority (`SPALL_SERVER_PRIORITY=low` in `xtask play`).

The water field's recentre no longer stalls a frame on its upload. A replacement
field (`GameRenderer::replace_water_field`) goes to a second texture 64 rows a
frame (16 frames for 1024 rows) while the old field keeps drawing, then swaps;
rectangle updates in the meantime are for the new field. A recentre used to cost
about 25 ms in one frame (2-7 ms CPU rebuild, 21 ms upload); it is now 8 ms
(5.5-6.3 ms of it the CPU rebuild in `ColumnSurface::recentre`, still in one
frame) and a millisecond a frame for 16 frames. The first install of a field is
still immediate (17 ms once).

Water far from every player steps less often, and catches up when one returns
(`ColumnConfig::throttle_from_cells`, `catch_up_substeps`; the server sets
them for generated worlds, `SPALL_WATER_THROTTLE=off` turns them off). The sim
tells the water where the players are each tick (`ColumnWorld::set_focus`). An
active region farther than 160, 320 or 640 m from the nearest player steps one
step in 2, 4 or 8; on the others it rests: it exchanges water with its
neighbours through its edge, as a sleeping region does, but does not
redistribute, so far water runs in slow motion and no water is made or lost.
The time a region rested is its debt (`region_owed_s`); once a region is near
again it spends up to 24 substeps a step, nearest and largest debt first, four
regions a step, stepped alone for up to 4 s of its debt, until it has caught up.
A region that is quiet for its sleep count still sleeps (counted in steps it was
stepped), and a draining lake is never quiet, so it never sleeps early; with no
player nothing is throttled. Tests (`tests/column_throttle.rs`): a far lake
drains late but finishes and conserves volume to 1e-9; no focus, a focus on the
lake, or the bands off change nothing bit for bit; after 40 s far and 40 s near,
a lake is at the level of one never throttled (29.17 against 29.17 cells; one
that stayed far was at 29.54) and its debt went from 35 s to 0.2 s. At the lake
walk (natural 4096, two runs each): frames over 16 ms 0.56% and 0.41% with it
against 1.58% and 0.91% without (a dry world: 0.14%), the water's lag behind
real time 3 s against 10 s at 130 s, and 190 of about 260 active regions resting.
More regions stay active (they go quiet at a slower pace), which costs memory
bandwidth rather than CPU. A catch-up hitch when a player arrives at a far lake
has not been measured in the game.

Debug report (`F9`, or "Copy debug report" in the `F10` menu): copies a plain
text report to the clipboard (Windows `clip`, `pbcopy`, `xclip`) and saves it to
`spall-debug-report.txt` in the system temp directory. It holds the build (git
commit and branch when built, command line), the player's position, cell and
region, camera mode, window size, the last 2400 frames' handler time (mean,
p50/p90/p99, worst, share over 8/16/33 ms), the HUD's last line one field per
line (GPU passes, chunks, tris, lighting sweep, corrections), lighting settings
and time of day, resident and drawn chunks, the client's water field, the end of
the server's stderr log (when launched by `xtask play`, which sets
`SPALL_SERVER_LOG`; it carries the water status and slow-tick lines), the view
radius, fog, LOD constants and every `SPALL_*`/`RAYON*`/`WGPU*` environment
variable. A `report` step in `--shots` triggers it from a scripted run.

Water terrain refreshes run on the water worker. Refreshing a region's terrain
(a committed edit, or bricks becoming resident as a player walks) costs time in
proportion to every region the water holds, not the one refreshed (a probe in
debug: 30 ms at 196 regions, 163 ms at 784, 627 ms at 3025), and the tick did it
under the core lock after waiting for the step in flight: during a 7.7-minute
walk the server's ticks reached 0.4, 2.4, 2.2 and 4.9 s in `tick.water`. The tick
now captures the terrain and queues it (`pending_terrain`); the next step applies
it, and the refresh figures reach the tick's metrics a tick later. The same walk
afterwards: worst slow ticks 326 ms and 305 ms (the first is the opening tick),
none in the seconds. Active regions grow along the walk with or without the
far-water throttle (100 to 438 with it, 93 to 382 without, over about 480 s of
water time); with it the water ended 8 s behind real time, without it 26 s. The
status line's `last step 0.0 ms ... (0 resting)` lines are ticks with no new step
(the metrics are taken once), not a stalled worker. Why regions stay active along
a walk (live rivers and their springs, regions woken as bricks come resident) is
not investigated.

Why water regions stay awake (`SPALL_WATER_DIAG=1` prints, with each water
status line, how many active regions carry which largest flow and how many hold
a spring or drain). Standing still on a 4096 world, active regions climbed from
100 to about 360 in 500 s of water time and stayed near 330, with no walking.
Of them 200-270 carried a largest flow of 0.1-1 cells^3/s, just over the 0.1
sleep threshold; only 27 regions in the world hold a spring or drain and at most
9 awake ones did, so it is currents in channels, not the springs. The steady-flow
sleep (`SPALL_WATER_STEADY_STEPS=40`) never froze a single region (steady 0 all
run). Raising the threshold to 0.5 (`SPALL_WATER_SLEEP_FLOW`, now the generated
worlds' default): active regions stayed 53-78 standing and 51-83 walking the lake
for 508 s, a step about 1 ms (before 14-40), the water 1.9-2.5 s behind real time
(before 5-8 s). Flows under 0.5 cells^3/s (8 mL/s through the busiest pipe) stop
being stepped until something disturbs them, so the slowest creeping on slopes
and in channels freezes; rivers (1-100) keep running. Not looked at in game.

ENG-122 worker observability and transport profiling sections (2026-10-04, removed grid solver): see [reports/history-architecture.md](reports/history-architecture.md).

### ENG-128 day/night presentation clock

The sandbox's default sky, sun, moon, and environment lighting derive from the
authoritative simulation tick. One Spall day is 72,000 ticks at 60 Hz (20 real
minutes); tick zero is 06:00. Clients render the same clock from replicated
server ticks instead of maintaining wall clocks. The lunar phase advances by
one eighth per day over an eight-day cycle, from full moon through new moon and
back. Moon visibility follows its orbit and phase, including a subdued daytime
disc. Night ambient fill has a nonzero floor independent of moon visibility;
moonlight adds a small phase-dependent contribution.

The clock is presentation state, not persisted world state: a newly started
simulation begins at day one sunrise. Explicit environment selection and the
in-game cycle pause are visual overrides; resuming restores tick-derived
lighting. No protocol clock field is needed because the existing server tick
already supplies synchronization.

### ENG-130 regional terrain startup

Generated-world Run in game now installs terrain geometry within a radius of 10 bricks (80 m on each
axis from the spawn brick), with exact digests for the remaining logical terrain and complete body
geometry. Client residency retains the moving interest cube, prioritizes nearest missing bricks,
and admits at most four reload requests per step under existing cooldown and admission limits.
Defaults are 16,384 resident terrain bricks and 1 GiB resident dense-cell budget. The 8 GiB staging
ceiling remains an admission bound, not a preallocation. Scene runs retain their existing default.

Prediction requires resident collision geometry over the capsule's next bounded movement sweep,
including vertical velocity. It holds input prediction at an unknown boundary until geometry arrives;
the server remains authoritative. Rendering continues its existing incremental nearby mesh work.
Camera-only flying does not independently expand player interest in this implementation.

The authoritative server still generates, stores and simulates the entire world. Initial capture and
canonical hash/catalogue construction still visit all logical bricks, and distant edits may request
geometry even outside current interest. Global water/ecology replication is unchanged. This change
reduces initial terrain geometry transfer/client residency; it is not server region generation,
interest-filtered transactions, or distant visual LOD. Those require separate measured work.

## Animals

An animal is a plain server-authoritative entity: `CreatureRecord` (id, species
key, root pose, locomotion tag, clip phase). It is drawn from the species' SPVX
asset and owns no world voxels, so it is not part of any volume hash, journal
entry or transaction. `spall_sim` keeps the records (`creatures.rs`) and moves
them by sweeping a capsule against terrain with the same kinematic controller
players use (`creature_motion.rs`); the game's `CreatureDirector`
(`spall_server::creature_service`) decides where each one goes. Spawn and despawn
are replicated records, and positions travel in `CreatureMotionFrame`s; see
[protocol](protocol.md#creatures). Players and bodies do not collide with
animals. Worlds with animals cannot be saved. The design and the planned hunting
path are in [animal lifecycle](animal-lifecycle.md).

## Cabin interactions

The sandbox ships `fixtures/cabin`, an editor project with a fixed SPVOX shell,
four player starts, and separate bed, table, bench, lamp and door assets. The
scene importer places these into normal destructible terrain. Furniture is
modular for authoring; it has no runtime pickup behavior.

Quarter-metre doors use `spall_voxel::doors` to find a visible nearby hinge and
plan a +X-to-+Z panel turn. The hinge material records open/closed state. E sends
an interaction request; the server selects the door from its authoritative
player position. Ordinary staged geometry commits preserve damage, reject
obstructed turns, update collision, and replicate/journal the resulting cells.
The conservative swing box is checked for players and debris both when preparing
and validating a commit. Breaking the hinge disables the interaction.
