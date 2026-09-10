# G3 row 7 — cache-placement-independent logical topology (design + slice plan)

**Status:** design amended per the 2026-09-10 review
(`docs/reviews/2026-09-10-g3-residency-handling.md`). **Slice A implemented**
(this increment). Slices B–E open. Residency stays **default-off**; row 7 stays
open and row 8b stays dependent on it. This document is the frozen contract; it
does not certify a gate.

## The blocker (unchanged)

`docs/validation.md` G3 needs a 256 x 128 x 256 m bounded world with *"resident
cache limits low enough to force eviction"*. T18 built the whole residency
subsystem (`spall_server::residency`, `spall_voxel::residency`,
`spall_client::ClientResidency`) and its unit fixtures, but deliberately left
`spall_server::serve` unwired.

`SimWorld::world_hash()` and `ReplicaWorld::world_hash()` both fold
`canonical_volume_for` / `canonical_volume` over **resident bricks only**, and
the server and clients converge by comparing that value. Evicting one clean
terrain brick mid-run changes the reported hash on that side.

## Why a reporting-only hash fix is not enough (review findings)

The first design proposed only a residency-aware *reporting* hash. The review
rejected that as insufficient. Cache-resident-only state also leaks into:

| Area | Where | Consequence |
| --- | --- | --- |
| **Transaction validation** | `spall_sim::commit` computes parent `result_hashes` with `volume_topology_hash_for`; the replica hashes the **entire candidate volume** before committing. | Reloading only the *edited* bricks is insufficient — an untouched evicted brick in the same volume must still contribute to the candidate hash on both ends. |
| **Structural staging** | `spall_sim::stage::stage_edit` builds its structural index with `ResidencyMode::AllResident`, which treats absent neighbours as empty. | An edit staged against evicted terrain can mis-classify support. Dependency-complete or streamed staging is required before eviction is enabled. |
| **Baselines / repair** | `spall_server::serve::capture_for` → `baseline::capture_transfer`; `baseline::world_baseline` / `baseline_volume` / `brick_repair_patch` snapshot **live resident** geometry. | The claim "live late-join baselines already come from durable records" was wrong. These paths need a coherent live-plus-backing view. |
| **Recovery** | `--replay` reconstructs fully resident; equality is conditional on complete checkpoints and residency-neutral `result_hashes`. | Re-run replay for the wiring slice; it is not automatically unaffected. |
| **Conservation** | `SimWorld::total_solid_cells` calls resident-only `solid_cells`. | Whole-world conservation must use *logical* counts; digests carry `solid_cells` and are updated atomically with topology. |
| **Interest** | `spall_voxel::residency::update_interest` replaces each entry from **one** centre. | Calling it once per player discards earlier players' interest. Compute the union of all players + relevant active-body trajectories. |
| **Backing** | `MemoryBrickBacking` keeps payloads in a `BTreeMap`; `enforce_budget` persists synchronously. | Moving payloads to another in-process map is not a process-memory reduction or crash durability. A real bounded backing path + ack rules + snapshot accounting are required. |

## Frozen contract

### One logical topology, independent of cache placement

For each live volume, its **logical brick set** is resident bricks **plus**
explicitly retained evicted-brick digests. Every logical key
`(VolumeId, BrickCoord)` contributes **exactly once**. A missing cache entry is
neither a deletion nor proof of air. Modified-air ("mined") bricks are retained
with their real revisions and never dropped to make a hash match.

The canonical encoding is **unchanged**: `spall_protocol::canonical_topology_hash`
over `spall.topology.v1` canonical records — volume IDs, cell-size codes,
ownership, sequence lengths, coordinate/revision encoding, and the
material-layer kind + 32 content-hash bytes. Volumes sort by ID, bricks by
`(z, y, x)`. Motion stays outside this hash. The value must be **byte-identical**
to today's under full residency:

```text
logical_hash(full_world, no_evictions)            == existing_world_hash(full_world)
logical_hash(evict_any_subset(full_world), digs)  == existing_world_hash(full_world)   # any order
```

and the equivalent per-volume identity, including **staged transaction
candidates**. Hash comparison is meaningful only at the same committed
transaction frontier and the same logical coverage.

### Crate boundaries

- **`spall_voxel`** owns the pure brick-digest record and its lifecycle (a
  *parallel* record, not an overload of `GraphBrickMeta` — graph summaries
  answer structural questions; digest membership defines exact topology
  coverage; both describe the same revision). No network or store runtime.
- **`spall_protocol`** keeps canonical encoding + hashing.
- **`spall_sim`** and **`spall_client`** each adapt the logical brick set into
  `spall_protocol` canonical records — the same records, no `spall_server`
  import.
