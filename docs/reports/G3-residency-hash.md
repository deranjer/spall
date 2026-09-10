# G3 row 7 — residency-aware world hash (design)

**Status:** design, awaiting integrator sign-off. No code in this increment.
Blocks the serve-loop residency wiring (G3 open row 7) and, through it, row 8b
(traverse away/back before the save).

## The blocker

`docs/validation.md` G3 requires *"a 256 x 128 x 256 m bounded world with
resident cache limits low enough to force eviction"*, driven while clients join,
reconnect, and traverse.

T18 (ENG-25) built the whole residency subsystem — `spall_server::residency`
(`ResidencyController`, `ResidencyCache`, `MemoryBrickBacking`), the
LRU/hysteresis/collision policy in `spall_voxel`, `spall_client::ClientResidency`
— and its acceptance fixtures (`spall_server/tests/residency.rs`,
`spall_client/tests/residency.rs`). It deliberately did **not** wire any of it
into `spall_server::serve`. The reason:

- `SimWorld::world_hash()` (`crates/spall_sim/src/world.rs`) folds
  `canonical_volume_for(volume, owner)` over **`volume.resident_brick_coords()`
  only** — each resident brick contributes `(coord, revision, content_hash)`.
- `ReplicaWorld::world_hash()` (`crates/spall_client/src/replica.rs`) does the
  same over the replica's resident bricks.
- The harness (`tools/xtask/src/session.rs`) and the server/clients converge by
  comparing those values; `sandbox-server --replay` reproduces it from the
  checkpoint + journal with everything resident.

So **evicting one clean terrain brick mid-run changes the reported hash on that
side**, and convergence, the exact-replay check, and every gate assertion break.
`Volume::evict_brick`'s own doc says as much: *"absence is not air"*.

The residency subsystem already solved this **for checkpoints only**:
`ResidencyController::capture_checkpoint` rebuilds full volumes from live +
durably-evicted bricks and `checkpoint_hash` recomputes a stable
`canonical_topology_hash` over them. There is no equivalent for the value
reported every tick / at handshake / at end of run.

## What eviction must stay invisible to

1. the world hash the server reports (`ServeSummary.final_world_hash`) and any
   per-tick / handshake hash clients converge on;
2. per-transaction `result_hashes` (already per-affected-volume — see below);
3. `sandbox-server --replay` (reconstructs fully resident — must still match);
4. checkpoint recovery — already handled by `capture_checkpoint`.

And eviction must actually **hold memory down** between checkpoints (a
persist→evict→immediate-reload that never keeps anything evicted is not
eviction), with **no mined-terrain regrowth** and exact solid-cell
conservation.

## The missing piece

`GraphBrickMeta` — the metadata `ResidencyController` retains when it evicts a
brick — carries `{ revision, solid_cells, occupied_faces, touches_anchor }`. It
**does not carry the brick's canonical content hash**, so the controller cannot
reconstruct `world_hash()` without reloading the brick.

Everything else needed is already present:

- `enforce_budget` persists dirty eviction candidates first, then drops **only
  clean, durably-acknowledged** bricks (`ResidencyCache::plan_evictions` —
  *"drops only clean acknowledged bricks"*). The retained digest would therefore
  always be the durable revision's content, i.e. exactly what a reload
  contributes.
- `load_brick` reloads on demand; a committed edit that touches an evicted brick
  must reload-then-edit, so a mined-to-air brick comes back as modified-air, not
  regenerated.
- Bodies are pinned complete and never evicted (`register_volume(..,
  pin_complete = true)`).

## Proposed design

### 1. Retain the content digest on eviction

Add to the retained eviction metadata (extend `GraphBrickMeta`, or a parallel
`BTreeMap<BrickCacheKey, EvictedDigest>` where
`EvictedDigest { revision: Revision, content_hash: [u8; 32] }`). Populate it in
`enforce_budget` from `snapshot_brick(coord)` immediately before
`body.volume.evict_brick(coord)`. Clear the entry when `load_brick` brings the
brick back or a commit reloads-then-overwrites it.

`content_hash` is `spall_voxel::BrickHash::to_bytes(snap.content_hash())` — the
same 32 bytes `canonical_volume_for` already uses.

### 2. A residency-aware hash, computed identically on both ends

New free function in `spall_server` (dependency direction forbids putting it in
`spall_sim`):

```
fn residency_world_hash(world: &SimWorld, evicted: &EvictedDigests) -> Hash32
```

