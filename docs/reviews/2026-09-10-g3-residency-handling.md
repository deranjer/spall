# G3 residency design review and handling plan

Date: 2026-09-10. Scope: ENG-30 / T23, row 7 and its row 8b dependency.
Reviewed local commit `bcd309f` on `feat/eng-30-t23-inc5-residency-hash-design`,
the [proposed design](../reports/G3-residency-hash.md), the
[gate report](../reports/G3.md), repository contracts, and the current Loopira
ENG-30 description. This is a source review and implementation recommendation;
it does not certify PR CI, approve a merge, or record a passing gate.

## Recommendation

Accept the principle that cache residency must be invisible to logical topology.
Retaining exact brick digests is the right foundation. **Do not approve the
current design as sufficient for serve-loop wiring.** A reporting-only hash
fix leaves transaction validation, structural staging, baselines, and recovery
exposed to resident-only state.

Keep residency default-off. Revise the design to cover one logical brick view
used consistently by world hashes and affected-volume result hashes, plus
explicit geometry loading for consumers that need cells. Complete those
prerequisites before enabling eviction in `serve()`. ENG-30 remains in progress;
row 7 remains design/open and row 8b remains dependent on it.

## Findings that change the proposed plan

| Priority | Evidence in this checkout | Required handling |
| --- | --- | --- |
| Blocking | `spall_sim/src/world.rs::canonical_volume_for` and `spall_client/src/replica.rs::canonical_volume` enumerate resident bricks. | Preserve the canonical hash over the complete logical brick set despite different cache contents. |
| Blocking | `spall_sim/src/commit.rs` computes parent `result_hashes` with `volume_topology_hash_for`; replica transaction application hashes the entire candidate volume before committing. | Include untouched evicted bricks in candidate-volume hashing on both ends. Reloading just edited bricks is insufficient. |
| Blocking | `spall_server/src/serve.rs::capture_for` calls `baseline::capture_transfer`; `baseline::world_baseline` and `baseline_volume` snapshot live resident geometry. `brick_repair_patch` also requires a resident snapshot. | Replace or extend these capture paths with a coherent live-plus-backing view. The design's claim that live late-join baselines already come from durable records is incorrect. |
| Blocking | `spall_sim/src/stage.rs::stage_edit` builds its structural index with `ResidencyMode::AllResident`. The architecture explicitly says that mode treats absent neighbors as empty. | Integrate dependency-complete staging or streamed dependency resolution before edits run against evicted terrain. A standalone T18 test does not prove the live staging path safe. |
| High | `spall_voxel/src/residency.rs::update_interest` replaces each entry's interest using one center. | Compute the union of all players and relevant active-body trajectories; calling this method once per player can discard earlier players' interest. |
| High | `SimWorld::total_solid_cells` calls resident-only `solid_cells`. | Use logical solid counts for whole-world conservation; retain current counts at eviction and update them atomically with topology. A stable hash alone does not repair gate accounting. |
| High | `ResidencyBacking` requires durability, but `MemoryBrickBacking` retains payloads in a map. `enforce_budget` persists synchronously; `capture_checkpoint` collects backing records and reconstructs volumes. | Define a real bounded backing path, acknowledgement rules, and snapshot memory accounting. Moving payloads into another in-process map is not proof of a process memory reduction or crash durability. |

These findings are source-inspection results, not reproduced runtime failures.
The first two are directly implied by the hash inputs. The staging and capture
paths must be exercised under eviction to establish their corrected behavior.

## Contract to freeze before implementation

### One logical topology, independent of cache placement

For each live volume, define its logical brick set as resident bricks plus
explicitly retained evicted bricks. Every logical key `(VolumeId, BrickCoord)`
contributes exactly once. A missing cache entry is neither a deletion nor proof
of air. Retain modified-air bricks with their real revisions.

Use `spall_protocol::canonical_topology_hash` and its existing canonical records.
Preserve `spall.topology.v1`, volume IDs, cell-size codes, ownership, sequence
lengths, coordinate/revision encoding, and the material-layer kind and content
hash bytes. Volumes sort by ID and bricks by `(z, y, x)`. Merely hashing a list
of brick digests, concatenating resident and evicted lists, or changing the
domain would not preserve the current value. Motion remains outside this hash.

