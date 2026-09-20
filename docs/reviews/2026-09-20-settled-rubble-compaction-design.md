# Settled-rubble compaction — design for review (not implemented)

Status: **design + measurements only.** Nothing here is authorised for
implementation; it is written so the wake root cause
([G3 increment 38, finding 8](../reports/G3.md)) and this design can be reviewed
together before a dedicated lifecycle change is scoped. It does not change
`docs/protocol.md`, any fixed DTO, or any shipped behaviour.

## 1. Problem, restated with the evidence

In the G4 workload every edit that detaches matter creates one body (measured:
8,866 commits → +8,865 bodies). Bodies are never merged, retired, or shrunk:

| Cost | Per rubble body (a 6-cell pole tip) | Where measured |
| --- | --- | --- |
| Server memory | one **dense 32,768-cell brick = 64 KiB** (`DENSE_LAYER_BYTES`) for 6 solid cells | `spall_voxel::brick` |
| Solver / motion / pose-journal cost while awake | `step_physics` walks it; a `PoseBatch` record carries it every 20 Hz batch | stage timers; 28,851 journal records in the first soak |
| Durable size | a checkpoint record per body; `world.db` 265 MB at ~5.4 k bodies after 2 minutes, 5.7-8.9 GB after 44 min | run summaries |
| Replicas | every client holds the same bodies | client `body_count` |

The wake root cause explains why the *awake* set grows (piles under a continuous
edit stream never finish sleeping); it does not remove the one-body-per-edit growth.
Compaction targets the growth, but the tiers below have very different risk, and
only the first is lossless *without* a new authoritative operation.

## 2. Hard constraints (from review direction)

1. **Lossless**: exact voxels and materials; future edits and collisions behave as
   they would have on the uncompacted bodies.
2. Server-authoritative, deterministic, journaled, replicated, crash-recoverable.
3. Clients transition **atomically**: no stale entity/volume references, no
   duplicated solids, no ownership gap at any observable instant.
4. **Must not compact the 4,096 sleeping persistent workload bodies**, or any
   other body, merely to improve a metric.
5. Terrain-edit cost (whole-volume reindex/hash/replan) is a *separate* open issue
   and is not addressed here.
6. Existing checks stay: conservation, exact replay, cold restart, late join.

## 3. Tier A — sparse brick storage (recommended first; internal, lossless)

**Idea.** A `Brick` today is `Uniform(m)` or `Dense(65,536 B)`. Add a third layer
form, `Sparse(sorted [(cell_index: u16, material: u16)] + background material)`,
chosen by the same `collapse()` that already turns an all-equal dense layer back
into `Uniform`, when the non-background cell count is below a threshold
(e.g. `< 2,048`, i.e. `< 8 KiB`).

Why this is lossless and narrow:

- `Brick::get`, `snapshot`, `content_hash`, `set_cell`, iteration and revision
  semantics are defined on *logical cell content*; the canonical hash and every wire
  encoding already operate on logical content (the wire already compresses a
  6-cell brick to tens of bytes), so **no DTO, hash, journal record, or replica
  behaviour changes**. A replica may use a different in-memory form than the server.
- No ownership, identity, frame, or collider change: the body keeps its entity id,
  volume id, pose, collider, and edit history. Entities, references, and the
  4,096 workload bodies are untouched (their bricks may *store* sparsely, which is
  invisible to gameplay and to every hash).
- Crash recovery and checkpoints: persisted bytes are already the logical layer
  (`Brick::restored(cells, ..)` collapses on load), so old checkpoints load
  unchanged and new ones are byte-identical.
- Determinism: representation is a pure function of content (canonical sort +
  threshold), so two servers agree.

Expected effect (to be *measured* by a prototype, not assumed): in-memory bytes per
rubble body 64 KiB → ~100 B; server and client memory for 13 k rubble bodies
~850 MB → ~1 MB. It does **not** reduce body count, solver cost, awake-set size, or
pose-journal records; it removes the memory and (indirectly) baseline/checkpoint
build cost of *holding* rubble. It is the only tier that needs no new authoritative
operation.

Prototype scope (small, reviewable): `Layer::Sparse` behind the existing `Brick`
API, a property test that random edit sequences give identical `content_hash`,
`get`, `snapshot` and `EditOutcome` for dense-only vs sparse-capable bricks, and a
byte-count report on `g4_comb_body` tip volumes. No server, protocol or client
change.

## 4. Tier B — dormant pile consolidation (object-level; needs new authoritative ops)

This is the tier that reduces **body count**. It is only lossless under strict
eligibility, and it needs a versioned protocol operation. It is not proposed for
implementation until Tier A and the wake fix are reviewed.

### 4.1 Eligibility (all must hold, evaluated by the server, deterministically)

- **Provenance = rubble**: the body was created by a structural split (it has a
  recorded `parent` at creation). Bodies created at scene construction —
  including all 4,096 sleeping workload bodies, the giant, combs, towers, and
  debris — carry `origin = Scene` and are **never eligible**. A per-body
  `origin` bit is authoritative state, journaled and checkpointed.
- **Dormant** (already deactivated by the dormancy policy: rapier-asleep, still,
  no active region within margin) for at least `N` ticks (config, default 30 s).
