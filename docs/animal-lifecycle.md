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
within the persistent `WorldId`). `generation` (`u32`) identifies an actor
incarnation and remains constant through alive, damage, and corpse ownership
transitions. V1 assigns generation 1 and never recycles the creature ID; any
future same-ID reincarnation requires an explicit format decision. A
`world_epoch` is a separate reset namespace incremented on replace-world reset
and never substitutes for, or resets, the creature allocator. The complete
network key is `(WorldId, world_epoch, CreatureId, generation)`. Runtime ECS,
Rapier, and asset handles never enter persistent records.

The game supplies a bounded `SpeciesKey` (ASCII portable key, 1–63 bytes), an
`AssetIdentity` consisting of the SPVX UUID and logical content hash, a
`clip_name` (1–255 UTF-8 bytes resolved against that exact asset identity),
and the explicit `MaterialManifest` hash plus a game mapping version/hash.
There is no overloaded numeric `AssetId` and no clip-index-or-name ambiguity.
Runtime resolution checks every reference against the validated asset and
material manifest. SPVX part IDs and exact source cells are the authored source
identity; a cell identity is `(WorldId, world_epoch, CreatureId, generation,
source_part_id, source_cell_xyz)` with integer part-local coordinates.
Expansion into render geometry is derived data. Colors/tints are visual only.
Source fixture materials such as stone or wood do not imply biological
density, hardness, collision, or damage rules.

Species policy is game-owned and versioned. Its bounded definition includes
spawn eligibility, explicit SPVX portable-material-key to validated
`MaterialManifest` material-ID mapping, damage thresholds/rules, movement
bounds, habitat query budget, support/water tolerances, locomotion constants,
animation mapping, and an explicit collision profile. Physics density,
hardness, and other material properties come from that validated manifest;
the animal contract does not introduce a competing density catalog. Every
numeric field has finite/range validation; keys and arrays have fixed caps.
Unknown species, asset hash, clip, material mapping, or policy version rejects
creation or recovery before publication. No silent fallback to a default
species or material.

## Cell ownership, damage, and death

An alive creature is one authoritative assembly owner with a bounded,
canonical `part_id -> source VolumeId` binding table. Every bound part has a
distinct source volume, its own brick revisions, and an explicit part-local
cell coordinate space; one aggregate topology revision advances whenever any
part topology changes. Multiple source volumes do not imply multiple physics
bodies. Each cell is owned exactly once by its source volume and identified by
the assembly key plus part ID and part-local coordinate. `PART` hierarchy is
not a structural joint, collision rig, bond graph, or damage rule. `ANIM` is
not client authority. Animated SPVX transforms do not rewrite source topology
or the mapping. Source-part identity and its binding must survive damage,
baselines, checkpoints, and journal replay.

Every hit is resolved by the server against a versioned assembly and source
volume revision. A hit proposal names the creature key, source part and volume,
source cell or bounded impact ray, expected brick revisions, aggregate topology
revision, and input/action identity. Server-side geometry resolves the actual
part/cell and material; claimed part/material/damage is never trusted. A stale
generation or revision rejects without side effects. Damage changes remain
ordered topology/lifecycle transactions and use the same server tool/structural
rules as other editable voxel volumes.

Death is a transaction, not a visual flag. It changes the assembly owner from
`AliveCreature` to `CorpseAssembly` and transfers every surviving per-part
volume binding atomically. It preserves exact part-local cell coordinates,
materials, brick revisions, source-part IDs, and the authoritative part/root
poses; it does not revoxelize or copy cells. Each cell has one assembly owner
before and after the transaction. The CreatureId and generation remain attached
to provenance even after the living actor record retires. Empty source volumes
retire only through the ordered topology transaction.

**Separate corpse feasibility gate:** the current ordinary dynamic-body model
owns one `Volume` per body and does not represent a multi-volume articulated
corpse aggregate. Do not convert each part into an independent body, infer
joints, or claim that existing `spawn_body` handles corpse transfer. A
reviewed corpse aggregate/ownership representation must define how those same
source volumes remain editable and physically represented. Until it passes,
the death transaction may be specified and tested at the ownership/serialization
layer, but live death-to-physics and corpse destruction acceptance remain
blocked. Any later transfer into ordinary body records must be atomic and
preserve source identity and pose without revoxelization.

## Collision and feasibility gate

