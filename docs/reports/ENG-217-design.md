# ENG-217 design: canonical creature ownership and lifecycle reduction

Status: coordinator-approved implementation boundary, 2026-10-17.

## Scope and compatibility

This slice adds reusable versioned creature/source ownership values and a pure
candidate-first lifecycle reducer. It does not add a wire operation, journal
entry, baseline section, checkpoint row, store migration, server publication,
or production registration. In particular, it does not change
`TopologyTransaction`, `TopologyOp`, `WireTag`, `WIRE_SCHEMA_VERSION`,
`BASELINE_WORLD_SCHEMA`, or `STORE_SCHEMA_VERSION`.

The reducer consumes a caller-supplied existing transaction context and its
expected before hash. It owns no transaction, control, journal, or sequence
allocator. A following integration slice must carry these exact records and
call this reducer from the existing ordered transaction/journal path, then
extend replica/baseline and durable recovery consumers before production
registration is enabled.

## Portable ownership record

`CreatureOwnershipRecordV1` is a bounded protocol DTO with its own schema and
canonical hash domain. It contains the existing `CreatureStateV1`; each bound
source part's original `VolumeId`, part id, cell-size code, bounds, source
generation, sorted resident brick records, full material cells, brick revision
and edited flag; and the static parent/initial transform plus current frozen
local pose for every part. Runtime `Volume::state_stamp`, Rapier or ECS handles,
and process-local allocator data are excluded. Source freshness is represented
by portable source generation, per-brick revisions, and a canonical content
hash over the exact source bytes and metadata.

The record also carries sorted stable physical component serials and exact
source-cell masks. Assigned serials are nonzero and below
`next_component_serial`, which is a monotonic high-water mark; it remains
advanced when only one formerly fractured component survives. A mask entry
identifies the original source volume, part,
integer cell coordinate, material and source revision, matching ENG-214's
`SourceCellRef` semantics. Validation proves that every occupied source cell
appears in exactly one component, no mask references air or a foreign/stale
cell, component IDs are unique under the logical owner key, and all component
frames and twists are finite and within the existing creature DTO bounds.
Sources retain their complete authoritative brick data; component masks do not
revoxelize, duplicate, or replace it.

The decoder checks the outer byte limit and bounded declared sequence lengths
before allocating nested vectors. Its 16 MiB decode budget is a conservative
estimate of decoded memory, including fixed record overhead and nested vector
capacities, rather than only the serialized byte length; a record below the
serialized ceiling can therefore still exceed the memory estimate. It enforces
canonical order for owned records and checked coordinate/range arithmetic.
Empty resident bricks remain in the source record with their exact revision
and edited flag; air cells are not expanded into per-cell entity records or
emitted as separate mask entries.

The format can validate multi-component frozen corpses, but this reducer does
not create fractured records. Lifecycle death must preserve identical source
VolumeIds, brick bytes, revisions and source hash, and must supply the exact
frozen local hierarchy and final transferred root motion. Source edits,
fracture, re-split and partial removal are rejected until a later synchronized
source/physics operation defines them. No hierarchy-based welding or
component coalescing is allowed.

## Candidate-first reducer

Protocol code owns portable DTO validation and canonical source/owner hashing.
The reducer operates on the affected creature record and supplied allocator,
receipt, witness and collision metadata; it has no physics or game dependency.
An input change is explicitly versioned and names the caller's existing
transaction context, creature key, operation, expected before record hash and
complete after record where applicable.

Birth validates an Alive/Playing record against the supplied allocator
snapshot, exact source IDs and material/asset manifest, then stages the record
and source ownership in a clone. IDs must be exactly those reserved by the
authoritative caller's cloned registries; this DTO reducer never allocates
them. Death checks the complete current before hash and Alive state, then
accepts only a Corpse/Frozen after record whose source bindings, full source
bytes, revisions and content hash match the existing owner. It validates the
frozen hierarchy and exact final motion before replacing the candidate record.
The reducer publishes the whole candidate only after every check succeeds.

An exact repeat is a no-op only when the current record matches the change's
after hash and change identity. Reusing an identity with different contents,
applying against a divergent before hash, or attempting unsupported source
mutation rejects without changing records, ownership maps, or supplied
counters. The reducer does not choose replay ordering: the existing journal
caller supplies the order and context.

## Canonical compatibility and follow-up gate

Creature owner state hashes under a separate versioned domain. The existing
Terrain/Body owner encoding and `spall.topology.v2` preimage remain unchanged;
no-actor world hashes are pinned by regression tests. Creature ownership is
not folded into the legacy complete-world hash, and an actor-bearing state
cannot be exported as a complete legacy world hash or baseline.

The later consumer slice must explicitly version an ordered transaction
representation and teach the existing replica, baseline capture/install,
server fanout, checkpoint, and journal recovery paths to carry these records.
Those consumers must reuse this record validator, canonical hash and reducer.
Until then, production actor registration/publication remains disabled.

## Invariant test plan

- Canonical round trip and input-order independence; each identity, source,
  hierarchy, component mask, motion and lifecycle field is bound by validation
  and/or the canonical hash.
- Oversized declared nested counts reject before allocation; reject malformed,
  noncanonical, duplicate, overlapping, missing, air, foreign and stale masks,
  unknown materials, invalid transforms, and arithmetic overflow.
- Bridge the ENG-216 authoritative birth/death fixture into the record and
  compare exact source IDs, brick content/revisions, retained hierarchy,
  frozen local poses and transferred motion.
- Candidate birth/death success and stale, duplicate, divergent, allocator,
  material and unsupported-edit failure preserve exact source ownership and
  candidate counters; replay exact repeats idempotently.
- Pin legacy no-actor hash bytes and reject actor state from legacy complete
  hash/baseline paths.
