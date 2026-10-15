# Authoritative animal lifecycle contract (ENG-205 proposal)

**Status:** contract proposal for coordinator integration review. This freezes
the server/game boundary and the invariants for a later runtime slice. It does
not claim that live animals, animal replication, or animal recovery exist.

## Boundary and ownership

`sandbox` owns species definitions, behavior parameters, habitat policy,
material mapping, and animal presentation. Engine crates own reusable actor
identity/state DTOs, lifecycle scheduling points, validation, ordinary voxel
volume ownership, Rapier adaptation, transaction ordering, transport, and
storage. No engine crate imports `sandbox`. The host accepts a statically linked
game behavior implementation/configuration, following the existing
`InitialGameWorldSetup` direction; it must not grow a general plugin or AI
framework.

One persistent creature has a stable `CreatureId` (`u64`, nonzero, never reused
within a `WorldId`) and a monotonically increasing `generation` (`u32`) for
reset/replacement epochs. The pair identifies one incarnation. A `CreatureId`
is distinct from runtime `EntityId`, source `VolumeId`, `WorldId`, player IDs,
SPVX UUIDs, and asset filenames. Creation assigns it on the authoritative
simulation thread from a persisted creature allocator. A candidate key is
`(WorldId, CreatureId, generation)`; world reset also advances a persisted
`world_epoch` before new candidates may be admitted. Runtime ECS, Rapier, and
asset handles never enter these records.

The game supplies a bounded `SpeciesKey` (ASCII portable key, 1–63 bytes),
`AssetId` (SPVX UUID plus logical content hash), `clip_id` (asset clip-table
index or an explicit game clip key), and `material_mapping_version` (game
catalog version/hash). Runtime resolution checks all of these against the
current asset and material manifest. SPVX part IDs and exact source cells are
the authored source identity; a cell identity is
`(CreatureId, generation, source_part_id, source_cell_xyz)` with integer local
cell coordinates. Expansion into render geometry is derived data. Colors/tints
are visual only. Source fixture materials such as stone or wood do not imply
biological density, hardness, collision, or damage rules.

Species policy is game-owned and versioned. Its bounded definition includes
spawn eligibility, source-to-world material mapping, mass density by material,
damage thresholds/rules, body envelope and movement bounds, habitat query
budget, support/water tolerances, locomotion constants, animation mapping,
and an explicit collision profile. Every numeric field has finite/range
validation; keys and arrays have fixed caps. Unknown species, asset hash,
clip, material mapping, or policy version rejects creation or recovery before
publication. No silent fallback to a default species or material.

## Cell ownership, damage, and death

An alive creature owns exactly one authoritative voxel volume. Its exact source
cell records, source-part identity, mapped material IDs, per-cell damage state,
brick revisions, and actor generation are authoritative server data. Animated
SPVX transforms do not change voxel topology or ownership. `PART` hierarchy is
not a structural joint, collision rig, bond graph, or damage rule. `ANIM` is
not client authority. Source-part identity must be retained as a structural
metadata layer through brick partitioning, damage cuts, splits, baselines,
checkpoints, and journal replay.

Every hit is resolved by the server against a versioned actor/source-volume
revision. A hit proposal names `CreatureId`, generation, source `VolumeId`,
source cell or bounded impact ray, expected brick revisions, and input/action
identity. Server-side geometry resolves the actual part/cell and material;
claimed part/material/damage is never trusted. A stale generation or revision
rejects without side effects. Damage changes remain ordered topology/lifecycle
transactions and use the same server tool/structural rules as other editable
voxel volumes.

Death is a transaction, not a visual flag. A transaction either (a) changes
alive cells and leaves one living owner, or (b) changes state to `Corpse` and
atomically transfers every surviving source cell to one or more ordinary
editable voxel body volumes, preserving material, revision, exact cell
position, and source provenance. Within the transaction, each cell has one
owner before and one owner after. Source cells are removed as child volumes
are created; there is no frame in which a cell is duplicated or lost. Empty
animal volumes retire through the ordinary empty-volume path. Corpse bodies
then use existing tool edits, structural split transactions, motion, baseline,
checkpoint, and journal paths. A corpse transition emits a reliable lifecycle
record and ordered topology transaction(s); recipients hold subsequent state
until all dependencies and bulk geometry arrive.

