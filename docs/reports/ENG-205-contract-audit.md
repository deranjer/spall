# ENG-205 authoritative animal lifecycle contract audit

Date: 2026-10-15. This is an API/source audit and proposed contract only. Source
line references are anchored to inspected baseline commit `71540f8`
(`Render articulated animal fixtures and validate their habitats`), not moving
main. No
runtime implementation, game acceptance, lifecycle test, GPU gate, or
population performance result is claimed. The detailed contract is
[`animal-lifecycle.md`](../animal-lifecycle.md).

## Finding

The reviewed foundations provide a place to initialize bodies in a fresh world,
observe committed player cuts, simulate ordinary voxel bodies, and replicate/
persist those bodies. They do not provide a generic animal actor lifecycle.
ENG-205 records the shared invariants and prerequisite map, not a frozen runtime
interface. Runtime work needs measured collision and corpse representation
gates plus review of source-cell provenance before shared interfaces should be
added.

## Source evidence: current behavior

- `crates/spall_server/src/serve.rs:1136-1147` defines
  `InitialGameWorldSetup` as `FnOnce(&mut Simulation)` and documents that it
  runs on the authoritative thread before the initial checkpoint, not when an
  existing save is restored. `serve.rs:5671-5760` invokes it only for
  non-persisted startup and a database with no checkpoint; the recovery arm
  calls `persist::restore_with_terrain_collider_mode` and skips it.
- `crates/spall_server/src/serve.rs:1142-1162` defines committed edit hooks.
  `CommittedEditHandler` receives a committed player's cut and removed
  material counts; the outbox encoder gives a journal sequence for durable
  game events. This is not a create/update/death callback or actor scheduler.
  `serve.rs:3579-3610` fans out already-committed edit outcomes.
- `crates/spall_sim/src/world.rs:1503-1609` implements `SimWorld::spawn_body`
  and `spawn_body_with_material_densities`: allocate entity/volume IDs, build
  a volume, construct an occupancy grid/collider and mass properties, add a
  Rapier body, then insert owner/body maps. It emits no `TopologyTransaction`
  and does not provide a journal/replication creation event. The server uses
  startup setup before baseline publication; this path is not safe as a
  runtime birth/death path. `retire_empty_volume` at `world.rs:1630-1650`
  removes an empty detached body and its adapter body.
- `crates/spall_sim/src/sim.rs:722-895` establishes the current main tick
  seam. It runs the staged edit pipeline first, derives committed terrain
  brick changes for water, steps/publishes column water, advances vegetation
  and weather, applies buoyancy from the published frame, steps physics, and
  advances players. Later `sim.rs:1012+` applies contact-damage policy outside
  the solver and admits cuts through the ordinary edit path. There is no
  animal system in this schedule (`crates/spall_sim/src` has no actor/species
  hook); verify this when implementing rather than relying on a proposed hook.
- `crates/spall_sim/src/registry.rs:10-82` owns monotonic per-world
  `EntityId`, `VolumeId`, transaction and journal counters and resumes them
  from checkpoint counters. `crates/spall_core/src/ids.rs:17-60` supplies
  `WorldId` as a 128-bit world identity; it is not a per-creature identity.
  There is currently no creature allocator/counter.
- `crates/spall_sim/src/body.rs:1-17, 83-132` defines the current body as one
  voxel `Volume`, pose/velocities, body kind and one opaque physics handle.
  `crates/spall_physics/src/collider.rs:1-110` builds a voxel shape or a
  merged-cuboid compound from an occupancy grid. `spall_physics/src/world.rs`
  provides `PhysicsWorld::add_body`, character shape sweeps, and
  `PhysicsWorld::contact_impulses` (symbols at lines 519, 924-1065 and 1412).
  It has no actor-to-SPVX-part/cell provenance
  contract. This is the exact articulated collision feasibility gap.
- The live server's optional reset path explicitly refuses persistence,
  residency eviction, startup game content, and contact damage/dormancy
  combinations (`crates/spall_server/src/serve.rs:2660-2662, 3161-3170`). Its
  reset replaces the world and publishes a replace-world baseline
  (`serve.rs:5064-5168`); it is not actor-ID or spawn-director policy.
- `crates/spall_protocol/src/records.rs:447-554` has topology transactions for
  edits and server-decided splits, including inline/compressed/bulk split
  geometry. `records.rs:644-676` defines motion datagrams. These records do
  not identify a creature, species, asset, clip, source part, or lifecycle.