- **Frame-compatible**: an exact common frame exists, i.e. the members' rotations
  are equal (bit-equal quaternions, or all identity) and their translations differ
  by whole numbers of cells. Anything else (a tumbled tip resting at 17 degrees)
  is ineligible: merging it would require resampling voxels, which is lossy.
  (A deterministic "settle-snap" that rounds a resting pose to the cell grid would
  widen eligibility but changes collision by up to half a cell and is a *separate*
  authoritative decision requiring its own review; it is not part of this design.)
- **Contact-closed**: no awake body, player, or pending edit within the dormancy
  margin (already the dormancy criterion).
- Spatially adjacent (touching or within 1 cell) so the merged volume has no
  large empty bounding box.

### 4.2 The operation

A new versioned topology op `MergeBodies { target, sources[], frame }` inside one
ordinary `TopologyTransaction` (so it inherits ordering, journaling, replay,
`before/after` revision preconditions, and result hashes — no new channel):

- `target` keeps its entity id and volume id; each source's cells are written into
  `target`'s volume at `frame`-translated coordinates (`CellRun` ops, exactly the
  representation splits already use); each source body is retired.
- `result_hashes` cover the target volume (and prove the sources' cells appear
  exactly once); `before` names every source's brick revisions, so a stale or
  concurrent edit to any source makes the transaction fail its precondition and
  nothing merges (edit wins; merge retries later).
- Conservation check: solid-cell count and per-material counts before = after
  (the existing ledger, applied to the union).
- Physics: target collider rebuilt once (dormant target: no solver cost); future
  edits on merged matter target the target body at the new local coordinates; the
  structure analysis on a later edit sees ordinary connectivity (a pile that was
  several disconnected components stays as several components inside one volume
  and splits back out as they are edited — identical result to editing the
  separate bodies).

### 4.3 Atomic client transition (no stale references, no duplicates, no gap)

The merge is one `TopologyTransaction`; a replica applies it under its existing
all-or-nothing rule (the transaction is staged against `before`, applied, and
verified against `result_hashes` before publish; on mismatch the replica requests a
repair as it does today). Consequences the design relies on:

- there is no instant at which a source's cells exist both in the source and the
  target (single publish) or in neither (cells are written before the source is
  retired within the same staged application);
- entity references: motion snapshots and action requests naming a retired source
  are rejected by the server as `unknown entity` (already the behaviour after a
  body is retired), and the retrier does not resend non-`throttled` rejections; a
  client-side selection/aim on a retired entity must re-resolve — the same rule
  that already applies when an edit empties a body (`ENG-56`);
- late join: baselines are captured from the logical world after the merge, so a
  joiner never sees sources; a joiner mid-catch-up receives the merge in its
  queued transaction stream in order.

### 4.4 Durability and recovery

- The merge is journaled as an ordinary topology record; replay reproduces it
  bit-for-bit (the op is deterministic given the journal).
- Crash points (existing matrix): before journal append → nothing merged, sources
  intact; after append, before checkpoint → replay re-merges; after checkpoint →
  recovered merged. Torn/partial writes are covered by the existing CRC and
  interior-gap detection. A **new crash-injection scenario** (kill between
  journal append and checkpoint of a merge) is a required test.
- Memory/disk growth is bounded by *pile* count, not edit count, for eligible
  frame-aligned rubble.

### 4.5 Why not compact "everything that settled"

Rubble tumbles to arbitrary rotations; exact merging of those needs resampling
(lossy) or a change to how bodies store orientation. So Tier B alone would compact
only bodies that happen to be frame-compatible, which in the G4 fixture is a
minority (tips fall from stationary poles and mostly land upright, so a settle-
snap review may well be the deciding question). Tier B's payoff must be measured
against Tier A + the wake fix before it is scoped.

## 5. Test plan (required before any implementation lands)

- Property tests: exact voxel/material multiset and canonical hash equality
  before/after; conservation ledger; edit-equivalence (apply the same random edit
  sequence to merged vs unmerged worlds and compare resulting solid sets and
  body partitions).
- Replay: merge in the journal replays to the same hash. Restart and late-join
  scenarios with merges in flight. Crash-injection at every merge boundary.
- Multi-client atomicity: eight replicas, merge under 100 ms/2% and 200 ms/5%;
  assert no client ever hashes to a state with duplicated or missing cells
  (sample hashes around the transaction).
- Guard tests: the 4,096 workload sleepers, the giant, combs, towers and the
  256 active bodies are never chosen; a merge is refused if any source has
  `origin = Scene`.
- Non-regression: terrain edits, dormancy wake rules, motion suppression.

## 6. Recommendation and open questions for review

1. Land the **wake fix first** (a commit wakes only bodies whose support may have
   changed — see `wake_reasons.rs`); measure how much of the awake growth remains.
2. Prototype **Tier A** (sparse bricks) — it is invisible to protocol and clients.
3. Decide whether Tier B is needed after (1)-(2), and whether a settle-snap is
   acceptable; if so, review it as its own authoritative operation.
4. Open: should `origin` be tracked per body or per volume; what is the maximum
   merged-volume bounding box (grid-size limit `8,388,608` cells applies); and how
   should a merge interact with `--residency` (evicted bricks).