## Collision and feasibility gate

The only physics adapter remains `spall_physics` / Rapier. No voxel is an ECS
entity or rigid body. A creature's collision and hit representation must
correspond to its current authoritative source-cell geometry and articulated
pose, with stable mapping from a contact/query result to source part/cell.
Collision rebuild work may use immutable bounded snapshots and prepared
adapter objects; only the simulation owner applies a result at a tick boundary
after checking creature generation, topology revision, clip/pose epoch,
and collider revision. A stale result is discarded.

**Coordinator decision required before runtime implementation:** the current
adapter builds colliders from one occupancy grid per body, and SPVX v1 does not
specify a physical joint graph. An exact articulated voxel animal may require
an explicit game-owned part/joint rig and a Rapier adapter representation
whose query/contact results preserve source-cell provenance. The adapter does
not currently expose that actor contract. Select and measure that representation
before claiming animal collision or damage. A single body envelope, frozen
rest-pose hit map, deforming render-only animation, hidden anchor, or
per-voxel rigid-body workaround cannot be reported as exact articulated
collision. If feasibility fails, stop at this gate and report the measured
candidate and workload; do not substitute an approximate topology under the
same acceptance name.

After that gate is passed, species-defined physical parts may share one actor
identity and one transaction/lifecycle owner while Rapier owns the explicitly
chosen aggregate. Engine actor identity is not tied to collider count. Static
source-part/cell identity survives any internal collider decomposition.

## Tick and worker contract

The current `Simulation::tick` order is: increment tick; run staged edits and
publish validated commits; apply committed terrain changes to water boundaries
and step/publish current column water; advance vegetation and weather; apply
buoyancy from the current water frame; step Rapier; advance players. Animal
integration must make its ordering explicit with the host:

1. Drain bounded lifecycle, movement, and damage intents admitted for this
   server tick; validate stable identity, policy, expected actor/terrain/water
   epochs, and budgets.
2. Apply completed immutable worker results only when every token matches the
   current world epoch, creature generation, source topology revision, terrain
   revision stamp, water frame revision, and requested movement/pose epoch.
   Otherwise discard and resubmit within a bounded budget.
3. Commit terrain edits already selected by the canonical edit pipeline, then
   update/step authoritative column water against the post-commit terrain.
4. Validate each creature's movement against the post-commit terrain snapshot
   and just-published water frame. Advance a bounded species-owned locomotion
   state, apply actor motion/collision changes on the owner thread, then step
   Rapier once. Contacts can enqueue damage for a later transaction; contacts
   never mutate topology inside the solver step.
5. Emit lifecycle/topology outcomes in transaction order, then poses/clip-time
   presentation snapshots from the resulting authoritative tick.

This ordering needs a reviewed insertion point in `Simulation::tick`; callbacks
must not re-enter or mutate `SimWorld` while the pipeline, fluid worker, or
Rapier step is active. Worker snapshots own their data and never borrow live
world state. If terrain or water advances during a worker job, its captured
epoch/revisions no longer match and its result cannot apply.

`Unknown` terrain, missing body/source volume, exhausted query budget, stale
water frame, or unavailable water is not empty space or dry ground. It freezes
the affected move, retains the last valid stable state, and requests a bounded
retry or interest/residency load. It cannot cause a spawn, teleport, damage,
death, or destructive topology edit. Water drain invalidates a trout's current
habitat after the new water frame; the fish stops and performs a bounded
reselection, then transitions only through a server-authored move/death rule.
Ground removal invalidates deer support after the commit; the deer must use
Rapier gravity/contact and a bounded grounded relocation policy, never a hidden
support anchor. Crow movement is bounded waypoint/steering flight using the
same current occupancy queries and adapter. These are the first three simple
behaviors; no general pathfinder or planner is implied.