The only physics adapter remains `spall_physics` / Rapier. No voxel is an ECS
entity or rigid body. A creature's collision and hit representation must
correspond to its current authoritative source-cell geometry and articulated
pose, with stable mapping from a contact/query result to source part/cell.
Collision rebuild work may use immutable bounded snapshots and prepared
adapter objects; only the simulation owner applies a result at a tick boundary
after checking creature generation, topology revision, clip/pose epoch,
and collider revision. A stale result is discarded.

**Measured feasibility candidate (ENG-209; not an accepted runtime interface):**
ENG-209 measures one Rapier kinematic root with one exact collider per source part,
using the existing `NativeVoxels` or exact `MergedCuboids` representation for
that part. Apply the authored part pose locally under the root. This is one
physics root and one collider per part, never one rigid body per voxel. Measure
geometry agreement and query-to-source-cell mapping for rotated/moving parts.
No live actors may be implemented until the measured candidate and source
provenance are reviewed. The corpse aggregate is a separate feasibility gate;
passing ENG-209 alone does not resolve it. No envelope, frozen-rest hit map,
hidden anchor, implicit joints, or per-voxel rigid-body workaround counts as
exact articulated collision.

## Tick and worker contract

The current `Simulation::tick` order is: increment tick; run staged edits and
publish validated commits; apply committed terrain changes to water boundaries
and step/publish current column water; advance vegetation and weather; apply
buoyancy from the current water frame; step Rapier; advance players. The animal
insertion contract is:

1. Commit canonical terrain edits and publish the resulting current water
   frame, following the existing simulation order.
2. Only after those commits and that water publication, drain completed
   immutable movement/collision worker results. Apply a result only when its
   world epoch, creature generation, per-part revisions, aggregate topology
   revision, terrain revision stamp, water frame revision, clip/pose epoch,
   and requested movement epoch all still match. Otherwise discard it.
3. Validate bounded lifecycle, movement, and damage intents against this
   post-edit terrain and current water. Validate stable identity, policy, and
   budgets. A move job computed from pre-edit terrain can never install before
   this validation point.
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

Water queries return distinct `KnownWet`, `KnownDry`, or `Unknown` results.
`Unknown` terrain, missing body/source volume, exhausted query budget, stale
water frame, or unavailable water freezes the affected move, retains the last
valid stable state, and requests a bounded retry or interest/residency load.
It cannot cause a spawn, teleport, damage, death, or destructive topology edit.
A current `KnownDry` result after water drains invalidates a trout's current
habitat; the fish stops and performs bounded reselection or an explicit
species-owned stranded behavior. Known dry is not an automatic death. Ground
removal invalidates deer support after the commit. The kinematic-root candidate
does not receive Rapier dynamic gravity. For that candidate, the game-owned
deer locomotion integrates bounded velocity/gravity and submits displacement
through exact Rapier sweeps; grounded/support state comes from adapter queries
and a bounded grounded relocation policy, never a hidden support anchor.
ENG-209 must measure this sweep/query path; whether dynamic solver contacts or
responsive actor pushing are required remains a separate integration decision.
Crow movement is bounded waypoint/steering flight using the same current
occupancy queries and adapter. These are the first three simple behaviors; no
general pathfinder or planner is implied.

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
schema:u16, world_id:u128, world_epoch:u64, creature_id:u64, generation:u32,
state_tag:u8, species_key:utf8[1..=63], species_version:u32,
asset_identity:{spvx_uuid:[u8;16], logical_hash:[u8;32]},
material_manifest_hash:[u8;32], material_mapping_version:u32,
material_mapping_hash:[u8;32], clip_name:utf8[1..=255],
clip_phase_tick:u64, clip_subtick_remainder:u32, clip_epoch:u64, clip_mode:u8,
source_part_count:u16,
part_volumes:count × {part_id:u32, source_volume_id:u64,
  topology_revision:u64, root_relative_pose:PartPoseV1,
  effective_render_visible:u8},
