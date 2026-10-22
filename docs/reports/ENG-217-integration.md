# ENG-217 integration: portable animal ownership and lifecycle reduction

Date: 2026-10-17. Coordinator review of the explicitly narrowed ENG-217 scope.

## Delivered behavior

`spall_protocol::creature_owner` defines versioned portable animal ownership,
original source volumes and bricks, hierarchy and local poses, exact component
cell masks, stable component serials and their allocation high-water mark.
Validation binds the selected asset's material keys, complete disjoint occupied
cell coverage, revisions, source content, finite motion and rooted hierarchy.
One remaining derived component retains its original serial and retired serials
stay consumed. Uniform and equivalent dense bricks have the same content hash;
resident air bricks, edited flags and revisions remain significant.

The pure lifecycle reducer validates before cloning the affected record. It
checks the caller's allocator snapshot, reserved source IDs and existing owner
metadata. Birth requires fresh lineage and zero initial motion. Death preserves
source identity, static definition and exact source data, and requires a checked
physical transfer witness for frozen poses and final motion. Exact replay needs
the caller's receipt binding transaction ID, operation ordinal and change hash.
No separate ledger or sequence is allocated here.

The scoped decoder shares cumulative budgets across nested parts, bricks,
dense cells, component masks and mask cells. Declared excess rejects before
vector reservation; streams without size hints are charged as elements arrive.
The 16 MiB conservative decoded-memory estimate is intentionally stricter than
the separate 16 MiB serialized-byte cap. Optional JSON fields still default to
None; required fields and postcard sequence fields remain required.

The test-only simulation bridge reads actual ENG-216 source ownership after
birth, applies movement to a three-part nested hierarchy with a rotated root,
and performs checked death. Portable reduction preserves original VolumeIds,
source bricks/revisions, parent/initial transforms, frozen poses and transferred
motion. It also checks selected-asset material rejection and aggregate limits.

## Review and changed files

- `crates/spall_protocol/src/creature_owner.rs`: records, validation, canonical
  hashes and candidate reducer; `src/lib.rs` exports them.
- `crates/spall_protocol/src/creature_owner_decode.rs`: cumulative decoder,
  preallocation bounds and optional-field compatibility regressions.
- `crates/spall_protocol/tests/creature_owner_review.rs`: 13 independent
  coordinator tests, including exact mask rejection, collision/exhaustion,
  canonical source metadata, hierarchy, replay and death witness contracts.
- `crates/spall_protocol/tests/creature_legacy_hash_review.rs`: legacy terrain
  plus body hash pin, unchanged by animal records.
- `crates/spall_sim/src/creature_owner_bridge.rs` and the test module hook in
  `sim.rs`: actual simulation-to-portable three-part lifecycle bridge.
- `docs/reports/ENG-217-design.md`, `docs/animal-lifecycle.md` and `docs/tasks.md`:
  reviewed scope, evidence and next integration requirement.

Independent review corrected cumulative allocation bounds, validation before
duplicate replay, source/material binding, retained component allocation state,
and missing optional map fields. Coordinator added failure cases for foreign,
air, stale, missing and overlapping ownership, ID collisions, counter exhaustion
and source metadata hashing. Legacy terrain/body hash remains
`83da04a97d7864ada9de47468b9c285c770eb371cc6a48e48ac51579a013a025`.

## Validation

Integrated checkout checks:

- `cargo test -p spall_protocol -p spall_sim`: **331 passed, 0 failed,
  9 existing ignored**. This includes 77 protocol library tests, 13 independent
  ownership review tests, the legacy hash pin and the three simulation bridge
  tests. Raw output: `.local/eng217-integrated-tests.log`.
- `cargo check --workspace --all-targets --all-features`: passed; filesystem
  incremental-cache hard-link warnings were emitted. Raw output:
  `.local/eng217-workspace-check.log`.
- `cargo clippy -p spall_protocol -p spall_sim --all-targets -- -D warnings`:
  passed (filesystem incremental-cache warnings only). Raw output:
  `.local/eng217-clippy.log`.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

The ignored cases are existing manual collider/proximity/full-world diagnostics,
the large collapse scenario and demo frame dumps. The test total covers the two
listed packages; it is not a full-workspace test or a population benchmark.

## Remaining launch requirements

This slice changes no TopologyOp, wire schema (7), baseline schema (1), saved
schema (3), journal or production spawning path. The reducer is reusable code;
it is not yet called by real network or durable recovery consumers. Simulation
source storage and lifecycle mutation remain unit-test-only. Source edits,
fracture and terminal removal reduction remain unsupported.

The next unblocked task is ENG-218: coordinated ordered transaction, baseline/replica
and checkpoint/recovery integration using these same records and reducer.
It needs explicit version handling and a topology hash that excludes motion;
the complete ownership hash here includes motion and cannot replace the legacy
topology hash. Source edit/fracture synchronization, species/current habitat,
two-client restart/destruction proof and measured population/GPU performance
remain necessary before the fully simulated 1024 m weather-world launch.
No combined launch, two-client, crash-recovery or GPU gate was run in this slice.
ENG-216's previously measured 18 corpse cases are historical evidence, not a
new measurement in this protocol-only change.
