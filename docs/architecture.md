# Architecture and invariants

Status: proposed implementation contract. Numerical defaults are frozen after G1/G2. Multiplayer and full-world destruction are mandatory throughout.

## Workspace and dependency direction

Arrows below mean "depends on". Create crates when their first task needs them; avoid empty scaffold packages.

```text
spall_core       IDs, coordinates, ticks, units, material definitions
spall_voxel   -> spall_core                     storage, sampling, edits, queries
spall_jobs    -> spall_core                     bounded scheduling, result tokens
spall_mesh    -> spall_voxel                    surface generation, no GPU
spall_structure -> spall_voxel, spall_jobs      connectivity, support, split plans
spall_physics -> spall_voxel                    Rapier adapter, collision builds
spall_sim     -> spall_structure, spall_physics    authoritative state and tick order
spall_protocol -> spall_core                   explicit DTOs and codecs only
spall_net     -> spall_protocol                 Quinn transport adapter (Tokio; T09)
spall_store   -> spall_protocol                 checkpoint/journal bytes and indexes
spall_render  -> spall_mesh, spall_core            wgpu resources and render passes
spall_server  -> spall_sim, spall_net, spall_store, spall_jobs
spall_client  -> spall_net, spall_voxel, spall_physics, spall_render, spall_jobs
sandbox (example) -> spall_server, spall_client   game rules and executable entry points
xtask                                    process/scenario/build orchestration
```

spall_sim owns conversion between authoritative state and protocol records; persistence does not own simulation objects. The client maintains a replica and prediction state; it never runs server-only structural decisions. Render input is an extracted immutable view of the replica, never a reference into a running server.

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

### Large collapses and load policy

Do not solve overload by turning every voxel into a body. Use connected compound bodies and bounded fracture granularity. A huge connected object can remain one multi-brick body if the collider representation supports it. If it cannot, T06 must establish a documented deterministic coarse fracture policy before the scale gate.

Pending work is allowed within a declared latency budget. Once exceeded, throttle new expensive intents with explicit retry responses. Never keep a completed unsupported component permanently static because a body cap was reached. Cosmetic dust can be capped/dropped freely; solid gameplay matter cannot disappear. Settled distant bodies may be persisted and deactivated only when their whole interaction region is dormant, and must reactivate before contact or edits.

## Collision and character physics

T06 compares native voxel colliders with deterministic merged-cuboid compounds for static and dynamic volumes. Verify concave contacts, edit/rebuild cost, and mass handling on the pinned Rapier version. Avoid dynamic concave triangle-mesh colliders; the Rapier documentation cautions against them. Terrain contains tunnels/overhangs, so heightfields are insufficient. [Rapier collider guidance](https://rapier.rs/docs/user_guides/rust/colliders/).

Exact occupied-space compounds are the correctness baseline. A single convex hull over a hollow building would block its rooms and is unacceptable. Approximate far collision is permitted only where it cannot affect gameplay and before promotion to a fully simulated area. Keep collision materials separate from visual palettes where the adapter requires it.

Use a capsule character controller with grounded state, gravity, jump, slope/step constraints, swept movement, and server-authoritative interaction with dynamic bodies. Physics queries need up-to-date collider revisions. Enable CCD selectively for fast damaging bodies; test tunneling and contact-energy thresholds before enabling impact-triggered destruction.

Client player movement is predicted and reconciled. Remote bodies initially use snapshot interpolation plus kinematic collision proxies. They never gain authority from a client's local collision. Later local dynamic extrapolation is optional only if correction tests show a benefit.

For a larger world, multiple independently rebased physics regions are necessary when players are far apart. Region merge/split and body transfer must be atomic and tested. A single origin following one player is not an acceptable multiplayer large-world solution. This is a G5 gate, not hidden work in the initial sandbox.

## Renderer and visual target

Separate geometry visibility from lighting. The initial renderer rasterizes greedy-meshed surfaces for both terrain and transformed voxel bodies. Merge faces only when material, face direction, and relevant lighting/AO attributes match; verify AO interpolation and triangle diagonal choices. Generate faces using neighbor samples across bricks, including local-volume brick seams. Use material colors and procedural world/local-coordinate detail initially; texture atlases are unnecessary.

Implement in this order:

1. Camera, depth, correct face winding, opaque materials, frustum culling, bounded GPU mesh uploads, reusable buffers, resize/device failure handling.
2. HDR linear-light rendering, physically based roughness/metalness shading, sun and sky, cascaded shadows, contact AO, tone mapping with fixed test exposure.
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
