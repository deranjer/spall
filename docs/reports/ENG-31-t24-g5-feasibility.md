# ENG-31 / T24 — G5 larger-world feasibility scoping

Status: **feasibility scoping only. Not an implementation. No acceptance
bullet of T24 is claimed met.** T24's own dependency (T23, ENG-30) is not
done — `docs/tasks.md` lines 292-296 records T23 as still open ("T23 stays
open; the next recommended fix is atomic failed-reload handling... Durable-
backing and bounded-capture deferrals remain unmet contract requirements, not
an accepted waiver"), and `docs/reports/G3.md`'s Open Items table (lines
1816-1839) shows row 7 (resident-cache eviction reconciliation), row 11 (join
budget at G4 scale), and rows 13-14 (sustained soak bandwidth separation) with
still-open sub-items as of increment 27 (2026-09-13, `a02e475`). This report
does the scoping work ENG-31 asks for — what the current architecture already
supports, what it explicitly does not, and what experiments would need to run
once T23 closes — without touching code and without prototyping.

Per `AGENTS.md` ("Respect the dependency graph... Do not start dependent
implementation early merely to keep slots occupied") and the assigning
instructions, no `spall_*` crate, fixture, or scenario file is modified by
this change. Only this report is added.

---

## 1. What "G5" already means in the frozen docs

`README.md` line 76 defines G5 as: "Measured streaming/LOD and multiple
physics regions; then survival content, inventory, crafting, creatures,
liquids, and fire." `docs/validation.md` lines 706-708 (the G5 gate
definition) is explicit that G5's job is to *define* the actual radius,
height, concurrent active regions, topology metadata size, and persistent
debris envelope **from G4 measurements** — i.e., G4 has to produce real
numbers before G5 can respond to them, and G4 itself is not closed (`G3.md`
rows 12-14 still have soak/bandwidth items open). `docs/tasks.md` T24 (lines
320-326) mirrors ENG-31 verbatim and depends on T23.

This means T24's very first prerequisite — a measured G4 envelope to react
to — does not exist yet. Everything below is scoped against the
*architecture's stated intentions* for G5, not against measured G4 numbers,
because none exist.

---

## 2. Per-area assessment

### 2.1 Region streaming

**Current architecture support.** `docs/architecture.md`'s "Streaming and
lifetime" section (lines 183-212) already specifies a single-world streaming
model: three tracked ranges (render residency, physics interaction,
structural dependency), a brick lifecycle (`absent -> requested -> resident
-> dirty -> checkpointed -> evictable`), and the T18 implementation
(`spall_voxel::residency`, lines 193-199) — enter/retain hysteresis, pinned
overlap counts, LRU eviction of clean/durable/unpinned entries only. T23
increments 6-12 and 22 (cited in `docs/reports/G3.md` lines 273-624, 1308+)
extended this to a **logical topology** (resident bricks ∪ retained evicted
digests) so a hash-agreeing replica and server can each evict independently
within *one* volume/world.