The compatibility assertion must be stronger than the proposed empty-map test:

```text
logical_hash(full_world, no_evictions) == existing_world_hash(full_world)
logical_hash(evict_subset(full_world), retained_digests)
    == existing_world_hash(full_world)
```

Require the equivalent identity for each affected volume, including staged
transaction candidates. World-hash comparison is meaningful only at the same
committed transaction frontier and for the same logical coverage.

### Ownership and crate boundaries

Keep the host convenience wrapper `residency_world_hash` in `spall_server` if
useful, but do not make it the only place where the contract exists. The
simulation commit path and client candidate validator also need it.

Recommended boundary: keep a pure brick-digest record and its lifecycle in
`spall_voxel`, alongside `Volume` or a narrowly scoped logical-volume view;
retain canonical encoding in `spall_protocol`. Simulation and client adapters
produce the same canonical records without importing `spall_server`. The server
controller owns I/O, cache policy, and durable acknowledgements. No sim-to-server
or client-to-server dependency is needed, and no network/store runtime enters
voxel algorithms.

Prefer a parallel digest record over overloading `GraphBrickMeta`: graph
summaries answer structural questions, while digest membership defines exact
topology coverage. Both must describe the same revision. The exact container
is an implementation decision; access must be available inside candidate
validation, not only in the serve summary.

### Digest lifecycle

| Transition | Required invariant |
| --- | --- |
| Evict | Capture revision, content hash, and conservation metadata from the current snapshot. On the server, require acknowledgement of that exact revision. Publish retained state and remove geometry together on the owning thread. Failure leaves geometry available. |
| Reload | Validate key, revision, and content against retained state before installing. Remove the evicted entry only after successful installation. Failed, unavailable, or mismatched loads preserve retained state and remain pending/error. |
| Edit or split | Obtain required geometry first. Candidate hashes combine changed resident state with unchanged evicted contributions. Publish geometry, ownership, and metadata atomically after validation. Failed/stale candidates change none of them. |
| Full baseline or recovery | Replace the logical state and digest namespace atomically; invalidate old-world/old-baseline entries. |
| Repair or bulk split patch | Reconcile only authoritative covered keys/volumes; preserve unrelated evicted state and clear superseded entries. Never count old and new ownership twice. |
| Volume retirement | Remove its digest and backing references under the same logical deletion contract; eviction alone never retires a volume. |

Scope retained state to the current world/session generation, since stable IDs
are unique within a world. Reject conflicting resident/evicted entries rather
than silently choosing one. A resident update legitimately supersedes an old
digest through an explicit atomic transition.

Handle `KnownEmpty` separately from an evicted edited-air record. T18 currently
materializes known-empty loads as revision-zero air bricks. Adding a previously
unrepresented air brick changes the current canonical brick count. Freeze a
logical-membership rule for these loads: either the same explicit empty records
exist in both comparison runs' starting state, or availability of known-empty
space is represented without adding a new canonical brick. Do not silently
drop modified-air records to make hashes match. This needs a dedicated test.

### Client scope and the no-new-wire-data claim

Locally retained digests need no new protocol fields when a client started
with the complete logical baseline and has applied every relevant transaction
through the comparison frontier. It can hash an untouched evicted brick.
It cannot reconstruct that brick's cells from its digest to apply a later edit.

For the first server wiring slice, keep client eviction off and test against
fully resident replicas. This provides an independent hash oracle and keeps
scope reviewable. It is an intermediate result, not completion of symmetric
client residency. Enable client eviction in a subsequent slice with bounded
local reload or the existing repair/baseline mechanism, pending-transaction
handling, and candidate digest validation. Existing geometry repair traffic
may still be necessary; “no digest messages” does not mean “no reload traffic.”

