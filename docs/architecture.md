# Architecture and invariants

Status: proposed implementation contract. Numerical defaults are frozen after G1/G2. Multiplayer and full-world destruction are mandatory throughout.

## Workspace and dependency direction

Arrows below mean "depends on". Create crates when their first task needs them; avoid empty scaffold packages.

```text
spall_core       IDs, coordinates, ticks, units, material definitions
spall_voxel   -> spall_core                     storage, sampling, edits, queries
spall_jobs    -> spall_core                     bounded scheduling, result tokens
spall_mesh    -> spall_voxel, spall_jobs        surface generation, no GPU
spall_structure -> spall_voxel, spall_jobs      connectivity, support, split plans
spall_physics -> spall_voxel                    Rapier adapter, collision builds
spall_sim     -> spall_structure, spall_physics, spall_jobs, spall_protocol   authoritative state and tick order
spall_protocol -> spall_core                   explicit DTOs and codecs only
spall_net     -> spall_protocol                 Quinn transport adapter (Tokio; T09)
spall_store   -> spall_protocol                 checkpoint/journal bytes and indexes
spall_render  -> spall_mesh, spall_core            wgpu resources and render passes
spall_server  -> spall_sim, spall_net, spall_store, spall_jobs
spall_client  -> spall_net, spall_voxel, spall_physics, spall_render, spall_jobs
sandbox (example) -> spall_server, spall_client   game rules and executable entry points
xtask                                    process/scenario/build orchestration
```

spall_sim owns conversion between authoritative state and protocol records; persistence does not own simulation objects. The graph edge is refined from `spall_sim -> spall_structure, spall_physics` to add `-> spall_jobs, spall_protocol` (T08): staging is submitted to a `spall_jobs::Scheduler` and re-validated through a `JobToken` like any other off-tick result, and every commit emits a `spall_protocol::TopologyTransaction`. Both new targets are foundation crates (`-> spall_core`); no cycle is introduced. The client maintains a replica and prediction state; it never runs server-only structural decisions. Render input is an extracted immutable view of the replica, never a reference into a running server.

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

CPU jobs operate outside the tick on immutable data. GPU submission stays on the render owner thread. Network and disk runtimes cannot hold simulation locks or await inside a tick. Separate priority and byte budgets for generation, edits, collision, topology, and visuals prevent background work starving player actions.

## Editing and cross-brick atomicity

Every player/bot/console action becomes an intent with a request ID. The server determines the hit against its current authoritative geometry. A client-supplied hit point, material, target transform, or body ID is only a claim to validate.

Quantize approved cutting operations into the target volume's local integer coordinates. Version each integer brush algorithm; use checked arithmetic and explicit inclusion rules. A sphere removes cells whose centre has squared distance <= radius squared in specified fixed-point units. Never derive client geometry changes from independently evaluated floating-point world rays.

An edit plan records before/after brick data, revisions, ownership changes, support consequences, colliders, and body mass/transform changes. Multi-brick/multi-volume edits are one transaction. Prepare all required authoritative results, then swap at a tick boundary. Until ready, the old geometry/collision remains authoritative and the request is pending; local effects can preview the action without claiming a committed hole.

Conflict rule: jobs may overlap in preparation, but commits revalidate every read dependency. Conflicting older work retries or is recomputed in request order. After repeated conflicts, serialize the affected region through a bounded queue to guarantee progress. Moving-body edits are defined in the body-local frame sampled when the server accepts the action; at commit they apply to that same material location if the topology preconditions still hold.

Physics must never collide with a deleted wall or fall through a newly built wall after the authoritative transaction commits. Renderer uploads may lag briefly, but staging keeps an old consistent replica visible until its transaction can be presented. Expose revision lag and prioritize local collision/render updates.

