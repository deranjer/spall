# ENG-216 design: in-memory creature source ownership

Status: coordinator-approved design; the bounded in-memory lifecycle seam is
implemented under `cfg(test)`. Production actor registration remains gated by
ordered transaction replay and checkpoint/baseline support. This slice does not
publish live actors from a server, add a wire operation, or claim durable
recovery.

## Ownership model

`SimWorld` remains the owner of authoritative voxel `Volume`s. Creature part
volumes live in a separate test-only source assembly keyed by the stable
`CreatureKey`; each part maps `(part_id, original VolumeId)` to the exact
`Volume`, its full birth freshness token, and its authored hierarchy metadata.
A separate reverse index maps every actor `VolumeId` to exactly one key and
part. The freshness token contains the volume state stamp, canonical
source-volume content/revision hash, and every resident brick coordinate and
revision. The state stamp is a runtime stale guard, not part of the deterministic
source hash. At death, the store also retains each authored parent ID and
initial parent-local transform beside the current frozen parent-local pose;
these reconstruct the hierarchy without physics handles.
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

The current direct test-only birth call accepts an already-created DTO and
bounded candidate part volumes. It clones the `IdRegistry` and `CreatureHost`,
then requires the DTO key and each candidate source `VolumeId` to be exactly
the next checked allocations from those clones; it does not construct source
volumes or rewrite caller state. It rejects anything except Alive/Playing,
requires the DTO and supplied catalog material manifest hashes to match the
authoritative `SimWorld` manifest, and checks every occupied source material
against that manifest and the asset's mapped portable keys. It then validates
complete part/binding coverage, exclusive ownership against terrain, ordinary
bodies, and existing creature parts, source-cell and dense-work caps,
geometry/query validity, and complete articulated collider/mass preparation
before publishing cloned counters, host record, and owner maps. The entrypoint
is synchronous; there is no externally retained prepared-plan DTO or later
stale-plan commit boundary in this slice.

Death preparation takes the current live actor key and expected topology, pose,
state, and per-source freshness tokens. It rechecks each source stamp, hash, and
sorted brick revision, not merely the maximum brick revision, and freezes the
current local hierarchy poses while preserving parent/initial-transform
metadata. It derives source grids from those same volumes. The physics adapter
computes the corpse's resultant rigid-body twist from all part momenta and
prebuilds all replacement colliders, then invokes a value-only owner validation
closure before its first mutation. That closure stores the exact resultant
twist in the corpse DTO and validates the replacement host candidate;
rejection leaves physics unchanged. After acceptance, physics publication and
the prevalidated source/host swaps have no recoverable failure path. This is a
direct synchronous test seam, not a general prepared transaction API.

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
creatures. The fixture-only, explicitly versioned source hash covers sorted
CreatureKeys, part IDs, source VolumeIds, and canonical source-volume contents.
It is deliberately source-only: it does not cover the runtime freshness stamp,
authored parent/local transforms, current frozen poses, or lifecycle DTO. Those
are retained in separate records and checked by targeted assertions; a future
combined actor-state hash must include their canonical versioned
representations. Do not return the old complete
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

## Implemented checkpoint

The test-only birth seam in `Simulation` validates the V1 state against its
catalog, allocates the next creature key and source volume IDs on cloned
counters, derives occupancy from the exact source `Volume`s, validates exclusive
source ownership, and prebuilds articulated physics before publishing the host,
source owner maps, and ID registry. The test-only death seam revalidates the
state revision, source bindings/revisions, and current physics topology/pose
tokens; it rejects pending poses, freezes the clip, captures current root
motion and per-part local poses, binds density to materials present in source
cells, then performs the prepared ENG-214 transfer before replacing the host
record. Failure tests cover invalid geometry, a caller-selected foreign
VolumeId, stale and pending death tokens, and unchanged counters/host/physics.

`SimWorld` stores actor source volumes in a test-only owner map and reverse
index. They do not enter ordinary `Body`, `volume_owner`, `volume_ref`, or
`body_by_volume` paths. A distinct source ownership hash has its own v1 domain;
the legacy `world_hash` fails closed in test builds when source owners exist.
The physics crate exposes read-only value snapshots of live articulated owner
tokens/root motion and sorted applied part poses; no runtime handle escapes.

The fixtures prove a one-part lifecycle transfer plus a three-part nested,
rotated-root transfer with mixed mapped materials, per-part query-identity
checks, source-hash preservation, hierarchy reconstruction metadata, and
independent density-derived mass conservation. A stale-source regression
reconstructs candidate source data with changed content in a low-revision brick
while preserving a separate high-revision brick at revision 5; the maximum
remains 5 but the volume stamp and canonical hash change, so death is rejected.
This covers same-maximum replacement/reload drift; normal `apply_edit` advances
the volume-wide revision high-water mark. They do not
provide production species construction, an actor wire/journal operation,
checkpoint or baseline coverage, replica application, partial source edits, or
split-component ownership integration. The only supported lifecycle entry
points remain compiled out of production builds until the ordered replay and
durability slice lands.

Validation in the dedicated worktree passed `cargo test -p spall_sim -p
spall_physics`, `cargo clippy -p spall_sim -p spall_physics --all-targets --
-D warnings`, `cargo fmt --check`, and `git diff --check`. Existing ignored manual
and measurement tests were not run.