- `crates/spall_protocol/src/baseline.rs:15-25, 33-86` transfers exact
  authoritative terrain/body volumes and revisions; motion is sent separately
  after catch-up. `crates/spall_server/src/replication.rs:1-19, 27-66` keeps
  committed topology reliable and schedules interest-filtered body motion.
  There is no actor manifest or interest dependency yet.
- `crates/spall_server/src/persist.rs:1-18, 30-55, 217-245` captures bodies,
  topology, counters and periodic pose batches into the sole durable writer's
  checkpoint/journal path; `STORE_SCHEMA_VERSION` is independently checked.
  There is no creature state/provenance section or creature lifecycle journal
  payload. Existing game progression outbox data cannot stand in for it.
- `examples/sandbox/src/animal_habitat.rs:1-10, 34-50, 91-119, 203-246`
  validates a bounded candidate against a current terrain view and, for
  aquatic animals, a live water query. It returns candidate rejections for
  unknown terrain, unavailable water, or shallow/dry columns. It does not
  expose a persistent `KnownWet` / `KnownDry` / `Unknown` state nor decide
  movement, stranded behavior, or lifecycle.
  `examples/sandbox/src/animal_visual.rs:1-18, 50-120` expands and composes
  render cells from part hierarchy/transforms, retaining source material/tint
  identity under a fixed expansion cap; those are render cells.

## Format and existing animal scope

`docs/spvox-format.md` sections 5.4 and 5.7 define `PART` hierarchy and
`ANIM` transform clips at 60 ticks/second. The format explicitly leaves
server-authoritative collision changes and per-frame voxel topology deltas out
of v1. Hierarchy/animation does not define joints, collision, or damage.
`docs/animals.md:29-46, 94-105` and
`docs/reports/ENG-204-animal-preview.md` likewise scope habitat/visual work and
record that spawning, steering, collision, damage, replication, and recovery
are absent. ENG-204 fixture source materials are visual/game catalog inputs;
they are not validated biology or mass values.

`docs/protocol.md` already requires stable server-assigned identity, ordered
topology, revision/hash checks, a reliable baseline barrier, motion datagrams,
and save schema separation. The proposal in `animal-lifecycle.md` adds a
separate creature-state schema and allocator while retaining these contracts;
it does not repurpose `MotionSnapshot`, `BaselineWorld`, or SPVX schema for
mutable actor lifecycle.

## Decisions frozen in the proposal

The server owns actor creation, location, damage, death, material mapping,
source-cell ownership, collision, and consequences. Sandbox owns species and
behavior data behind a statically linked game interface. Creature IDs are
never reused in a persistent world; v1 generation remains 1 through damage and
corpse transfer. `world_epoch` separately namespaces reset and never resets
the allocator. One assembly owner binds each bounded part ID to a distinct,
independently revisioned source volume; multiple volumes do not imply multiple
physics bodies. Cell identity includes the assembly key, source part, and
part-local coordinate. Death transfers the bindings atomically to a retained
corpse assembly without revoxelizing. Its editable physical aggregate is
unresolved and separately gated. Per-part sampled pose and effective render
visibility are bounded state; death freezes clip time and preserves the exact
part poses. Hidden art cells remain physically/topologically present.

The selected **next feasibility candidate** is ENG-209: one Rapier kinematic
root with one exact existing voxel/cuboid collider per source part, pose-updated
locally, with query-to-source-cell provenance measured. ENG-209 is geometry and
query only, has no lifecycle/runtime-interface dependency, and its assignment
does not accept the candidate for shipping. Because the root is kinematic, deer
gravity/velocity must be game-integrated and displacement checked through
Rapier sweeps; dynamic solver contacts/pushing remain for the feasibility and
integration decisions. A separate corpse gate must decide how the current
one-volume-per-body API can own/edit an articulated multi-volume corpse without
exploding it into per-part bodies or inventing joints.

