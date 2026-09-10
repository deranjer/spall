# G3 row 7 — cache-placement-independent logical topology (design + slice plan)

**Status:** design amended per the 2026-09-10 review
(`docs/reviews/2026-09-10-g3-residency-handling.md`). **Slices A–D + E1
implemented.** Slice E2 (client residency in the live session + row 8b
traversal) open. Residency is a **default-off** `ServeConfig` knob — row 7 is
covered when on; the client reload digest lifecycle is in place; row 8b still
depends on E2. This document is the frozen contract; it does not certify a gate.

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
| **C — backing + capture** ✅ *(this increment)* | `spall_sim::BrickBacking` trait + `MemoryBacking`; `SimWorld::set_backing` + `reload_brick` / `reload_bricks` (load from backing → reinstall → `verify_reload` → drop digest; `KnownEmpty` → air brick at the retained revision). The edit pipeline catches `EvictedGeometryRequired` from stage **and** commit, reloads the named bricks, and re-stages next tick; no backing / `Unavailable` → a bounded explicit `evicted geometry unavailable` rejection with nothing mutated. `spall_server::logical_world_baseline` / `logical_capture_transfer` / `logical_brick_repair_patch` fill evicted bricks from the backing so a late joiner / repair still reaches the exact hash; `world_baseline` = `logical_world_baseline(_, None)` (unchanged when nothing is evicted). | `cargo test -p spall_sim --test logical_reload` (3): a seam cut whose region needs an evicted brick reloads it and commits to the same world as a fully resident run; with no backing it is rejected cleanly and the world is untouched; a wrong backing record is refused by `verify_reload` and keeps the digest. `cargo test -p spall_server --test logical_baseline` (3): a late-join baseline over an evicted server reconstructs the full topology on a fresh replica; a one-brick repair patch covers an evicted brick at the right revision; `world_baseline` is byte-unchanged with no evictions. Full `cargo test --workspace --all-features` green. *Deferred to D:* routing `ResidencyController::enforce_budget` through `SimWorld::evict_brick`, bridging the durable store to `BrickBacking`, and checkpoint capture via `ResidencyController::capture_checkpoint` in the serve loop. |
| **D — default-off server wiring** ✅ *(this increment)* | `ServeConfig.residency` (`None` → byte-identical to today) + `sandbox-server --residency-budget-bricks` / `--residency-radius-bricks` + `session.rs` scenario passthrough. `spall_server::residency_pass::ResidencyPass`: seeds an `Arc<MemoryBacking>` from terrain, installs it on `SimWorld`, and per tick keeps a brick box around every player capsule resident while evicting out-of-interest terrain after a `EVICT_SETTLE_TICKS` hysteresis (so it does not fight the edit pipeline's reload-and-retry). `on_commit` refreshes the backing for touched bricks; `reload_all` before every periodic + shutdown checkpoint so `persist::capture` snapshots the whole world. Late-join baseline + brick-repair capture go through `logical_capture_transfer` / `logical_brick_repair_patch` with the pass's backing. Collider-rebuild `EvictedGeometryRequired` now names **every** evicted brick in the volume, so one reload makes the candidate whole and the retry commits next tick rather than churning one brick at a time. `ServeSummary` v4 adds `residency_evictions_total` / `residency_reloads_total` / `resident_terrain_bricks_{min,max,final}` / `residency_budget_miss_ticks`. | `cargo test -p spall_server --test residency_pass` (2): a live `Simulation` driven twice from the identical edit script — with and without `ResidencyPass` — reaches the **same** `world_hash`, `total_solid_cells`, and per-transaction `result_hashes` trace, while the pass really evicts (`evictions_total > 0`, resident count drops below the start, the out-of-interest region is evicted when its cut arrives) and the pipeline reloads on demand (`retried > 0`); and default-off (`no install`) is a bit-exact no-op. `cargo xtask scenario --name t23-g3-residency`: `t23-g3` + `--residency-budget-bricks 6 --residency-radius-bricks 0` → server + 4 clients + `--replay` + cold `--restart` all converge to `cac2893c…d18e6de` (the residency-off agreed hash), `56` evictions / `7` reloads / min `2` of `9` resident terrain bricks over the run. Full `cargo fmt --all --check` + `cargo clippy --workspace --all-targets --all-features -- -D warnings` + `cargo test --workspace --all-features` green. *Deferred to E:* unifying this pass with `ResidencyController::enforce_budget` / `capture_checkpoint`, bridging the durable store (not just an in-memory `MemoryBacking`) to `BrickBacking`, and incremental (non-full-reload) checkpoint capture. |
| **E1 — client reload digest lifecycle** ✅ *(this increment)* | `spall_voxel::EvictedBricks::drop_resident(volume)` — drop every retained digest whose brick is resident again (a traversal reload at the same revision or an authoritative repair patch that healed a `before` gap at a newer one). `ReplicaWorld::apply_baseline_patch` and the transaction-commit path call it, so the logical view never carries a resident-**and**-evicted brick. `ClientResidency::wanted_reloads(replica, centre, enter_radius, max)` names retained-digest terrain bricks back inside interest for a bounded `RepairRequest`; the digest stays retained until the patch lands. | `cargo test -p spall_voxel --lib logical` (`drop_resident` clears only resident-brick digests, idempotent). `cargo test -p spall_server --test client_residency` (3): a replica that evicted east bricks reloads them from an ordinary server repair patch → digests dropped, `resident_brick_count` and `world_hash` match the server; a floor cut into a region the replica has fully evicted gaps → `NeedsRepair` → repair patches + retry → the reloaded bricks' digests are dropped (far untouched bricks stay evicted) and the replica converges; `wanted_reloads` names an evicted brick back in interest and nothing on a resident centre. Full `cargo fmt --all --check` + `cargo clippy … -D warnings` + `cargo test --workspace --all-features` green. |
| **E2 — client residency in the session + row 8b traversal** | Wire `ClientResidency` (evict + `wanted_reloads`) into `spall_client::net` behind `sandbox-client --residency-*`; a scripted `player_paths` traversal fixture that walks a client across the world and back with a low client budget, server residency on, save + cold restart. | Different server/client eviction subsets still converge live; travel away/back, save/restart; verify revisions, ownership, counts, replay — no regrowth. |

Paired on/off runs use the **same** scene, logical starting brick set, seed, and
authoritative transaction sequence (record committed IDs/order + any automatic
damage events; wall-clock timing alone does not guarantee equal input). Keep
some bricks evicted **during** edits, baseline capture, and the final hash
sample — a full reload just before assertions would hide the defect.

## Re-run during implementation

```sh
cargo test -p spall_voxel --lib logical
cargo test -p spall_sim --test logical_hash
cargo test -p spall_sim --test logical_commit
cargo test -p spall_sim --test logical_reload
cargo test -p spall_client --test logical_residency
cargo test -p spall_server --test logical_baseline
cargo test -p spall_server --test residency_pass
cargo test -p spall_server --test client_residency
cargo xtask scenario --name t23-g3-residency
cargo xtask scenario --name t23-g3
cargo xtask scenario --name t23-g3 --loss-percent 3
cargo xtask scenario --name t23-g3-impaired-join
cargo xtask crash-test --suite persistence
cargo test -p spall_store --test abrupt_crash
cargo xtask check
```

`--residency-budget-bricks` / `--residency-radius-bricks` and the paired
`t23-g3-residency` scenario landed in slice D. A dedicated traverse-away-and-back
fixture (row 8b) stays **proposed** until slice E. Record exact commands, raw
summaries, seeds, and measured results in `G3.md`.

## Not covered by this contract

Row 11 (join-duration budget) is the next independent T23 slice while B–E
proceed. Rows 12–14 (G4 eight-client workload + soak) are separate. Neither the
PR stack nor a passing CPU suite establishes the engine gate.