A future near-player-only baseline cannot produce a global hash for terrain it
never received. Keep full logical coverage for this G3 comparison. Regional
coverage needs an explicit scoped-hash contract before substituting it for a
global convergence assertion; it must not be smuggled into row 7.

## Safe serve-loop integration

An end-of-tick eviction call alone is insufficient. Establish these boundaries:

1. Before dispatching work or advancing collision consumers, compute required
   geometry from the union of player interests, swept body/player paths,
   pending edits, and structural dependencies. Reserve capacity and load it.
   Missing geometry defers the affected operation; it is never sampled as air.
2. Stage against immutable dependency-complete snapshots or a streamed path
   that resolves unknown support. Preserve revision/generation validation when
   committing. Retained graph metadata cannot establish support on its own.
3. Apply validated transactions at the tick boundary, updating logical hashes,
   counts, owners, and dirty state together. Include contact/strength-generated
   edits and bulk-split paths in the audit, not only player edits.
4. Refresh cache accounting and union interest. Track scoped pins for active
   consumers and release them on completion, rejection, cancellation, or
   disconnect. Immutable snapshots may outlive cache eviction, but their
   retained bytes still count; pin the geometry that later validation requires.
5. Submit bounded persistence work and process exact-revision acknowledgements.
   Evict only eligible clean state. If the working set cannot fit, report
   capacity pressure and defer safely rather than violating readiness or
   silently claiming the budget passed.
6. Capture baselines/checkpoints at a single tick and journal cursor using
   immutable live snapshots plus exact-revision backing records. Continue
   bounded catch-up after that cursor. Capture must not reload the whole live
   cache just to serialize it.

Audit the normal and shutdown checkpoint call sites in `serve.rs`, baseline
requests and catch-up recaptures, one-brick repairs, and bulk split capture.
The existing `capture_checkpoint` merge is useful groundwork, but it does not
by itself establish asynchronous capture or bounded staging memory. Latest
durable state is also not necessarily current live state: dirty live revisions
must override older backing records, and asynchronous reads need an immutable
version view so later writes cannot contaminate a snapshot.

`--replay` can retain its existing CLI and fully resident reconstruction.
Its equality remains conditional on complete checkpoints and residency-neutral
transaction result hashes. Re-run replay; do not declare it unaffected solely
because no replay code was edited.

## Memory and observability

Define whether the new brick limit covers terrain only or all cached geometry:
the current controller registers and pins dynamic-body bricks too. Report
terrain and body counts separately, plus total cache bytes. Pinning can prevent
`enforce_budget` from reaching the requested ceiling; its eviction count alone
does not prove success.

Keep the proposed eviction/reload totals and resident terrain min/max/final.
Also record evicted-brick occupancy over time, longest sustained eviction,
blocked-load/pin counts, budget misses, digest metadata bytes, backing memory,
retained job/baseline/checkpoint bytes, hash duration, and process peak memory.
Define each sampling boundary, including bootstrap and transient allocations.
Do not exclude startup from a “maximum” without labeling a separate steady
state metric. Version summary output according to existing conventions and
update harness readers together.

The digest fold covers the logical extent, not just visible bricks. Current
canonical encoding sorts records and allocates canonical vectors, so do not
promise a cheap strictly linear tick cost or assume only a few hundred evicted
entries. Measure hash p95/max and bytes retained on the named workload before
adding incremental caching. Dense voxel bytes alone are not the G3/G4 process
memory gate.

## Implementation increments and acceptance