- **`spall_server::residency`** owns cache policy, I/O, and durable
  acknowledgements. A host convenience `residency_world_hash(...)` may exist but
  is **not** the only place the contract lives — the sim commit path and the
  client candidate validator use the same logical view.

### Digest lifecycle

| Transition | Required invariant |
| --- | --- |
| **Evict** | Capture `(revision, content_hash, solid_cells, modified_air)` from the current snapshot. On the server, require a durable ack of *that exact revision* first. Publish the retained digest and drop the geometry together, on the owning thread. A failure leaves geometry available. |
| **Reload** | Validate key, revision, and content against the retained digest before installing. Drop the digest only after a successful install. Failed / unavailable / mismatched loads keep the digest and stay pending/error. |
| **Edit or split** | Obtain required geometry first. Candidate hashes combine changed resident state with **unchanged evicted contributions**. Publish geometry, ownership, and the superseding digest atomically after validation. Failed/stale candidates change none of them. |
| **Full baseline / recovery** | Replace the logical state and the whole digest namespace atomically; invalidate old-world / old-baseline entries. |
| **Repair / bulk-split patch** | Reconcile only authoritative covered keys/volumes; preserve unrelated evicted state; clear superseded entries; never count old and new ownership twice. |
| **Volume retirement** | Remove the volume's digests + backing refs under the same logical-deletion contract. Eviction alone never retires a volume. |

Retained state is scoped to the current world/session generation. Reject a
coord that is *both* resident and evicted rather than silently choosing one; a
resident update supersedes an old digest only through an explicit atomic
transition.

### Known-empty loads

T18's `load_brick` materialises a `KnownEmpty` slot as a revision-0 air brick,
which **adds** a canonical brick that was not previously represented. Freeze:
either the same explicit empty records exist in both comparison runs' starting
state, **or** known-empty availability is represented without adding a new
canonical brick. Never silently drop a modified-air record to make hashes match.
This needs its own test (slice A covers modified-air round-trip; the
`KnownEmpty` rule is finalised in slice C where `load_brick` is on the path).

### Client scope

