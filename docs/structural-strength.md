# Material-dependent structural strength (T22)

Status: **frozen model proposal for the G4 gate**, awaiting integrator sign-off.
`docs/architecture.md` ("Material strength stage") requires an experienced
reviewer to freeze the equations and units before an implementation agent writes
them. This document is that freeze request. Constant values are provisional
until the G4 integration (T23) measures the fixtures.

`spall_structure::strength` implements exactly what is written here and nothing
else. It is a **gameplay approximation**: no finite-element method, no stress
tensor, no continuous deflection, no dynamic/impact load (impact damage is T21).
All-resident G1 residency only; streamed cross-brick support is T18.

## 1. Scope and layering

Connectivity-only collapse (a component with no path to the anchor plane
detaches) is already delivered by T07 (`SupportGraph` / `SupportReport`). T22
adds a second, stricter test **on top of** connectivity: a component that is
still connected to the ground can nonetheless fail because a bond somewhere
along its load path carries more than its material can hold.

The strength pass consumes the same `Volume` + `AnchorPlane` as the support
pass, plus:

- the world `MaterialManifest` (for `density_kg_m3` and `bond_strength`), and
- the volume's authoritative `DamageState` (the set of already-broken bonds,
  persisted — see §6).

It produces a `StrengthReport`: the updated `DamageState`, an ordered list of
bond failures, and the set of components that detached as a consequence (handed
to the unchanged T07/T08 split path).

## 2. Fixed-integer inputs

