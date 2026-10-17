# ENG-216 design: in-memory creature source ownership

Status: coordinator-approved design; implementation remains gated by the
checks listed below. This slice owns an in-memory atomic model and tests only.
It does not publish live actors from a server, add a wire operation, or claim
durable recovery.

## Ownership model

`SimWorld` remains the owner of authoritative voxel `Volume`s. Creature part
volumes live in a separate `CreatureSourceAssembly` collection keyed by the
stable `CreatureKey`; each part maps `(part_id, original VolumeId)` to the exact
`Volume`, source revision/generation, and authored/runtime source pose. A
separate reverse index maps every actor `VolumeId` to exactly one key and part.
These source volumes never enter ordinary `Body`, `bodies`, or
`volume_owner`; no actor part is disguised as a one-volume body. Ordinary
terrain/body lookup and edit semantics remain unchanged in this slice.

The existing `CreatureHost` remains the actor identity/state owner. Lifecycle
entrypoints are coordinated by `Simulation` so a prepared candidate covers
both host and `SimWorld` state. No stable record contains `ArticulatedBodyId`,
Rapier handles, derived component IDs, or another runtime handle. Source part
bindings remain present for the lifetime of the assembly, including a part
whose source volume has become empty. Whole-assembly removal is legal only
after all source cells are authoritatively removed. Partial source editing and
removal are deferred; this slice does not discard source cells to implement
death.

## Candidate transactions

Birth preparation accepts validated immutable creature state plus bounded part
geometry/pose inputs, not caller-selected identity or source `VolumeId`s. It
clones the `IdRegistry` and `CreatureAllocator`, allocates the `CreatureKey`
and each source `VolumeId` from those clones, constructs the exact
`spall_voxel::Volume`s, and rewrites bindings to those allocated IDs at their
initial revisions. It checks identity/catalog limits, duplicate ownership
against terrain, ordinary bodies, and existing creature parts, source-cell and
dense-work caps, geometry/query validity, and complete articulated collider
and mass preparation before publishing either clone or any owner map.

Death preparation takes the current live actor key and expected topology, pose,
clip, per-part source revision, and source-generation tokens. Commit recaptures
and compares those tokens. It freezes the accepted clip, retains every source
VolumeId/cell/material/revision, derives source grids from the exact volumes,
and prepares the ENG-212/214 dynamic aggregate from the current root pose and
twist. Actor state, source owner index, source assemblies, physics aggregate,
and checked counters publish together only after all recoverable fallible
validation and physics preparation succeeds. Stale or failed work changes
none of them. Prepared structs are untrusted inputs: commit revalidates their
version, complete part/binding coverage, allocator snapshots, exact source
content/revisions, tokens, and geometry before mutation.

Production entrypoints remain crate-private/test-only. Existing checkpoint and
baseline capture does not include creature actors, and the current ordered
transaction/replay path has no lifecycle/source-volume operation. Therefore
no populated actor world can be offered as a complete legacy world hash,
checkpoint, or baseline. This slice keeps the test path in-memory and does not
advance the journal or control-sequence counters. Ordered lifecycle replay,
replication, checkpoint schema/migration, and crash recovery must be reviewed
and implemented before production registration is enabled; they must extend
the existing transaction/journal order rather than introduce another journal.

## Canonical compatibility

`CanonicalOwner` currently encodes only Terrain and Body. Keep their canonical
byte streams and all existing world hashes byte-identical for worlds without
creatures. Define an explicitly versioned, domain-separated source/lifecycle
hash over sorted CreatureKey, part ID, source VolumeId, source revision and
canonical source-volume contents for in-memory fixture checks. Lifecycle
state is a separate hash input from topology. Do not return the old complete
`world_hash` for a populated actor world. A later wire/checkpoint slice must
review the versioned combined hash and update every replay/baseline consumer
before exposing a populated world.

## Existing integration seams and remaining prerequisites

Ordinary edits use `stage_edit` and `commit`: a `JobToken` is revalidated, a
cloned `IdRegistry` reserves entity/volume/transaction/journal IDs, candidate
volumes/colliders and a `TopologyTransaction` are prepared, then an infallible
publish appends one `JournalEntry`. `SimWorld::replay_transaction` applies the
ordered ops. Those paths resolve a detached volume through `volume_owner` to
one `Body`; canonical world hashing enumerates terrain and `Body` records.

The ENG-214 fracture planner and keyed editing live in the `spall_physics`
adapter. They preserve source-cell provenance in derived collider masks, but
do not own authoritative voxel `Volume`s or CreatureKeys. The physics
articulated constructor validates transforms, every source grid, density,
mass, collider work and poses before its first Rapier insertion; after that
boundary its insertion/publication path has no recoverable `Result` failure.
The dynamic transfer similarly validates tokens, densities, mass and
replacement colliders before replacing old colliders. Simulation must still
prepare source volumes, cloned counters and all other fallible checks before
calling those publish operations; it must not call physics first and then
perform fallible world/host validation.

The following slice must add the versioned lifecycle/source-volume operation
to the existing transaction and journal/replay path, update checkpoint and
baseline formats with explicit migration/versioning, and prove durable
recovery and replica application before production actor registration. This
design deliberately does not change `TopologyTransaction`, `WireTag`, store
schema, or the legacy canonical hash.

## Required in-memory invariants

- Birth allocates actor and all source-volume IDs from cloned checked counters;
  success installs each ID once, while invalid definitions, duplicate or
  foreign ownership, counter exhaustion, source/material/revision mismatch,
  and collider preparation failure preserve the full pre-state.
- Death transfers the same source IDs, cells, material IDs, revisions, and
  geometry exactly once; wrong-owner, stale topology/pose/clip/source tokens,
  pending source motion, and physics preflight failure preserve the living
  owner and physics counts.
- Ordinary Body lookup/edit/canonical hashes do not include actor parts.
- Stale prepared plans cannot alter sibling actors or advance any counter.
- Empty source parts retain binding metadata; an actor can be removed only
  after all source parts are empty.
- Existing ordinary world hashes and transaction replay results remain
  byte-identical.