For the terrain volume it folds, in canonical `(z, y, x)` order:

- every **resident** brick as today — `(coord, revision, content_hash)` from the
  live snapshot; plus
- every **evicted** terrain brick as `(coord, revision, content_hash)` from the
  retained digest.

Bodies are unchanged (always fully resident). The result is **bit-identical to
`SimWorld::world_hash()` whenever nothing is evicted** — that is the correctness
anchor and the regression guard.

The client computes the same value from `ReplicaWorld` resident bricks ∪ its own
`ClientResidency` evicted digests. **Neither side sends digests over the wire** —
a client that applied every transaction up to tick *T* and then evicts a clean
brick knows that brick's `(revision, content_hash)` exactly, the same way the
server does. Eviction stays a local memory decision on each side.

### 3. Where `serve` uses it

Only when residency is enabled (`ServeConfig.residency: Option<ResidencyLimits>`,
default `None` — zero behaviour change for every existing scenario):

- report `residency_world_hash(...)` in `ServeSummary.final_world_hash` and
  wherever a live hash is exposed for convergence;
- `sandbox-server --replay` reconstructs fully resident, so its plain
  `world_hash()` already equals `residency_world_hash` with an empty digest set
  — no replay-path change.

### 4. Serve-loop residency pass (the follow-up increment, sketched here)

Per tick, after the sim tick + commits, before the next tick's reads:

1. build the interest set from player-capsule brick positions (`InterestRadii`,
   near/far from `ResidencyLimits`);
2. `cache.update_interest(...)` for terrain bricks;
3. `enforce_budget(world)` — persist dirty, evict clean beyond budget, record
   digests;
4. `load_brick` for any brick back in interest but evicted, before meshing /
   structure / physics / replication read it. Structural analysis already
   *"loads dependencies past visibility"* (T18
   `structural_dependencies_load_past_visibility_before_releasing_a_component`).

Checkpoints go through `capture_checkpoint` + `persist_volume`.

New `ServeSummary` fields: `residency_evictions_total`,
`residency_reloads_total`, `resident_terrain_bricks_{min,max,final}`,
`residency_peak_dense_bytes`.

### 5. Acceptance for the wiring increment

A bounded-world traversal fixture (extend `t23-g3` or a new
`t23-g3-residency.json`) run with `--residency-budget-bricks N`:

- `residency_evictions_total > 0` and `resident_terrain_bricks_max <= N` —
  eviction genuinely fired and memory stayed capped;
- the residency-**on** run converges to the **same** agreed hash as the
  residency-**off** run, and `sandbox-server --replay` matches it — eviction is
  invisible;
- solid-cell conservation holds across the whole run — no regrowth;
- a `--late-join` client and a reconnecting client still converge (they pull a
  full baseline, so they are fully resident and their plain `world_hash()`
  already matches).

## Risks / open questions for the reviewer

- **Interest reach vs structural reach.** A support chain can cross more bricks
  than a player's interest radius. T18's dependency-load path covers the
  *analysis*, but the residency pass must not evict a brick an in-flight
  structural job still needs. Simplest: pin the union of (interest set) ∪ (bricks
  named by any pending structural/collision job) each tick.
- **Per-tick hash cost.** `residency_world_hash` folds `resident + evicted`
  digests — O(bricks in the bounded world's *populated* extent), same order as
  `world_hash()` today, plus the (≤ a few hundred) evicted entries. Cheap, but
  measure it in the wiring increment.
- **Client eviction scope.** G3's client residency (T18) evicts replica terrain
  but keeps complete body geometry. The residency-aware hash handles that
  symmetrically; confirm the reconnect/repair path (`ReplicaWorld::evict_brick`
  → *"bounded repair/baseline path"*) still reloads a digest-tracked brick
  rather than requesting a repair it does not need.
- **Motion / replication baseline.** A mid-run baseline (a late joiner) is built
  from durable records, not the live cache, so it is unaffected. Confirm the
  incremental `result_hashes` a commit publishes are over the affected volume's
  *durable* extent (they are per-volume `volume_topology_hash_for`, resident at
  commit time because the edit reloaded its bricks) — so no change needed, but
  state it explicitly in the wiring increment's report.

## Reproduce (what exists today)

```sh
cargo test -p spall_server --test residency      # the T18 unit-level policy
cargo test -p spall_client --test residency
```

`serve()` runs everything resident; there is no `--residency-*` flag yet.