Habitat candidate validation can reuse sandbox `animal_habitat`'s live,
bounded occupancy and water query, but that validator alone is not movement,
articulated collision, or navigation. A spawn candidate is checked against
current state on the server and reserves its stable identity atomically. A
seeded spawn director, if used, is game-owned input to deterministic server
proposals; it must not regenerate a creature on baseline, reconnect, unload,
or reset. Actor population identity/state is checkpointed.

## Lifecycle and replication records (proposed, not implemented)

Define an independent `CREATURE_STATE_SCHEMA = 1`, separate from wire,
baseline, SPVX, and save schema versions. All records use bounded postcard DTOs
with explicit integer widths, units, tagged enum variants, and validation caps;
wire envelopes continue to use existing record framing/limits. Do not serialize
Rust enum layout, native-endian structs, ECS or Rapier handles. Proposed
canonical `CreatureRecord` fields are:

```text
schema:u16, world_epoch:u64, creature_id:u64, generation:u32,
state_tag:u8, species_key:bounded_utf8, species_version:u32,
asset_uuid:[u8;16], asset_hash:[u8;32], clip_key:bounded_utf8,
material_mapping_hash:[u8;32], source_volume_id:u64,
source_topology_revision:u64, source_part_count:u16,
source_cell_count:u32, root_pose:{translation_m:[f64;3], rotation:[f32;4]},
linear_velocity_m_s:[f32;3], angular_velocity_rad_s:[f32;3],
animation_tick:u32, locomotion_tag:u8, state_revision:u64
```

This record stores actor metadata and root motion. Source geometry/materials
remain in the ordinary `BaselineVolume`/brick payload; do not duplicate whole
SPVX assets into every state record or treat authored files as mutable saves.
Bounds must be frozen with the first implementing ticket: max 256 live actors
per world, 1,024 UTF-8 bytes per variable record, max 1,024 source parts and
250,000 source cells per actor (matching current visual bridge hard limit),
max 16,384 actor records per baseline, max 1 MiB decoded actor-state section
per baseline part, and max 64 KiB for one reliable lifecycle record. Larger
state uses a bounded bulk baseline transfer. Product/population capacity remains
unaccepted; these are admission safety limits pending measurement.

Reliable ordered records are required for `CreatureCreated`,
`CreatureStateChanged`, `CreatureDied`, `CreatureRemoved`, source-volume
ownership/topology changes, and actor baseline manifest parts. A lifecycle
record carries `world_epoch`, creature ID/generation, monotonically increasing
state revision, server tick, transaction/dependency sequence, and the relevant
source volume/revision. Repeating an already-applied revision is idempotent;
gaps hold later records and request repair/baseline. Motion datagrams may carry
only an existing actor reference plus `server_tick`, `state_revision`, root
pose/velocity, `clip_key`, and animation tick. Clip time and pose are
presentation; they never authorize topology, collision, damage, or lifecycle.
Clients interpolate received poses and sample the validated asset. Missing
assets can use a bounded placeholder presentation while lifecycle/source
topology stays authoritative; this does not replace server collision.

Late join: baseline begins with world epoch/tick and a sorted creature manifest
with IDs/generations/source-volume references and asset hashes. The ordinary
world baseline contains exact source volumes/materials/revisions. The client
installs the complete transfer atomically, acknowledges the manifest hash and
cursor, then receives queued reliable lifecycle/topology events before motion
keyframes. An actor cannot become visible as alive before its source geometry
and asset identity validate. Interest entry uses the same manifest+volume
baseline dependency; interest exit only drops local presentation/cache and
never destroys server state. Reconnect follows late join and does not create
new actors. A reset increments epoch, invalidates pending work and queued
requests, sends a replace-world baseline, and does not reuse IDs or replay old
epoch actors. Reset is unavailable with a persisted world in the current host;
creature reset/reseed semantics must be resolved in the integration design.