The model runs entirely on `u64`/`u128` integers. Two per-material tables are
derived **once**, with a single `f64` rounding step each, and are the fixed
integer inputs to everything downstream (`docs/architecture.md`: "Use fixed
integer inputs, stable traversal/tie-breaking").

Let `s` be the volume cell edge in metres (`CellSizeCode::metres`) and
`v_cell = s³` the cell volume in m³.

| Quantity | Definition | Unit | Stone example (`ρ=2600`, `bond_strength=12`) |
| --- | --- | --- | --- |
| `cell_weight(m)` | `round(ρ(m) · v_cell · 1000)` | grams | `round(2600 · 0.015625 · 1000)` = `40 625` |
| `axial_cap(m)` | `round(bond_strength(m) · BOND_SCALE)` | grams | `round(12 · 100 000)` = `1 200 000` |

Frozen constants (`strength_algo_version = 1`):

| Constant | Value | Meaning |
| --- | --- | --- |
| `BOND_SCALE` | `100_000` | grams of axial bond capacity per unit of `bond_strength` |
| `LATERAL_NUM / LATERAL_DEN` | `1 / 2` | a bending (lateral) joint holds half of `axial_cap` |
| `WEIGHT_GRAMS_SATURATES_AT` | `u64::MAX` | accumulated load is saturating; a saturated load is itself a definite overload |

A material without `MaterialFlags::STRUCTURAL`, or absent from the manifest,
contributes `cell_weight` as dead load but has `axial_cap = 0` (any bond it
governs fails immediately). Air is never solid and never participates.

Changing `strength_algo_version` or any constant is an **authoritative content
revision**, not a hot-reload (`docs/architecture.md`: "structural properties
change through an explicit authoritative content revision"). The version is
recorded in world metadata (§6); the constants themselves are compiled, not
saved.

## 3. Clusters and bonds

- A **bond** is an unordered pair of face-adjacent solid cells `{a, b}`. It is
  stored and compared in canonical form with `key(a) < key(b)` where
  `key(c) = (c.z, c.y, c.x)` — the same `(z, y, x)` canonical order used
  everywhere else in `spall_structure`.
- A bond is **vertical** if `a.y != b.y` (a stacked joint, pure axial
  load) and **lateral** otherwise (`a.x != b.x` or `a.z != b.z`: a
  cantilever/bending joint).
- A **cluster** is a maximal set of face-connected solid cells of the **same
  `MaterialId`**, connectivity not crossing a broken bond, within one T07 global
  component. A homogeneous beam is one cluster; a two-material joint is two
  clusters linked by inter-cluster bonds. Clusters are reported for inspection
  and give bonds a stable "which side is weaker" answer, but the failure test
  itself is per-bond (§5), so cluster identity never changes an outcome.

Only bonds whose **both** cells are solid, structural-or-not, and not already in
`DamageState` are "live". A bond in `DamageState` is treated as absent for
connectivity and load.

## 4. Support field (deterministic propagation)

Over the graph of live bonds:

1. **Anchor set** `A` — every solid cell with `y == AnchorPlane::y`. (Support
   inherited from an adjacent already-supported *other* volume is T18; here the
   plane is the only ground.)
2. **Support distance** `d(c)` — breadth-first distance in bonds from `A`.
   The BFS seeds `A` in canonical `key` order and visits face neighbours in the
   fixed order `-x, +x, -y, +y, -z, +z`. `d(a ∈ A) = 0`. A cell with no path
   has `d = ∞`: it is already `Unsupported` by pure T07 connectivity, is
   excluded from the strength test, and goes straight to the split path.
3. **Support forest** — every non-anchor supported cell's **parent** is its
   live face neighbour with the smallest `(d, key)`. This is a spanning forest
   rooted at `A`; each non-anchor cell has exactly one parent bond, the bond
   that carries its accumulated load.
4. **Accumulated load** `L(c)` — post-order over the forest:
   `L(c) = cell_weight(mat(c)) + Σ_{child} L(child)`, saturating at `u64::MAX`.
5. **Moment arm** `h(c)` — lateral bonds along the support path:
   `h(a ∈ A) = 0`; `h(child) = h(parent) + [parent→child bond is lateral]`.
   `h` is in cells and is the cantilever arm length from the nearest supporting
   column.

All five steps are pure integer / ordering operations with canonical
tie-breaking, so the field is identical on every machine and every run.

## 5. Per-bond demand, capacity, and the failure test

For the parent bond of a non-anchor supported cell `c` (parent `p`):

```
demand(c)   = L(c)                       if the c–p bond is vertical
demand(c)   = L(c) · (1 + h(c))          if the c–p bond is lateral

base(c)     = min(axial_cap(mat(c)), axial_cap(mat(p)))   // weaker side governs
capacity(c) = base(c)                                     if vertical
capacity(c) = base(c) · LATERAL_NUM / LATERAL_DEN         if lateral   // = base/2

overloaded(c)  ⟺  demand(c) > capacity(c)
```

`demand` is computed in `u128` (`L` is up to `u64::MAX`, `1 + h` up to the cell
count) and compared without division. Ordering two overloaded bonds by
"how overloaded" also avoids division: bond `x` is worse than bond `y` iff
`demand(x) · capacity(y) > demand(y) · capacity(x)` in `u128`, ties broken by
canonical bond `key`.

## 6. Failure order (deterministic cascade)

```
iters = 0
loop:
    rebuild the §4 support field over the current live bonds
    candidates = { parent bonds c with overloaded(c) }
    if candidates is empty:            stable = true;  break
    if iters == MAX_ITERS:             stable = false; break
    worst = max(candidates) by (overload ratio, then bond key)      // §5
    break `worst`: add it to DamageState, append BondFailure{ bond, demand,
                   capacity, iter: iters }
    iters += 1
```

One bond breaks per iteration so load **redistributes** before the next test — a
redundant second path can save a bond that looked overloaded in isolation, and
the failure list is a fully ordered, replayable cascade rather than a set.
`MAX_ITERS = 2 · live_bond_count + 64`, a bound fixed by geometry; reaching it
reports `stable = false` (an oscillating or pathological structure, surfaced to
the integrator, never silently trimmed).

After the loop, any component that now has no anchored cell is added to
`detached` and handed to the existing split path; its share of `DamageState`
travels with the child body (§7).

## 7. Save and replication fields

Wiring these into `spall_protocol` / `spall_store` DTOs is **increment 2** of
this task; increment 1 (this change) defines the shapes and the canonical
encoding used for hashing.

- **World metadata** (`docs/protocol.md`: "structural algorithm versions") gains
  `strength_algo_version: u16` (this document = `1`). A mismatch on load fails
  the handshake with an actionable error, exactly like a material-manifest
  mismatch — a saved `DamageState` is only meaningful under the algorithm that
  produced it.
- **Per-volume authoritative layer** `DamageState` — a canonically sorted
  `Vec<BrokenBond>`, where a `BrokenBond` holds two `GlobalCell` endpoints with
  `key(a) <= key(b)`. The endpoints are private and `BrokenBond::new` is the only
  constructor, so a non-canonical bond cannot be built and every hash / dedupe /
  lookup sees one representative per face. `DamageState::canonical_bytes` is the little-endian
  `i64`-sextuple-per-bond encoding that feeds BLAKE3. This layer participates in
  revision / hash / checkpoint like every other authoritative layer
  (`docs/architecture.md`: "All authoritative layers participate in
  revision/hash/checkpoint rules"), so a replicated world hash covers structural
  damage.
- **Body records** (`docs/protocol.md`: "damage/bond state") — a detached child
  carries the subset of `BrokenBond`s with both cells inside its membership,
  re-based to the child's local cell frame when the body is created.
- **Recovery** loads `DamageState` and starts the §6 loop with those bonds
  already broken, so a beam caught mid-failure at a checkpoint stays failed:
  "restart cannot heal a failing beam".

## 8. Acceptance fixtures

Small enough for CPU CI (`docs/validation.md`, `cantilever-strength` row).
Implemented in `crates/spall_structure/src/strength.rs` tests and mirrored as a
`spall_structure` scenario test.

| Fixture | Assertion |
| --- | --- |
| `weak_cantilever_fails` | A long horizontal beam of a low-`bond_strength` material, one end embedded in an anchored column, sheds its outer span: `failures` is non-empty, `detached` names the freed component, `stable == true`. |
| `strong_cantilever_holds` | The identical geometry in a high-`bond_strength` material produces zero failures and zero detachments. This is the "material capacity changes the failure outcome" pair. |
| `short_stub_holds` | A 2–3 cell stub of the weak material off the same column holds — the model is span/load dependent, not "all cantilevers of weak material fail". |
| `undermined_ground_cascades` | Deleting anchor cells under a wide slab makes the newly unsupported edge overload progressively; `failures` are ordered outer-to-inner and `detached` grows monotonically as support is removed. |
| `damaged_joint_changes_outcome` | Seeding `DamageState` with one bond at the beam root turns a configuration that otherwise holds into one that fails — pre-existing damage is respected. |
| `deterministic_failure_order` | Two evaluations of the same inputs (and a shuffled brick/cell iteration order) yield byte-identical `failures` and `DamageState`. |
| `restart_preserves_damage` | Feeding a report's `DamageState` back in as input yields no new failures and an unchanged `DamageState` (idempotent; recovery cannot heal). |

## 9. Non-goals

- No finite-element simulation, stress field, or deflection geometry.
- No impact / contact / explosion loads (T21).
- No cross-volume or streamed-residency support inheritance (T18).
- No claim of physically exact engineering behaviour — the numbers are tuned for
  legible gameplay, not certification.