T08 outcome (`spall_sim`): the whole path above is implemented for the all-resident G1 case. An accepted `EditIntent` is staged off-tick against an immutable snapshot (`stage_edit`) — deterministic brush plan, dry-run `EditOutcome`, `spall_structure` re-classification, and a `JobToken` over every brick read — then committed atomically at a tick boundary (`commit`). A stale token at commit means an earlier commit that tick touched a shared brick; the intent is recomputed and retried in request order, and a region that keeps losing that race is routed through a bounded serial queue (one commit per tick) so it still progresses. On commit the brush is applied to the live volume, every unsupported component (terrain) or every non-largest component (a free dynamic body) is copied into a new body **at its exact split-instant world location** with mass from the fine voxel grid and velocity `v_parent + omega_parent x (r_child_com - r_parent_com)` plus a one-time explosion impulse, and every affected collider is rebuilt in the same tick. A repeated `RequestId` returns the existing `ActionStatus` and can never cut twice or apply a second impulse. The collider budget from `docs/collision-decision.md` (`B = 4096` merged boxes, then a conservative same-resolution coarsen) is enforced here. Terrain-to-body and body-to-body transfer do **not** re-centre the child onto its COM: keeping the child's cells and transform identical to the parent's is the exact preservation the architecture requires, and `spall_physics` carries the off-origin COM. No replication, persistence, player movement, or contact-to-intent conversion is in T08.

T10 outcome (`spall_sim` replication surface + `spall_server` / `spall_client`): a committed `TopologyTransaction` is now **self-describing**. `spall_sim::commit` appends, after the `IntegerBrush` op and each `SplitOff`, explicit canonical `+X` `TopologyOp::CellRun`s — one group filling each child volume with its detached cells' materials, then the runs that set those cells to air in the source — so a replica reconstructs an exact split with no `spall_structure` (`docs/protocol.md`: "canonical source-cell ranges ... not a client rerun of ... support heuristics"). A split whose inline `CellRun`s would overflow one reliable control record is re-encoded (T17 increment 1 / ENG-64) as `[IntegerBrush, SplitOffBaseline*, SourcePatchBaseline]` — the child geometry and the source's post-cut affected bricks travel as compressed `spall_protocol::baseline::BaselineVolume` blobs inside the op list, so the durable journal and exact-replay reconstruct it unchanged; a split whose blob is still over `MAX_SPLIT_BASELINE_BLOB` (the G1 64-brick collapse) commits with marker-only `SplitOffBulkBaseline` / `SourcePatchBulkBaseline` ops (T17 increment 2 / ENG-64) plus one out-of-band `BaselineWorld` delivered to replicas on a bulk stream and journalled as a `TopologyBulkSplit` payload; the replica holds the marker transaction (`AwaitingBulkSplit`) until the blob assembles. `spall_sim` also exposes the per-tick server feed (committed transactions, `ActionStatus`, a 20 Hz `MotionPublisher`, a `RepairRequest` → `CellRun` responder). `spall_client::replica::ReplicaWorld` holds the last consistent view as plain `spall_voxel` volumes and applies each transaction **candidate-first**: it checks every `before` brick revision against the live replica (a `Revision::ZERO` entry means "was absent"), replays the ops into an isolated clone, verifies every declared `result_hash`, and only then swaps it in — a `before` gap yields `RepairRequest`s and no mutation, any op or hash failure is rejected whole. The applied-`TransactionId` set is the replay guard; the control `SequenceGate` only tracks the high-water mark so a catch-up delivery of an earlier transaction is still evaluated. An emptied body (source or child) is tombstoned and a late motion packet can never resurrect it. `MotionTrack` holds the two newest accepted states for interpolation and one newest pending snapshot for a body the replica has not created yet (or whose topology revision it has not reached). `spall_server::serve` runs the authoritative `Simulation` on a blocking thread behind a `spall_net` QUIC endpoint and fans committed transactions + `ActionStatus` to every client's reliable control stream in commit order, with motion as datagrams; `spall_client::net` is the headless replica client. Live late join and hash repair land in T17; player prediction remains a later task (T19).

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

