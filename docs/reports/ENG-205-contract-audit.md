# ENG-205 authoritative animal lifecycle contract audit

Date: 2026-10-14. This is an API/source audit and proposed contract only. No
runtime implementation, game acceptance, lifecycle test, GPU gate, or
population performance result is claimed. The detailed contract is
[`animal-lifecycle.md`](../animal-lifecycle.md).

## Finding

The reviewed foundations provide a place to initialize bodies in a fresh world,
observe committed player cuts, simulate ordinary voxel bodies, and replicate/
persist those bodies. They do not provide a generic animal actor lifecycle.
ENG-205 can freeze shared invariants, but runtime work needs a coordinator
decision on exact articulated collision and source-cell provenance before
interfaces should be added.

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
- `examples/sandbox/src/animal_habitat.rs:1-10, 109-119, 203-246` validates a
  bounded candidate against a current terrain view and, for aquatic animals,
  a live water query. It rejects unknown terrain and unavailable/exhausted
  water queries; this is candidate validation, not movement or a lifecycle.
  `examples/sandbox/src/animal_visual.rs:1-18, 50-120` expands and composes
  render cells from part hierarchy/transforms, retaining source material/tint
  identity under a fixed expansion cap; those are render cells.

## Format and existing animal scope

`docs/spvox-format.md` sections 5.2 and 5.7 define `PART` hierarchy and
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
behavior data behind a statically linked game interface. Actor identity is
stable and never reused; source identity is `(creature, generation, part,
local cell)`. Live cells have one actor-volume owner; death is one atomic,
journalled owner transfer into ordinary editable voxel bodies, with material,
revision, and provenance preserved. Client clip sampling and poses remain
presentation only. Unknown or stale world/water/job data cannot authorize a
move or lifecycle event. IDs, topology, and actor state cross baseline,
interest, reset, and save/recovery through explicit bounded versioned records.

The stated first proof is one trout, crow, and deer in generated habitat;
two-client late join/reconnect; save and abrupt restart; water drain and ground
removal; then damage, death, and further body destruction with per-cell
conservation. GPU and population-scale gates are independent and unrun.

## Integration decisions still needed

1. Select and measure the exact Rapier representation for deforming articulated
   voxel collision and part/cell hit provenance. Current adapter has one
   occupancy-derived body representation and no game-authored physical joints.
   This needs a feasibility result and coordinator acceptance before runtime
   code. No envelope or frozen-rest approximation is accepted as exact.
2. Review the `CreatureId` allocator, world-epoch reset semantics, source
   provenance storage, and the proposed schema/bounds before changing shared
   protocol/store interfaces. In particular, reset is not currently available
   for persisted worlds, and schema migration policy must be explicit.
3. Freeze how species assets/policies are catalogued, authenticated by content
   hash, and made available to server and clients. Existing SPVX material
   mapping is project/game-owned; no biological profile is established by the
   fixtures.

## Dependency-ordered next slices

1. Coordinator reviews and freezes the host API, actor IDs/epoch, DTO caps,
   source provenance format, and collision feasibility workload.
2. Implement engine host/identity and tick invalidation seam; sandbox supplies
   species policies. Ownership: `spall_core`, `spall_protocol`, `spall_sim`,
   `spall_server`, sandbox. No runtime birth until the transaction path is
   included.
3. Implement lifecycle/topology transactions and source-cell provenance,
   including atomic alive-to-corpse transfer, replay, and baseline install.
4. Implement the selected Rapier actor representation, exact query mapping,
   versioned jobs, and bounded trout/crow/deer locomotion against post-edit
   terrain/current water.
5. Implement durable checkpoint/journal state, interest/late-join/reconnect/
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