Locally retained digests need **no new protocol fields** *when* a client started
from the complete logical baseline and applied every relevant transaction
through the comparison frontier — it can hash an untouched evicted brick. It
**cannot** reconstruct that brick's cells from the digest to apply a later edit;
existing geometry-repair / baseline traffic may still be needed ("no digest
messages" ≠ "no reload traffic").

**First server wiring slice keeps client eviction off** and tests against fully
resident replicas — an independent hash oracle, reviewable scope. Symmetric
client residency (bounded local reload or repair/baseline, pending-transaction
handling, candidate digest validation) is a later slice.

A future near-player-only baseline **cannot** produce a global terrain hash for
terrain it never received. This G3 comparison keeps **full logical coverage**; a
scoped/regional hash needs its own contract and must not be smuggled into row 7.

## Safe serve-loop integration (slice D)

An end-of-tick eviction call alone is insufficient. Per tick:

1. **Before dispatching work**, compute required geometry from the union of
   player interests, swept body/player paths, pending edits, and structural
   dependencies. Reserve capacity and load it. Missing geometry defers the
   affected operation — never sampled as air.
2. **Stage** against immutable dependency-complete snapshots or a streamed path
   that resolves unknown support; keep revision/generation validation on commit.
   Retained graph metadata cannot establish support on its own.
3. **Apply** validated transactions at the tick boundary, updating logical
   hashes, counts, owners, and dirty state together. Include contact/strength
   edits and bulk-split paths in the audit, not only player edits.
4. **Refresh** cache accounting and the union interest. Track scoped pins for
   active consumers; release on completion / rejection / cancellation /
   disconnect. Pin geometry that later validation will need.
5. **Persist** bounded work; process exact-revision acks; evict only eligible
   clean state. If the working set cannot fit, report capacity pressure and
   defer — never silently claim the budget passed.
6. **Capture** baselines/checkpoints at a single tick + journal cursor from
   immutable live snapshots + exact-revision backing records; continue bounded
   catch-up after that cursor. Capture must not reload the whole live cache.

Audit sites: normal + shutdown checkpoints in `serve.rs`, baseline requests and
catch-up recaptures, one-brick repairs, bulk-split capture. Dirty live revisions
override older backing records; async reads need an immutable version view.
`--replay` keeps its CLI and fully-resident reconstruction, but must be re-run.

## Memory and observability

Define whether the brick limit covers **terrain only** or all cached geometry
(the controller pins dynamic-body bricks). Report terrain and body counts
**separately**, plus total cache bytes. Pinning can keep `enforce_budget` from
reaching the ceiling — an eviction count alone does not prove success.

Record: eviction/reload totals; resident terrain min/max/final; evicted-brick
occupancy over time; longest sustained eviction; blocked-load / pin counts;
budget misses; digest-metadata bytes; backing memory; retained
job/baseline/checkpoint bytes; hash p95/max duration; process peak memory. Label
bootstrap/transient separately from a steady-state "maximum". Version the
summary per existing conventions and update the harness readers together. The
digest fold covers the logical extent (records sorted, canonical vectors
allocated) — do **not** promise a strictly linear tick cost; measure it on the
named workload.

## Slices and acceptance

| Slice | Deliverable | Evidence before proceeding |
| --- | --- | --- |
| **A — contract + digest foundation** ✅ | `spall_voxel::logical`: `BrickDigest`, `EvictedBricks` (record/verify_reload/clear/supersede/clear_all lifecycle), `logical_bricks`, `logical_solid_cells`; `spall_sim::canonical_logical_volume_for` adapter. | Existing-hash equality with no eviction; arbitrary subsets **and** eviction orders; negative coords; multiple volumes; modified air round-trips (not dropped); non-resident capture rejected; stale/duplicate digest rejected; mismatched reload keeps the digest. **Done — `cargo test -p spall_voxel --lib logical` (10) + `cargo test -p spall_sim --test logical_hash` (3).** |
| **B — transaction + structural correctness** ✅ *(this increment)* | `SimWorld` / `ReplicaWorld` carry an optional per-volume `EvictedBricks` (empty → zero change). `world_hash`, per-transaction parent `result_hash`, the replica's candidate-hash check, and `total_solid_cells` / staging `pre_solid`+`post_solid` all fold the logical view. Staging (`stage_edit`) and commit refuse — **without mutating** — when an edit writes, structurally borders (touched bricks + detached components, grown one brick), or its collider rebuild would sample an evicted brick (`StageError::EvictedGeometryRequired`, `CommitError::EvictedGeometryRequired`). `SimWorld::evict_brick` / `clear_evicted_after_reload` implement the evict/reload transitions; a full baseline clears the namespace. | `cargo test -p spall_sim --test logical_commit` (4): evicting an untouched brick moves neither the parent `result_hash`, `world_hash`, nor `total_solid_cells`; hash + conservation stable across eviction order; staging refuses a cut whose support path runs through an evicted brick and leaves the snapshot untouched; a cut clear of the evicted brick still stages + balances. `cargo test -p spall_client --test logical_residency` (3): a replica that has evicted clean bricks still `Published`-validates a full-server transaction and converges; the server's reported hash is unchanged before/after it evicts a clean brick; evicting a replica brick does not move its `world_hash`. Full `cargo test --workspace --all-features` green (residency default-off path unchanged). *Deferred to C:* dependency-complete reload-and-retry so a far-but-irrelevant eviction no longer blocks a commit, and the `KnownEmpty` membership rule. |
| **C — backing + capture** | Bounded backing integration; complete late-join / repair / checkpoint / shutdown capture; finalise the `KnownEmpty` rule. | Join/reconnect while terrain stays evicted; repair an evicted brick; checkpoint with newer dirty live revisions; cold restart + crash-prefix recovery; no restored removed cells or retired volumes. |
| **D — default-off server wiring** | `ServeConfig.residency` + CLI; load/read/evict boundaries; summary + harness fields; clients fully resident. | Sustained eviction under a declared achievable limit; all mandatory edits complete; **on/off agreed hashes equal**; exact replay + late join match; every budget miss stated. |
| **E — client residency + traversal (row 8b)** | Client digest lifecycle; bounded reload/repair; then row 8b traverse-away/back before save. | Different server/client eviction subsets still converge; edit while replica terrain is evicted; travel away/back, save/restart; verify revisions, ownership, counts, replay — no regrowth. |

Paired on/off runs use the **same** scene, logical starting brick set, seed, and
authoritative transaction sequence (record committed IDs/order + any automatic
damage events; wall-clock timing alone does not guarantee equal input). Keep
some bricks evicted **during** edits, baseline capture, and the final hash
sample — a full reload just before assertions would hide the defect.

## Re-run during implementation

```sh
cargo test -p spall_voxel --lib logical
cargo test -p spall_sim --test logical_hash
cargo test -p spall_server --test residency
cargo test -p spall_client --test residency
cargo xtask scenario --name t23-g3
cargo xtask scenario --name t23-g3 --loss-percent 3
cargo xtask scenario --name t23-g3-impaired-join
cargo xtask crash-test --suite persistence
cargo test -p spall_store --test abrupt_crash
cargo xtask check
```

`--residency-budget-bricks` and a paired traversal scenario stay **proposed**
until the slice-D CLI and fixture exist. Record exact commands, raw summaries,
seeds, and measured results in `G3.md`.

## Not covered by this contract

Row 11 (join-duration budget) is the next independent T23 slice while B–E
proceed. Rows 12–14 (G4 eight-client workload + soak) are separate. Neither the
PR stack nor a passing CPU suite establishes the engine gate.