V1 admission limits are 256 assembly records per world (living plus retained
corpse), 256 parts per assembly, 65,536 part bindings per world, 250,000 cells
per assembly, 32 KiB per encoded assembly record, 8 MiB actor-state bytes/256
records per baseline, and 64 KiB per reliable lifecycle event. Pose fields are
explicit finite `[f64;3]` root-relative metres plus normalized `[f32;4]`
quaternion, unit scale implicit; acceptance normalizes once and replay/death
preserve accepted bits. These bound memory and wire admission; they are not
performance acceptance. At capacity, births are rejected; corpses are never
deleted to make room. Capacity frees only after every source cell is removed
and removal is durably ordered. Identical replay at the same event key is
idempotent; divergent duplicate bytes are an error. Unknown water freezes
movement; known dry/drained water permits only a species-owned stranded/
reselection response, never automatic death. The current habitat candidate API
does not yet supply that tri-state lifecycle result.

Animation runs at SPVX's 60 ticks/second. The inspected simulation uses
`TICK_DT_S = 1/60` and the server handshake advertises `server_tick_hz=60`
(`spall_sim/src/sim.rs:56`, `spall_server/src/serve.rs:1436-1437`); 20 Hz is
motion snapshot cadence only. The DTO carries unfolded `clip_phase_tick`,
subtick remainder and `clip_epoch`; once/loop/ping-pong sampling folds phase at
sample time so ping-pong direction is retained. V1 pins simulation to 60 Hz;
another tick rate requires version review. Creature reset remains refused
until atomic checkpoint/journal replacement, epoch invalidation, and
replace-world baseline barriers are designed and implemented. The allocator
continues monotonically across epochs.

The target proof is one trout, crow, and deer in generated habitat; two-client
late join/reconnect; save and abrupt restart; water drain and ground removal;
then damage, death, and further corpse destruction with per-cell conservation.
Death/continued destruction is blocked on the separate corpse aggregate gate.
GPU and population-scale gates are independent and unrun.

## Integration decisions still needed

1. ENG-209 measures the one-kinematic-root, exact per-part collider candidate
   and query provenance only. It does not accept a runtime interface.
2. Separately decide the corpse aggregate representation and prove atomic
   ownership/editing of distinct part volumes while preserving exact float
   pose and provenance; current ordinary body storage is one volume per body.
3. Review persisted part provenance, `CreatureId` allocation, generation 1,
   world-epoch reset semantics, and baseline/journal schema migration before
   shared protocol/store changes. Current reset is unavailable for persisted
   worlds.
4. Freeze how server/client builds obtain assets and species policy by exact
   SPVX UUID/hash. Resolve portable SPVX material names through the explicit
   game `MaterialManifest`; fixture wood/stone visuals do not define density.
5. Add lifecycle-level wet/dry/unknown water state. Existing habitat validation
   is only a bounded candidate accept/reject query.
6. Decide whether kinematic sweep contacts satisfy deer support and actor
   pushing; kinematic roots do not receive Rapier dynamic gravity.

## Dependency-ordered next slices

1. ENG-209 geometry/query feasibility, with no lifecycle/runtime interface
   dependency.
2. Corpse aggregate feasibility and ownership/provenance review.
3. Coordinator freezes host API, actor IDs/epoch, bounded DTO/corpse retention
   rules, and the measured representation outcomes.
4. Implement engine host/identity and tick invalidation seam; sandbox supplies
   species policies. Ownership: `spall_core`, `spall_protocol`, `spall_sim`,
   `spall_server`, sandbox. No runtime birth until the transaction path is
   included.
5. Implement lifecycle/topology transactions and source-cell provenance,
   including atomic alive-to-corpse transfer, replay, and baseline install.
6. Implement the selected Rapier actor representation, exact query mapping,
   versioned jobs, and bounded trout/crow/deer locomotion against post-edit
   terrain/current water.
7. Implement durable checkpoint/journal state, interest/late-join/reconnect/
   reset assembly, then CPU and two-client save/restart/damage/destruction
   scenarios. GPU capture and population performance remain separate gates.

## Audit validation

Reads/searches covered the required guide/source documents, ticket detail,
ENG-204 report, `spall_server::{serve,persist,replication}`,
`spall_sim::{sim,world,body,registry}`, `spall_physics::{collider,world}`,
`spall_protocol::{records,baseline}`, core IDs, and sandbox habitat/visual
modules. Code search found no generic creature lifecycle hook. No tests were
run: this change only records a contract proposal and audit. `git diff --check`
passed with no whitespace errors (Git reported the configured LF-to-CRLF
working-copy conversion warning). No behavior or performance result is measured
here.