T21 outcome — contact damage, increment 1 (`spall_physics` + `spall_sim::contact_damage`): after each `step_physics`, `PhysicsWorld::contact_impulses` reports every solver contact pair's accumulated normal impulse (N·s), world contact point, and normal — a read-only view, no mutation in a physics callback. `spall_sim::contact_damage::ContactDamagePolicy` is the pure filter from those impulses to bounded [`EditIntent`]s: a contact qualifies only when its impulse exceeds `impact_ratio ×` the striking body's own resting support impulse `m·g·dt` *and* an absolute floor, so a body at rest (impulse ≈ its weight) never fractures the floor under it; a per-region (brick) cooldown blocks repeat damage for `cooldown_ticks`; at most `max_intents_per_tick` cuts are emitted world-wide per tick with the rest dropped and counted (never queued); and a body created by a topology commit on the same tick is skipped (recursion guard) so a cut cannot cascade into further cuts in one tick. `Simulation::apply_contact_damage` resolves contacts to the authoritative terrain volume and submits the cuts with server-authored request ids in a reserved band (`1 << 62`); admitted cuts stage off-tick and commit on a later tick like any client edit. Increment 1 damages **terrain only**.

T21 outcome — region dormancy, increment 2 (`spall_physics` + `spall_sim::dormancy`): a settled detached body whose whole interaction region has gone quiet is *deactivated* — `PhysicsWorld::deactivate_body` removes its Rapier rigid body and collider (a reversible sibling of `retire_body`, keeping the `BodyId` slot and every rebuild parameter), and `SimWorld` keeps the authoritative `Body` record frozen (`sleeping = true`, zero velocity, pose held). Dormancy touches no cells, ownership, or damage, so `world_hash`, conservation, and the checkpoint set are unchanged — it is purely a step-cost optimisation. `spall_sim::dormancy::DormancyPolicy` is the pure decision layer: a body asleep and still for `settle_ticks` consecutive ticks with no active region (a player capsule or an awake body) within `wake_margin_m` of its bounding sphere is deactivated; a dormant body is reactivated when an edit targets it (immediately, through `Simulation::submit`) or a player/awake body comes within the margin (after `min_dormant_ticks` of hysteresis); deactivations and proximity reactivations are each capped per tick. `Simulation::apply_dormancy` is opt-in like `apply_contact_damage` and is not run by `tick`. `PhysicsWorld::reactivate_body` rebuilds the body in its original slot from the caller's grid and stored pose before any contact or edit, so a dormant body stays fully destructible. Body-on-body contact fracture is the next increment.

## Collision and character physics