| Slice | Deliverable | Required evidence before proceeding |
| --- | --- | --- |
| A: contract and digest foundation | Amend the design with the above decisions; implement lifecycle and shared canonical inputs. | Existing-hash equality with no eviction; arbitrary subsets and eviction orders; negative coordinates; multiple bodies; modified air; known-empty loads; failed reload/persist; stale and duplicate metadata. |
| B: transaction and structural correctness | Candidate result hashes and conservation see logical state; staging resolves dependencies; revisions remain authoritative. | Evict untouched brick B, edit resident A in the same volume, and validate/replay the transaction on a full replica. Repeat with independent client eviction. Cut a remote support chain and verify exact detach/conservation; reject stale jobs without partial mutation. |
| C: backing and capture | Bounded backing integration plus complete late-join, repair, checkpoint, and shutdown capture. | Join/reconnect while terrain remains evicted; repair an evicted brick; checkpoint with newer dirty live revisions; cold restart and crash-prefix recovery; no restored removed cells or retired volumes. |
| D: default-off server wiring | Config/CLI, load/read/evict boundaries, summary/harness fields; clients initially fully resident. | Sustained eviction under a declared achievable limit, all mandatory edits complete, on/off agreed hashes equal, exact replay and late join match. State every memory-budget miss. |
| E: client residency and traversal | Client digest lifecycle, bounded reload/repair, then row 8b traversal before save. | Different server/client eviction subsets still converge; edit while replica terrain is evicted; travel away/back, save/restart, verify revisions, ownership, counts, and replay without regrowth. |

For paired on/off runs, use the same scene, logical starting brick set, seed,
and authoritative transaction sequence. Record committed IDs/order and any
automatic damage events; wall-clock timing alone does not ensure equal input.
Keep some bricks evicted during edits, baseline capture, and final hash
sampling. Full reload just before assertions would conceal the defect.

Existing commands to re-run during implementation (not run for this review):

```sh
cargo test -p spall_server --test residency
cargo test -p spall_client --test residency
cargo xtask scenario --name t23-g3
cargo xtask scenario --name t23-g3 --loss-percent 3
cargo xtask scenario --name t23-g3-impaired-join
cargo xtask crash-test --suite persistence
cargo test -p spall_store --test abrupt_crash
cargo xtask check
```

Add focused tests for slices A–C and a new paired traversal scenario in slices
D–E. Its command and `--residency-budget-bricks` remain proposed until the CLI
and fixture exist. Record exact commands, raw summaries, seeds, and measured
results in G3.md. GPU and hardware performance evidence remain separate.

## Handling the PR stack and other open rows

The reported #84 → #85 → #86 → #87 → #88 sequence is also recorded in ENG-30.
This review inspected the local stack tip, not remote PR status or CI artifacts.
Integrate code increments in dependency order with review and required checks;
do not treat their reported passing results as re-measured here. Amend #88's
design and synchronize ENG-30, tasks, and validation when the contract decision
is accepted. Preserve the distinction between design approval and gate passage.

Row 11 is the next independent **T23** slice while the row-7 contract is under
review. Implement a bounded byte-rate shaper in the UDP proxy, specify direction,
burst allowance, queue bounds, and whether the 1 MiB/s limit counts wire or
application bytes. Impose 100 ms total RTT and 2% packet loss, then measure
compressed baseline bytes and wall-clock time through installation and catch-up
to ready. Require both <=16 MiB and <=30 s; report QUIC overhead/retransmission
separately. Do not relabel the current full-world transfer as a regional baseline
or count a bounded join failure as a successful normal-join budget result.

Row 10's delayed-connect-after-shutdown fixture proves explicit failure handling,
but does not exercise retry/catch-up exhaustion while connected clients remain
active. Preserve its narrower evidence and add that live-overload scenario
before claiming the entire retry/catch-up requirement is covered. Rows 12–14
remain separate G4 workload and measurement work; neither five stacked PRs nor
a passing CPU suite establishes the complete engine gate.

## Review-session evidence

Changed file: this document only. Existing untracked `.claude/` and other
`docs/reviews/` files were preserved. Checks: source inspection using `rg` and
PowerShell `Get-Content`; `git status --short`, `git log -1 --format='%h %s'`,
and `git branch --show-current`; read project guide and ENG-30 via Loopira.
No build, runtime test, GPU capture, or performance measurement was run.
All acceptance results above are required future evidence, not measured passes.

Remaining risks: logical membership of known-empty bricks, candidate digest
ownership, structural/collision readiness, durable snapshot consistency, and
memory under pinned working sets. Next unblocked task ID: **T23 / ENG-30,
row 11**; row 7 starts with the contract corrections in slice A. No gate or
ticket is marked done by this review.