**What it does not yet address.** Everything built so far is **one bounded
volume** (`Volume::bounded`, `docs/architecture.md` line 44: "The first
bounded world uses one physics origin"; G3.md line 39: "one volume bounded to
256 x 128 x 256 m"). There is no multi-region world model at all: no second
independently-addressed world partition, no cross-region interest boundary,
and no notion of a region a player is *not* in being unloaded down to
metadata-only. `docs/architecture.md` line 189 does allow for this in
principle ("Storage partitions index data; they do not own indivisible
physical objects") but no partition boundary beyond brick-granularity
currently exists in code.

**Experiments needed post-T23.**
- Prototype scope: extend `spall_voxel` (or a new region-index layer above
  it) with a coarser-than-brick partition key, and measure resident-set
  memory/CPU when two such partitions are simultaneously active with
  players in each, versus the existing single-volume model. Rough cost:
  a `spall_voxel` + `spall_server` change on the order of T18's residency
  work (T18 landed across 9+ increments per `G3.md`); this is a multi-week
  slice, not a spike.
- Measure: resident brick count and dense-byte accounting (the existing T18
  ceilings, `docs/architecture.md` lines 193-199) under **two or more**
  concurrently active regions rather than one, to see whether the existing
  per-volume budget model composes linearly or needs a global budget
  arbitrated across regions.
- Measure structural-dependency loading (`docs/architecture.md` line 108,
  "Graph metadata for unloaded regions must remain available or be loaded
  before resolving support") when the dependency crosses a *region*
  boundary rather than just a brick boundary within one already-loaded
  volume — this is materially different from anything T07/T18 tested
  (`spall_structure`'s all-resident vs. `Streamed` residency modes,
  `docs/architecture.md` line 110, were built and tested inside one volume).

### 2.2 Render LOD seams

**Current architecture support.** `docs/architecture.md`'s renderer section
(lines 166-181) specifies greedy meshing per brick with cross-brick neighbor
sampling for seamless faces (line 168: "Generate faces using neighbor
samples across bricks, including local-volume brick seams") — this solves
*brick* seams at uniform resolution, not *LOD* seams between different
resolutions. The renderer roadmap's item 6 (line 177) explicitly defers
"distant LOD" to after G2, and no LOD geometry representation exists yet;
the closest analog is the lighting clipmap (item 3, lines 174, 179: "The
clipmap is a derived GPU lighting cache, not the world format"), which is a
lighting cache, not a mesh LOD system.

**What it does not yet address.** There is no distant-terrain LOD mesh
representation, no LOD transition/seam-stitching algorithm, and critically,
no code path that could violate or preserve the T24 acceptance bullet "LOD
changes never alter authoritative geometry" because there is no LOD geometry
yet to check that against. `docs/architecture.md` line 62 ("Each voxel
exists in exactly one authoritative owner... Derived meshes, colliders,
lighting volumes, and topology labels are caches") is the existing invariant
a LOD system would have to respect, but it has never been tested against an
actual multi-resolution mesh.

**Experiments needed post-T23.**
- Design spike (no acceptance claim possible without it): pick a LOD
  strategy (chunked mesh simplification vs. brick-coarsening vs. impostor)
  consistent with "cache, never authoritative" and "no resampling a falling
  building" (`docs/architecture.md` line 46). This needs an experienced
  graphics integrator sign-off analogous to T13's lighting-decision process
  (`docs/lighting-decision.md` precedent) before implementation — the repo's
  own contract (`docs/architecture.md` line 181: "not an invitation for
  separate agents to invent incompatible render pipelines") applies here too.
- Prototype scope: one seam-stitching fixture between a full-resolution near
  brick and a coarsened distant representation, measured for visible T-
  junctions/cracks and for whether the coarsened mesh can leak collision-
  relevant detail (explicitly disallowed: "Coarse occupancy can leak or
  over-occlude thin walls: this is a measured risk, not accepted correctness
  for collision" — architecture.md line 174, already flagged for the
  lighting clipmap and doubly true for a render-LOD mesh that must never be
  load-bearing for collision or support).
- Measure: re-lit/re-meshed cost when an edit lands inside the LOD band
  versus the near band, since dirty-region update costs (T14,
  `docs/validation.md` lines 20-42) were only measured against the near,
  full-resolution cache.

### 2.3 World generation versions

**Current architecture support.** `docs/architecture.md` line 50 requires a
"generator version" as authoritative metadata, and `docs/protocol.md` line
13 lists "generator version" in the handshake; `docs/protocol.md` line 102
("Required world metadata... generator version") requires it be persisted.
This is presently a single scalar tagging one deterministic generation
algorithm for the one bounded world — there is no support for *multiple*
coexisting generation versions inside one world (e.g., an old region
generated under v1 rules adjacent to a newly streamed-in region generated
under v2 rules).

**What it does not yet address.** Nothing in the current code generates
terrain procedurally at all beyond the fixed fixture scenes referenced
throughout `G3.md` (`separated_regions_scene`, `g1_full_envelope_setup`,
etc. — all hand-built, not procedurally generated). "World generation
versions" as a G5 concern (mixed-version regions, migration, or a frozen
generation contract that must reproduce byte-identical terrain for
unmodified regions) is entirely unbuilt and unspecified beyond the single
metadata field.

**Experiments needed post-T23.**
- This needs a design decision before any prototype: does Spall ever
  procedurally generate the larger world at all, or does "larger world"
  mean a larger hand-authored/edited bounded volume? README.md's own
  framing (line 53: "Prove the hard parts in a small world... Large streamed
  worlds follow measured success; an infinite world and unbounded active
  debris are not promised") does not commit to procedural generation.
  ENG-31 should get an explicit answer to this before scoping generation-
  version experiments further — it changes the entire shape of this area.
  Flagging as an **open question**, not a measurement task, until answered.
- If generation is added: prototype scope is a `spall_voxel`-adjacent
  generator crate/module, a fixture proving two adjacent regions generated
  under different frozen generator versions produce a byte-stable seam (no
  regeneration on version mismatch, explicit migration or coexistence
  policy) — comparable in size to T02/T03's original brick-storage-plus-
  fixture work.

### 2.4 Multiple rebased physics regions

**Current architecture support.** This is the most explicitly-flagged-as-
future area in the docs. `docs/architecture.md` line 43: "Physics and
rendering convert to f32 relative to an explicit nearby origin. The first
bounded world uses one physics origin." Line 164 (end of the "Collision and
character physics" section): "For a larger world, multiple independently
rebased physics regions are necessary when players are far apart. Region
merge/split and body transfer must be atomic and tested. A single origin
following one player is not an acceptable multiplayer large-world solution.
This is a G5 gate, not hidden work in the initial sandbox." This is a direct
statement that the architecture *anticipates* the ENG-31 requirement but has
built none of it — the entire simulation, collision, and replication stack
(`spall_physics`, `spall_sim`, T06's merged-cuboid compounds,
`docs/collision-decision.md`) assumes one origin today.

`docs/reports/G3.md` row 2 (line 1824) already exercises geographically
separated players up to 110 m apart (`Scene::SeparatedRegionsFar`) but this
is still **one f64-authority / one f32-physics-origin world** — separation
distance was tested, origin rebasing was not, because f32 precision loss is
not yet significant at hundreds of meters. `docs/architecture.md` line 43's
f64-authority / f32-local-physics split is the reason 110 m did not need a
second origin; it does not tell us where the precision cliff actually is.

**What it does not yet address.** No second physics origin, no rebasing
transform, no merge/split protocol, and no body-transfer-between-origins
code exists. `spall_physics` (T06 outcome, architecture.md lines 156-157) is
built entirely against one Rapier `PhysicsWorld` with one coordinate frame.

**Experiments needed post-T23.**
- Precision measurement (cheap, should be first): quantify f32 error
  accumulation in `spall_physics`/Rapier at increasing distances from a
  fixed origin (1 km, 10 km, 100 km) using the existing merged-cuboid
  compound representation, to find the actual distance at which contact
  stability or collision correctness measurably degrades. This is a
  bounded, code-light benchmark extension of the existing
  `collision-bench` binary (`docs/validation.md` line 89-91) — rough scope:
  days, not weeks.
- Prototype scope (large): a second concurrent `PhysicsWorld` instance
  rebased at a different origin, with `spall_sim` routing bodies/players to
  the correct instance by position, and an explicit rebasing transform
  applied at the boundary. This touches `spall_physics`'s adapter boundary
  and `spall_sim`'s tick loop (`docs/architecture.md` lines 16, 27, 68-80)
  significantly — comparable in size to the original T06+T08 combination,
  i.e., a multi-increment ticket of its own, not a T24 sub-task.
- Atomicity/testing plan for region merge/split and body transfer (the T24
  acceptance bullet: "approaching regions merge without body duplication")
  needs its own conservation invariant analogous to the existing split
  ledger (`docs/architecture.md` line 112: "source occupied cells = retained
  cells + child cells + explicitly destroyed cells") — a merge/split across
  *physics origins* has no equivalent ledger defined yet. This is a design
  question for the eventual T24 implementation, not something this report
  can specify without inventing the mechanism (out of scope here per the
  assigning instructions).

### 2.5 Region merge/split

**Current architecture support.** None directly — the closest existing
mechanism is `spall_structure`'s body split (terrain-to-body / body-to-body
transfer, `docs/architecture.md` lines 110-116) and T10's `SplitOff`
replication ops (`docs/architecture.md` lines 27, 96; `docs/protocol.md`
lines 42-46). Those are **within one volume/world**, not across two
independently-active *regions* in the G5 sense (two areas that were
independently simulated/streamed and are now becoming spatially adjacent).
The conservation-ledger discipline that governs body splits is the right
model to extend, but nothing currently defines what "merge" means for two
regions that may have diverged in resident/evicted state, structural graph
state, or (per 2.4) physics origin.

**What it does not yet address.** Region merge is unbuilt. There is also an
unresolved interaction with the still-open T23 row 7 residency reconciliation
(`G3.md` line 1829: "the `ResidencyController`/`ResidencyPass` API
duplication itself (unification, own design pass)... four residency
mechanisms exist in the tree, not reconciled into one") — a region
merge/split design should not be scoped against residency machinery that the
project's own increment 27 report says is still duplicated and unreconciled.

**Experiments needed post-T23.**
- Wait for the T23 row 7 residency unification (flagged in `G3.md` as its
  own follow-up, not a T24 sub-task) before designing region merge, since
  merge/split will need one consistent residency/backing contract to build
  against, not two.
- Prototype scope once residency is unified: a fixture with two
  independently-populated regions (distinct structural graphs, distinct
  resident/evicted brick sets) approaching each other, proving (a) no body
  is represented twice across the merge boundary, (b) the structural graph
  from each side stitches into one connected graph without re-running full
  connectivity analysis on the already-resolved side (`docs/architecture.md`
  line 108's "Searches are incremental, time/byte budgeted, and resumable"
  principle extended across a merge). This is a `spall_structure` +
  `spall_sim` design/prototype pass, likely comparable in scope to T07's
  original support-graph work (`docs/architecture.md` lines 98-116).

### 2.6 Body transfer

**Current architecture support.** Body transfer *within* one volume/world is
solid and tested: T08's terrain-to-body and body-to-body transfer
(`docs/architecture.md` lines 94, 112-116) preserves exact world location, id
stability, and mass/velocity, and T18's `BodySpatialIndex` (line 210) already
maps "every intersected world partition to the same stable body ID" — i.e.
the existing code already treats partition-crossing as a solved problem *at
brick granularity within one volume*.

**What it does not yet address.** Transfer of a body **between two
independently-active regions** (potentially under different physics origins,
per 2.4) is a different problem: it needs the same "one authoritative
identity and geometry owner" invariant (`docs/architecture.md` line 189) to
hold across a region boundary that may also be a physics-origin boundary and
a residency-backing boundary. None of that cross-boundary handoff exists.

**Experiments needed post-T23.**
- Once 2.4's rebasing prototype exists, extend it with one moving dynamic
  body (a rolling/falling detached beam, reusing the existing `sleep-wake`
  or `separated-regions` fixtures as a base) crossing the origin boundary,
  and measure: does its `BodyId` and geometry survive the crossing with no
  duplication and no discontinuous jump in world-space pose. This is a
  focused fixture on top of the 2.4 prototype, not separate large work.

### 2.7 Structural graph growth

**Current architecture support.** `spall_structure`'s support graph (T07,
`docs/architecture.md` lines 98-116) is designed for incremental,
budgeted, resumable analysis and already has a `Streamed` residency mode
(line 110) distinguishing "absent neighbor = empty" (`AllResident`) from
"absent neighbor = Unknown" (`Streamed`) specifically so a large, partially-
loaded world does not silently misclassify support. T22's material-strength
extension (lines 118-124, `docs/structural-strength.md`) adds a "support
forest rooted at the anchor plane" with saturating load accumulation — this
is a *global*, root-to-leaf structure per world, which raises an open
question below.

**What it does not yet address.** Nobody has measured what happens to graph
size, incremental-update cost, or the T22 support-forest's per-anchor-plane
traversal cost as the *number of loaded regions* (not just bricks) grows.
The support forest's root is "the anchor plane" (singular, per
`docs/architecture.md` line 120) — it is not yet clear whether a
larger/multi-region world has one anchor plane or needs a per-region
forest, and that ambiguity should be resolved before structural-graph-growth
experiments are designed (see open questions, section 3).

**Experiments needed post-T23.**
- Stress-scale the existing `spall_structure` incremental-update benchmarks
  (referenced implicitly by T07's acceptance bullets, `docs/architecture.md`
  lines 74-75) against a much larger connected structure spanning many more
  bricks/regions than any current fixture (`G3.md`'s largest is the
  64-brick `giant_collapse_attempt`, `docs/validation.md` line 1859-1860,
  currently `--ignored` in CI as an expensive measurement). Rough scope:
  extending existing benches, not new subsystems — a moderate task once T23
  closes and a real G4 envelope exists to size the stress fixture against.
- Measure graph metadata size per resident/evicted brick at the scale G4
  measurements eventually specify (`docs/validation.md` line 706's explicit
  instruction: "Measure... far graph traversal, and long-session storage
  growth" as literal G5 gate language) — this is a direct, literal G5 gate
  requirement and should be the first concrete measurement task queued once
  T23/G4 numbers exist.

---

## 3. Architectural risks and open questions

1. **Precision cliff is unmeasured.** Nothing in the repo states the actual
   f32 distance-from-origin threshold at which Rapier contact behavior
   degrades. This gates the entire multi-origin design (2.4) and should be
   the very first measurement, since it is cheap and determines how
   urgently multi-origin rebasing is actually needed for the G4-measured
   envelope (which itself does not exist yet — `docs/validation.md` line
   708's "Define actual radius... from G4 measurements").

2. **LOD-vs-authoritative-geometry divergence has no test surface yet.**
   The T24 acceptance bullet "LOD changes never alter authoritative
   geometry" cannot be checked today because no LOD geometry exists to
   check. The existing invariant (`docs/architecture.md` line 62, "Derived
   meshes... are caches") is the right principle but is currently only
   proven for full-resolution meshes; a LOD prototype needs its own
   conservation-style fixture (analogous to how T14's dirty-region updates
   were fixture-tested against ghosting, `docs/validation.md` lines 44-58)
   before any claim can be made.

3. **Region merge/split has no invariant defined that plays with the
   existing conservation ledger.** Section 2.5. Extending `docs/
   architecture.md` line 112's ledger ("source occupied cells = retained +
   child + destroyed") across a region merge is a design decision for
   whoever picks up T24 proper, not something this scoping report can
   settle — flagging it as the central open design question for T24's
   eventual implementation.

4. **Residency mechanism duplication is a blocking prerequisite, not a
   parallel-track item.** `docs/reports/G3.md` line 1829 explicitly
   documents four uncoalesced residency mechanisms
   (`ResidencyController`/`ClientResidency` from T18 vs.
   `ResidencyPass`/`ClientResidencyPass` from T23). Any G5 region-streaming
   or region-merge prototype (2.1, 2.5) that builds on top of "residency" as
   it exists today would be building on an acknowledged-unreconciled
   foundation. Recommend this unification lands (as its own ticket, already
   implied by G3.md's open items) before serious T24 region-streaming
   prototyping starts, or the region work will inherit the duplication.

5. **Structural anchor-plane scope is ambiguous at multi-region scale.**
   Section 2.7. Whether the T22 support forest is one global forest or one
   per region is undecided in the docs and materially affects both
   structural-graph-growth measurements and region merge/split design.

6. **World-generation scope is undecided** (section 2.3) — whether G5's
   "larger world" implies procedural generation at all is not answered by
   README.md or docs/tasks.md, and answering it changes the shape of the
   "world generation versions" measurement area substantially.

7. **G4 is the actual gating measurement, and it is not done.** Every
   "set a measured envelope" instruction in ENG-31 and in `docs/
   validation.md` line 708 depends on G4 numbers (bandwidth, memory, body
   counts under sustained load) that `docs/reports/G3.md` rows 12-14 show
   as only partially measured (rows 12-14 marked done in increments 14-19,
   but row 11's join-budget-at-G4-scale is still open per row 11's own
   entry, and the residency/reload correctness underlying all of it is
   still being hardened as recently as increment 27, 2026-09-13). Treat any
   G5 prototype sizing decision made before G4 fully closes as provisional.

---

## 4. Explicit non-claims

This report does not claim, and the coordinator should not read it as
claiming:

- That T24's acceptance bullets ("separated players retain correct physics
  precision," "approaching regions merge without body duplication,"
  "distant edits persist and affect support," "LOD changes never alter
  authoritative geometry") are met. None of the underlying mechanisms exist
  yet; this report only scopes the experiments that would test them.
- That T23 is done. It is not (`docs/tasks.md` lines 292-296;
  `docs/reports/G3.md` Open Items table, rows 7, 11, 13-14 as of increment
  27 / commit `a02e475`).
- That any numeric envelope (region radius, active-region count, debris
  count) is proposed here. `docs/validation.md` line 708 requires those
  numbers come from G4 measurements, which do not yet exist.
- That any code, prototype, or benchmark was written or run as part of this
  report. It was not — this is a documentation-only change.

---

## 5. ENG-32 / T25 — forward-looking note (not full scoping)

T25 depends on T23 (not done) and, for the larger-world envelope
specifically, likely on T24 (not started). Per the assigning instructions
this section stays intentionally short — sequencing information only, not
an API design.

**Prerequisites.** `docs/tasks.md` line 330 states the T25 dependency
exactly: "T23; T24 only if the game needs the larger-world envelope
immediately." Read literally, a first game-facing API slice (tools,
placement, materials, recipes, damage, entity spawn, asset loading) does
**not** strictly need T24 to be done if the game ships against the *current*
bounded-world envelope (256 x 128 x 256 m, per the G3 scene) rather than
waiting for a larger one. It does need T23 closed, since T25's acceptance
bullet ("agents can reproduce all engine gate scenes from a clean checkout")
implies the gate scenes it is reproducing are actually finished and stable.

**Where a first surface would sit, per the existing engine/game boundary.**
`README.md` lines 11-17 and `docs/architecture.md` lines 27-31 already draw
the boundary T25 must respect: engine crates (`spall_*`) never import
`sandbox`/game code; the sandbox package (`examples/sandbox`) owns material
catalogs, tool rules, and scenes; a *"small statically linked rules
interface"* is explicitly anticipated ("Add a small statically linked rules
interface when real game behavior first requires it," README.md line 15) as
the mechanism for exposing authoritative hooks (tool validation, damage,
recipes) to `sandbox_game` without an engine-side dependency inversion. A
first T25 API slice would plausibly be exactly that interface — a narrow
trait/callback surface `spall_sim` (or `spall_server`) exposes and
`sandbox_game` implements, not a new crate and not a generic plugin system
(explicitly ruled out, README.md line 94: "No plugin architecture... is part
of the foundation").

**Sequencing note only.** Given T23's still-open residency/join-budget/soak
items (`docs/reports/G3.md` Open Items table) and T24's total absence of
prototyping, real T25 work — beyond drafting the rules-interface shape on
paper — is blocked. This report does not design that interface; it only
notes where it would attach and that the attachment point (README.md's
"small statically linked rules interface") is already named in the docs, so
whoever picks up T25 is not inventing the boundary concept from scratch.

---

## Reproduce / verify this report's claims

This report makes no measurement claims of its own; every quantitative
figure cited above is reproduced from the existing `docs/reports/G3.md` and
`docs/validation.md`, whose own reproduction commands are listed in
`docs/reports/G3.md` lines 1841-1872 and `docs/validation.md`'s xtask block.
No new commands were run for this report.