T06 compares native voxel colliders with deterministic merged-cuboid compounds for static and dynamic volumes. Verify concave contacts, edit/rebuild cost, and mass handling on the pinned Rapier version. Avoid dynamic concave triangle-mesh colliders; the Rapier documentation cautions against them. Terrain contains tunnels/overhangs, so heightfields are insufficient. [Rapier collider guidance](https://rapier.rs/docs/user_guides/rust/colliders/).

Exact occupied-space compounds are the correctness baseline. A single convex hull over a hollow building would block its rooms and is unacceptable. Approximate far collision is permitted only where it cannot affect gameplay and before promotion to a fully simulated area. Keep collision materials separate from visual palettes where the adapter requires it.

T06 outcome (`spall_physics`, pinned `rapier3d 0.35.3`): the **merged-cuboid compound** is the selected representation for both terrain and dynamic bodies. Versus a native `parry` `Voxels` shape it rebuilds ~150x faster after an edit, stops a fast CCD projectile the voxel shape lets tunnel, and matches the analytic mass/COM/inertia exactly, while a deterministic greedy box decomposition keeps the occupied set exact. Its failure mode — one box per cell on highly fragmented occupancy — is bounded by a per-body primitive budget with a conservative coarse-downsample fallback (never a hull, never dropped mass). Full rationale, measurements, and the coarse-fracture policy are in [`docs/collision-decision.md`](collision-decision.md). `spall_physics` depends only on `spall_voxel`; Rapier types stay inside its `world` / `collider` modules.

Use a capsule character controller with grounded state, gravity, jump, slope/step constraints, swept movement, and server-authoritative interaction with dynamic bodies. Physics queries need up-to-date collider revisions. Enable CCD selectively for fast damaging bodies; test tunneling and contact-energy thresholds before enabling impact-triggered destruction.

Client player movement is predicted and reconciled. Remote bodies initially use snapshot interpolation plus kinematic collision proxies. They never gain authority from a client's local collision. Later local dynamic extrapolation is optional only if correction tests show a benefit.

T19 outcome — headless core (`spall_physics::character` + `spall_sim::player` + `spall_client::predict`): a player is a **kinematic capsule**, not a `spall_sim` body — it has no voxel volume, never splits, and never enters the dynamic-body set, so it does not touch the edit / split / collider path. `spall_physics::character::step_character` is the one **pure** movement kernel (input → world-frame wish velocity, gravity, rising-edge jump, post-move vertical clamp) and both sides run it: `PhysicsWorld::sweep_character` wraps Rapier's `KinematicCharacterController` (frozen tuning — walk/jump speed, `MAX_STEP_M ≈ 0.5`, slope-climb/slide angles, ground snap) against the authoritative collider world, and `spall_client::predict::ClientPhysics` runs the identical kernel against a collider rebuilt from the replica terrain. Physics is not lockstep (`docs/protocol.md`), so this buys only bounded agreement — `PredictedPlayer` keeps a bounded input history, snaps to each authoritative `MotionSnapshot` at its `acked_input`, and replays the still-unacknowledged inputs; the residual is reported as a bounded correction. The player capsule advances in `Simulation::tick` **after** `step_physics` (so the sweep sees the broad-phase BVH that tick's collider rebuilds refreshed) and before snapshot extraction. A committed edit within `INVALIDATION_MARGIN_M` of a player bumps its `movement_epoch` and depenetrates it server-side; the client, on any terrain-hash change near its path, rebuilds `ClientPhysics` and rebases prediction on the last authoritative state rather than replaying through geometry that no longer exists. Held input is reused for a 250 ms window then cleared to neutral, so a lost "button up" cannot leave the player walking or holding jump. The player id is a reserved band (`spall_core::player_entity_for(slot)` — `1 << 48 + slot`) both ends derive from the session slot, so no wire record was added; player state rides the existing 20 Hz `MotionSnapshot` with `body =` the player entity and `acked_input =` the last consumed input sequence. `spall_server::serve` gains `Scene::Walk`, gives each connecting client a capsule, and reads `InputFrame` datagrams (one applied per player per tick, redundant `recent` copies recover a dropped frame). Interactive WASD/mouse-look wiring in the graphical client, moving-body **crush** outcomes beyond "server owns push / no client authority" (contact→damage is T21), and formalising the reserved player-id band in `IdRegistry` are follow-up.

For a larger world, multiple independently rebased physics regions are necessary when players are far apart. Region merge/split and body transfer must be atomic and tested. A single origin following one player is not an acceptable multiplayer large-world solution. This is a G5 gate, not hidden work in the initial sandbox.

## Renderer and visual target

Separate geometry visibility from lighting. The initial renderer rasterizes greedy-meshed surfaces for both terrain and transformed voxel bodies. Merge faces only when material, face direction, and relevant lighting/AO attributes match; verify AO interpolation and triangle diagonal choices. Generate faces using neighbor samples across bricks, including local-volume brick seams. Use material colors and procedural world/local-coordinate detail initially; texture atlases are unnecessary.

Implement in this order:

1. Camera, depth, correct face winding, opaque materials, frustum culling, bounded GPU mesh uploads, reusable buffers, resize/device failure handling. **(T05)** `spall_mesh` owns culled/greedy face generation with an AO-aware merge rule and a `spall_jobs` halo token; `spall_render` owns one opaque WGSL pipeline (CCW front faces, `Depth32Float`, back-cull), a free-fly `Camera` with Gribb–Hartmann frustum culling, a growing reusable vertex/index buffer with a byte budget, and an offscreen `capture_scene` writing shaded plus normal/depth PNGs. Cell size stays 0.25 m; no revision proposed.
2. HDR linear-light rendering, physically based roughness/metalness shading, sun and sky, cascaded shadows, contact AO, tone mapping with fixed test exposure. **(T12)** `spall_render` owns four stable 2048² sun cascades, a `Rgba16Float` opaque pass with GGX direct lighting and mesh-derived contact AO, and an explicit ACES-fitted tone-map pass into a single sRGB target. Captures emit shaded, albedo, normals, linear depth, cascade/shadow visibility, and roughness views. Moving bodies use their world-transformed vertices in both opaque and shadow passes. On Windows the supported default is D3D12; `SPALL_WGPU_BACKEND=vulkan` remains available, but wgpu 24's Vulkan comparison-depth-array path crashed in the tested NVIDIA driver and is not the accepted T12 backend.
3. Early lighting prototype: camera-local occupancy/material clipmap in a fixed-size 3D texture/brick atlas; compute voxel ray traversal for low-resolution diffuse indirect light and soft visibility. Initial near volume: 128 cubed at 0.5 m, covering 64 m. Coarse occupancy can leak or over-occlude thin walls: this is a measured risk, not accepted correctness for collision.
4. Dirty-region updates for terrain edits and both old/new AABBs of moving objects. Clear/rebuild overlapping occupancy correctly; removing one object must not erase another. Emissive sources contribute to lighting. Limit bounce count initially to one diffuse bounce.
5. Temporal reprojection with depth/normal rejection, neighborhood clamping, disocclusion handling, and history invalidation after edits. Denoise and composite; reserve full-resolution raster silhouettes even when lighting is lower resolution.
6. Expand quality/range only after G2: multiple clipmap levels, better diffuse visibility, local lights, reflections, and distant LOD.

The clipmap is a derived GPU lighting cache, not the world format. Do not require experimental hardware ray tracing or a sparse voxel octree for the initial renderer. Optional future hardware acceleration must preserve a supported baseline and be justified by captured evidence.

Check indoor light leakage, emissive bounce, moving debris shadows, rapid edits, and motion ghosting. AO plus bloom alone does not pass G2. The exact indirect-light sampling/denoising implementation is a graphics integration decision in T13, not an invitation for separate agents to invent incompatible render pipelines.

## Streaming and lifetime

Track three distinct ranges: render residency, physics interaction, and structural dependencies. Structural dependency range is not capped to camera visibility. Server active regions are the union of player neighborhoods and active body trajectories, with hysteresis and safety margins. Predictively load collision ahead of fast bodies. Stop/defer entry into unknown collision rather than treating it as empty.

Brick states: absent -> requested -> resident -> dirty -> checkpointed -> evictable. Mesh, collider, and lighting readiness are separate versioned states. Persist edits before eviction. Do not regenerate over modified bricks; tombstones are needed even when the result is entirely air.

Storage partitions index data; they do not own indivisible physical objects. A body spanning partitions has one authoritative identity and geometry owner, plus spatial index references. Relevance uses its bounds, not just its centre.

Begin G1/G2 with all scene geometry resident. G3 introduces eviction against the same invariants. Distant render LOD never changes authoritative voxels, collision, support, or replicated destruction outcomes.

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
(`STORE_SCHEMA_VERSION`; a `PRAGMA user_version` guard rejects a newer database
untouched), zstd-compressed brick payloads with one-brick-bounded decode, a
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