aggregate_topology_revision:u64, total_source_cell_count:u32, state_revision:u64,
root_pose:{translation_m:[f64;3], rotation_xyzw:[f32;4]},
linear_velocity_m_s:[f32;3], angular_velocity_rad_s:[f32;3],
pose_epoch:u64, locomotion_tag:u8
```

The `part_volumes` array is strictly ascending by `part_id`; each part ID and
source `VolumeId` occurs once, and each bound source volume belongs to exactly
one creature/corpse assembly. The binding topology revision and aggregate
topology revision are checked together before a result can apply.

For V1 validation, the game-owned species/version catalog entry pins one exact
SPVX asset UUID and logical hash; a known asset cannot be substituted across
species. V1 accepts generation 1 only. Decoders reject source-binding sequence
lengths above 256 before allocating or reading any binding elements, in
addition to the whole-record byte cap.

`PartPoseV1` is a runtime composed transform, not an authored SPVX transform:
finite root-relative translation `[f64; 3]` in metres and a normalized
quaternion `[f32; 4]`; scale is implicitly unit. Normalize once before a pose is
accepted into authoritative state. The bounded pose section stores each part's
composed root-relative pose plus effective inherited render visibility. On
death, `clip_mode` becomes `Frozen`, `clip_epoch` advances, clip advancement
stops, and the reliable lifecycle record/checkpoint stores the final accepted
part poses and visibility with the root pose. Serialize every field using the
versioned DTO encoding, never a native struct or SPVX quantized transform. On
replay, preserve the accepted float bit patterns; do not renormalize or
requantize and shift the corpse pose. During living animation, clients
reconstruct part poses from the validated exact asset and clip name/tick;
per-part poses are not repeated in every motion datagram. Visibility is
render-only: hidden authored cells remain in source volumes, collision, damage
ownership, and persistence. Visibility never removes or disables voxel
topology.

V1 safety limits are fixed: at most 256 creature assembly records per world,
counting both living actors and retained corpses; at most 256 source parts/pose
bindings and 250,000 source cells per assembly; at most 32,768 encoded bytes
per creature record; at most 256 creature records and 8 MiB total encoded
creature-state bytes per baseline transfer; and at most 65,536 encoded bytes per reliable
lifecycle record. Total source-part bindings per world are at most 65,536.
These are bounded-admission safety limits, not a performance target or accepted
population capacity. At the cap, new births are rejected even after death; the
server never deletes a gameplay corpse to make room. Capacity is released only
after legitimate retirement, when all source cells are authoritatively removed
and that removal is durably ordered. Baselines and checkpoints include every
retained corpse under the same 256 record cap. Exceeding a cap rejects
admission/transfer explicitly; it does not truncate. Larger products require a
separately versioned contract.

Reliable ordered records are required for `CreatureCreated`,
`CreatureStateChanged`, `CreatureDied`, `CreatureRemoved`, source-volume
ownership/topology changes, and actor baseline manifest parts. A lifecycle
record carries `world_epoch`, creature ID/generation, monotonically increasing
state revision, server tick, transaction/dependency sequence, and the relevant
part/source-volume revisions. Replaying an identical record at an already-
applied revision is idempotent. The same key/revision with different canonical
bytes is a protocol error and requests repair/disconnect; it is not silently
treated as a duplicate. Gaps hold later records and request repair/baseline.
Motion datagrams may carry
only an existing actor reference plus `world_epoch`, creature ID/generation,
`server_tick`, `state_revision`, `pose_epoch`, root pose/velocity, `clip_name`,
`clip_epoch`, `clip_phase_tick`, and `clip_subtick_remainder`. SPVX clip time is
60 Hz; current `spall_sim::TICK_DT_S` is 1/60 s and the server handshake
advertises `server_tick_hz=60` (`spall_server/src/serve.rs`), so an unpaused
clip advances one phase tick per server tick. Motion snapshots are 20 Hz and do
not set the simulation/animation clock. V1 pins simulation ticks to 60 Hz and
requires `clip_subtick_remainder=0`; any other tick rate requires a versioned
clock-contract review. A future rate must use an integer accumulator and persist
its remainder; it may not truncate a fractional clip rate.

`clip_phase_tick` is unfolded monotonic time rather than the sampled key tick.
Fold integer phase before converting it to the sampler's numeric time:
`once` clamps to duration, `loop` uses phase modulo duration, and `ping-pong`
folds modulo `2 * duration` around the duration endpoint. A zero-duration clip
uses phase zero without modulo, following the existing sampler's empty-time
handling. Integer folding preserves ping-pong direction and avoids precision
loss for large phases while leaving the stored clock unfolded.
A clip switch resets phase and subtick remainder, increments `clip_epoch`, and
discards stale samples from the earlier clip. Phase increment is checked; on
`u64` exhaustion the clip freezes and emits a bounded diagnostic rather than
wrapping. A creature death freezes the last accepted unfolded phase in the
reliable state record and stores exact sampled part poses. Root pose has its
own increasing `pose_epoch`. Replicated snapshots and client interpolation are
presentation only; they never authorize topology, collision, damage, or
lifecycle. Server-sampled poses drive candidate per-part colliders and
authoritative hit queries. Clients interpolate received poses and sample the
validated asset. Missing
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
new actors. The current host refuses reset for persisted worlds. With creature
state enabled, every reset is refused until an atomic replacement path can
publish a new world epoch, durably establish the replacement checkpoint and
journal boundary, invalidate outstanding jobs/requests, and deliver the
replace-world baseline barrier. The monotonic creature allocator is not reset
across epochs. Reset cannot reseed or duplicate actors from generated habitat
data.

## Durable recovery

Creature state and topology must share the existing ordered journal sequence
and checkpoint cut. Add a versioned creature checkpoint section/table and
versioned journal payloads for creation, state changes, death/removal and source
ownership transfers. The durable writer remains the sole writer. Replaying an
identical event at the same world epoch, creature ID/generation, state revision
and journal sequence is idempotent; a divergent payload at that key is a
recovery error. Recovery verifies world ID, manifest/assets/policy versions,
schema, actor-to-volume ownership uniqueness, and topology hashes before
publishing the rebuilt Simulation. The allocator persists `next_creature_id`
and `world_epoch` with existing next entity/volume/transaction/journal
counters. Replay rejects ownerless source cells, double-owned cells, missing
volume references, divergent duplicates, and events after the durable prefix.

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
corpse aggregate after its separate representation gate, proving per-cell
conservation and exact source provenance across damage, death, edits, late
join, and restart. Reset must invalidate old actors and prevent ID reuse.
Unknown/unloaded terrain and
exhausted budgets must not authorize movement or spawn. These CPU and two-client
save/restart/damage/destruction scenarios are required before live animal
support is claimed.

GPU capture and population-scale/frame-time/bandwidth/per-tick query budgets
are separate gates and remain **unrun** by this contract. Preview captures and
habitat candidate tests are not lifecycle, collision, or population acceptance.

## Implementation slices after coordinator review

ENG-209 is reviewed and integrated for exact geometry and source-cell queries;
[its report](reports/ENG-209-articulated-collision.md) retains the original
single-root physical limits. ENG-210's bounded experimental per-source-part
rig addresses those cases through explicit terrain transition queries and
opt-in same-world solver substeps. Contact witnesses identify only geometrically
proved source faces; ambiguous witnesses do not authorize a cell edit.
[Coordinator integration evidence](reports/ENG-210-integration.md) defines the
accepted scope and remaining limits. ENG-212's bounded editable compound and
ENG-213's full-world locality are integrated, with combined evidence in
`reports/ENG-212-213-integration.md`. ENG-214 must now establish explicit corpse
connectivity and split ownership; a rigid compound alone does not accept
disconnected material remaining anchored. That reviewed provenance decision
remains a prerequisite to runtime actor interfaces; the host/transaction slices
below remain proposed.

1. **Geometry/query feasibility (ENG-209):** measure the selected kinematic
   root/per-part collider candidate and exact query-to-source-cell mapping.
   This ticket has no lifecycle/runtime interface dependency and does not
   accept the representation for shipping.
2. **Corpse aggregate feasibility:** determine an editable physical
   representation that atomically owns the existing per-part volumes and
   preserves pose/provenance without revoxelization or per-part auto-explosion.
   This is separate from ENG-209.
3. **Host contract and IDs:** engine-owned bounded actor DTOs, persisted actor
   ID/world epoch counters, a host-side statically linked game behavior hook,
   and invalidation tokens. Likely files: `spall_core` IDs, `spall_protocol`
   DTO definitions, `spall_sim` host state/tick seam, `spall_server` setup and
   restore. Prerequisite: reviewed feasibility conclusions and accepted bounds.
4. **Atomic lifecycle/topology:** creation/removal and alive-to-corpse source
   ownership transactions, provenance metadata on voxel cells, replication
   application and canonical hashes. Likely files: `spall_voxel`, `spall_sim`
   commit/replay, `spall_protocol` topology records, `spall_server` fanout.
   Prerequisite: slice 3 plus reviewed provenance storage format/version.
5. **Collision and movement gate:** Rapier aggregate/query adapter with exact
   source provenance, versioned collision builds, explicit species rig and
   trout/crow/deer bounded locomotion using current habitat queries.
   Likely files: `spall_physics`, `spall_sim`, sandbox species module.
   Prerequisite: ENG-209 gate and slice 4.
6. **Durable/streamed state:** checkpoint/replay schema, journal events,
   baseline/interest/reconnect/reset dependencies, two-client scenarios.
   Likely files: `spall_store`, `spall_server::persist`, protocol baseline,
   server/client assembly, acceptance fixtures. Prerequisite: slices 3–5 and
   explicit schema migrations; no in-place reinterpretation of old saves.
7. **Integrated proof:** generated habitat fixture and fault/restart/damage/
   destruction scenario with measured CPU and two-client results. GPU and
   population performance work stay separate and cannot be inferred from it.

ENG-209 was deliberately independent of runtime lifecycle interfaces. Runtime
actors must not start before ENG-210's movement/rig result, the separate corpse
aggregate decision, and source provenance format have been reviewed.

ENG-214 bounded physical progress (2026-10-16) is integrated through f4ea2ed:
one logical corpse owner can atomically partition source cells among physical
components, preserving original VolumeIds/coordinates/materials/revisions.
Stable component keys use a reserved logical owner key and monotonic serial;
they never encode physics handles. Component-scoped snapshots and keyed
removal/re-split operations keep independently moving frames separate. Material
densities are bound at creation/transfer. Sibling source shards share the
original volume revision, while topology/source-generation checks invalidate
prepared work. These are reviewed CPU adapter semantics, not persisted or
replicated actor DTOs. See reports/ENG-214-physical-review.md for measured
conservation, motion, query/contact and retirement evidence.

ENG-214 remains open: five actual crow/deer poses still reject ambiguous
rotated-cell connectivity. Certified geometric classification and explicit
asset review remain prerequisites; no hierarchy-derived bonds or source edits
have bypassed this gate. Runtime/protocol/storage slices above are still pending.

ENG-214 coordinator acceptance (2026-10-16), superseding the earlier open-gate
status above: bounded source connectivity, atomic physical splits and keyed
fragment editing/re-splitting are reviewed and integrated through8e620d1.
All nine original trout/crow/deer poses now prepare exact source partitions.
Canonical affine interval proofs and exact shared supporting planes with
strict positive projected area certify connections; zero-area/uncertain
contacts never gain a guessed weld. No authored cells or bonds were changed.
The physical/source ownership semantics are accepted for the next host design;
they are not a persisted/wire-format implementation. Evidence, independent
rational oracle, retained historical failures and measured limits:
reports/ENG-214-integration.md. The next implementation task is ENG-215
(host identities, bounded DTOs, registry and scheduling seam). Atomic actor
lifecycle transactions, species movement/habitat, actor replication/recovery
and the combined fully simulated 1024m launch are still pending.

ENG-215 coordinator acceptance (2026-10-16): stable identities, checked allocator
snapshots, bounded standalone state validation and immutable scheduling proposals
are integrated through `32cf744`. Production registration remains unavailable
until atomic source ownership is implemented. Species/version binds exactly one
asset, V1 generation is 1, corpse clips are frozen, and replacement worlds carrying
their own creature identity state reject before mutation. Combined checks pass
334 tests with 9 existing manual/large-scale tests ignored. See
`reports/ENG-215-integration.md`. The next slice is atomic actor lifecycle/source
ownership; this acceptance does not establish live actors, networking or persistence.

ENG-216 coordinator acceptance (2026-10-16 local date): the bounded in-memory
source ownership and birth/death seam is integrated through `cd0b6e3`. Original
source Volumes stay separate from ordinary Bodies. Full source freshness and
retained hierarchy protect death; a checked prepublication transfer validates
the exact resultant corpse motion. Combined checks pass 364 tests with 10
existing manual/large-scale tests ignored, and all 18 release animal corpse
cases pass. Mutation and source storage remain unit-test-only. No journal,
baseline or saved schema was changed. Evidence: `reports/ENG-216-integration.md`.
Next: ENG-217 versioned actor/source ownership and ordered lifecycle replay,
followed by replica/baseline and durable recovery consumers before live spawning.