## Durable recovery

Creature state and topology must share the existing ordered journal sequence
and checkpoint cut. Add a versioned creature checkpoint section/table and
versioned journal payloads for creation, state changes, death/removal and source
ownership transfers. The durable writer remains the sole writer; each record is
idempotent by world epoch, stable creature ID, generation, state revision and
journal sequence. Recovery verifies world ID, manifest/assets/policy versions,
schema, actor-to-volume ownership uniqueness, and topology hashes before
publishing the rebuilt Simulation. The allocator persists `next_creature_id`
and `world_epoch` with existing next entity/volume/transaction/journal
counters. Replay must reject duplicates, ownerless source cells, double-owned
cells, missing volume references, and events after the durable prefix.

The existing progression/outbox database is for progression effects; it is not
an actor store. Existing `InitialGameWorldSetup` is only invoked for new worlds,
before the first checkpoint, and skipped during restore; it is unsuitable for
runtime birth or restore. If the process crashes during a death transfer, the
checkpoint plus durable journal must reconstruct either the pre-transaction
alive owner or the post-transaction corpse owners, never an intermediate state.

## First integrated proof and acceptance

The first proof is one trout, one crow, and one deer in generated habitat, with
exact source identity and material mapping. Two clients must observe the same
IDs/state/topology after late join and reconnect. Save, abrupt restart, and
recovery preserve identity without a second spawn. Drain the occupied water
column and remove the deer support terrain; current-state invalidation changes
server behavior. Damage the actor, kill it, and continue editing/destroying the
corpse as ordinary voxel bodies while proving per-cell conservation and exact
source provenance across damage, death, splits, late join, and restart. Reset
must invalidate old actors and prevent ID reuse. Unknown/unloaded terrain and
exhausted budgets must not authorize movement or spawn. These CPU and two-client
save/restart/damage/destruction scenarios are required before live animal
support is claimed.

GPU capture and population-scale/frame-time/bandwidth/per-tick query budgets
are separate gates and remain **unrun** by this contract. Preview captures and
habitat candidate tests are not lifecycle, collision, or population acceptance.

## Implementation slices after coordinator review

1. **Host contract and IDs:** engine-owned bounded actor DTOs, persisted actor
   ID/world epoch counters, a host-side statically linked game behavior hook,
   and invalidation tokens. Likely files: `spall_core` IDs, `spall_protocol`
   DTO definitions, `spall_sim` host state/tick seam, `spall_server` setup and
   restore. Prerequisite: coordinator accepts collision policy and bounds.
2. **Atomic lifecycle/topology:** creation/removal and alive-to-corpse source
   ownership transactions, provenance metadata on voxel cells, replication
   application and canonical hashes. Likely files: `spall_voxel`, `spall_sim`
   commit/replay, `spall_protocol` topology records, `spall_server` fanout.
   Prerequisite: slice 1 plus reviewed provenance storage format/version.
3. **Collision and movement gate:** Rapier aggregate/query adapter with exact
   source provenance, versioned collision builds, explicit species rig and
   trout/crow/deer bounded locomotion using current habitat queries.
   Likely files: `spall_physics`, `spall_sim`, sandbox species module.
   Prerequisite: the measured representation decision above.
4. **Durable/streamed state:** checkpoint/replay schema, journal events,
   baseline/interest/reconnect/reset dependencies, two-client scenarios.
   Likely files: `spall_store`, `spall_server::persist`, protocol baseline,
   server/client assembly, acceptance fixtures. Prerequisite: slices 1–3 and
   explicit schema migrations; no in-place reinterpretation of old saves.
5. **Integrated proof:** generated habitat fixture and fault/restart/damage/
   destruction scenario with measured CPU and two-client results. GPU and
   population performance work stay separate and cannot be inferred from it.

The first ready slice is contract review plus a small collision feasibility
ticket if the coordinator confirms the representation. Runtime actors must not
start before that shared decision and provenance format are frozen.
