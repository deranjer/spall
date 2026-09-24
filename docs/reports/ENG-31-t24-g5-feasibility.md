# ENG-31 / T24 — G5 larger-world feasibility scoping

Status update, 2026-09-23: ENG-30/T23 was moved to `done` in Loopira at the
user's direction. ENG-31/T24 is now `in_progress`. The feasibility-scoping
work below remains the completed Increment 1; the old dependency review and
T23 audit are historical records, not the current ticket status. This report
is not a measured larger-world envelope.

Increment 1 was feasibility scoping only; it did not meet any T24 acceptance
bullet. It documents existing support, missing mechanisms, and experiments
needed for a larger-world decision. The 2026-09-18 T23 audit and its open-item
snapshot are preserved as historical evidence; they are not a claim about
current Loopira status.

Per `AGENTS.md` ("Respect the dependency graph... Do not start dependent
implementation early merely to keep slots occupied") and the assigning
instructions, no `spall_*` crate, fixture, or scenario file was modified by
the original scoping change.

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
- That the original 2026-09-17 scoping report accepted T23 or the G3/G4 gate.
  ENG-30 was moved to Done in Loopira on 2026-09-23 at the user's direction;
  the remaining evidence limitations are recorded separately in `G3.md`.
- That any numeric envelope (region radius, active-region count, debris
  count) is proposed here. `docs/validation.md` line 708 requires those
  numbers come from G4 measurements, which do not yet exist.
- That code or benchmarks were part of Increment 1. They were not; Increment
  2 adds the precision prototype below.

---

## 5. ENG-32 / T25 — forward-looking note (not full scoping)

At the time this note was written, T25 depended on T23 (not done) and, for the larger-world envelope
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

**Sequencing note only (historical, written before the 2026-09-23 ticket
updates).** Given T23's then-open residency/join-budget/soak items
(`docs/reports/G3.md` Open Items table) and T24's then-absence of prototyping,
real T25 work — beyond drafting the rules-interface shape on paper — was
considered blocked. This report does not design that interface; it only notes
where it would attach and that the attachment point (README.md's
"small statically linked rules interface") is already named in the docs, so
whoever picks up T25 is not inventing the boundary concept from scratch.

---

## Reproduce / verify the original scoping claims

Increment 1 made no new measurement claims; its quantitative context is
reproduced from the existing `docs/reports/G3.md` and `docs/validation.md`.
Increment 2's independent precision measurement and reproduction command are
recorded below.

---

## Increment 2 — fixed versus locally rebased precision probe (2026-09-23)

After ENG-30/T23 was moved to `done` in Loopira, `collision-bench --precision`
was added as the first measured T24 prototype. It sweeps 11 offsets from 0 m
to 100 km. At each, it drops a 0.5 m voxel cube onto an 8 m merged-cuboid
floor for 900 frames, walks a capsule forward for 60 ticks, and drives one
dynamic voxel cube into another for 240 ticks. Character and body interactions
are also repeated in an explicit local physics frame whose origin equals the
tested X offset; character positions are converted from global `f64` to local
coordinates at the sweep boundary. Raw JSON is
`.local/runs/eng-31-precision/collision-precision.json`.

Measured on this checkout with `rustc 1.96.1`, Windows x86_64 MSVC: all eleven
rigid-body floor drops and cube-to-cube contacts remained finite, with
maximum floor penetration 3.427 mm. Dynamic-body target peak speed varied by
at most 0.030 m/s from the 1.605 m/s origin-zero result. The fixed-world
character sweep exposed a more sensitive issue: some ticks reported an
immediate floor contact and zero horizontal movement despite grounded state.
Across the 60-tick walk, lost travel versus origin zero ranged from zero to
0.45 m (six 75 mm movement steps); the pattern was non-monotonic across tested
offsets. Collision traces show time-of-impact zero and an upward floor normal
on those ticks. With the same geometry and authority positions but an X-local
physics frame at each tested offset, all 60 ticks moved as in the origin-zero
control and the travel delta was zero at every offset. A paired 60-tick run
stepped two independent local `PhysicsWorld`s concurrently with players 100 km
apart: both stayed grounded, remained finite, and travelled 4.425 m (zero
delta). This is a physics-adapter prototype, not server integration. Together,
the measurements support local physics coordinates as the direction for T24,
but do not yet prove the root cause or establish runtime origin routing and
body transfer. This is not an accepted world envelope. CPU-only; no GPU capture
was involved.

Reproduce with:

```sh
cargo run --release -p spall_physics --bin collision-bench -- --precision --out .local/runs/eng-31-precision
```

## Increment 3 — coarse region grouping and resident-set sizing (2026-09-23)

Added a configurable `RegionLayout` over signed brick coordinates and a small
`region_bench` against the existing separated-regions fixture. The grouping is
runtime spatial organization only: it does not rewrite voxel coordinates,
volume identity, or structural connectivity. Euclidean division keeps regions
well-defined for negative brick coordinates.

On the fixture, 9 bricks are resident, of which 2 are dense and use 131,072
bytes. Grouping those same bricks into cubic regions gives 9 active regions at
1 brick per side (largest region: 1 brick / 65,536-byte dense upper bound), 4
regions at 2 bricks per side (largest: 4 bricks / 262,144 bytes), and 1 active
region at 4, 8, or 16 bricks per side (largest: 9 bricks / 589,824-byte dense
upper bound). These are bounds assuming every brick in each group is dense;
the current sparse fixture's actual dense payload is much smaller. The
partitions were derived from the fixture's existing resident set, preserving
its authoritative topology; this is a CPU sizing prototype, not streaming,
eviction, or persistence behavior.

Checks: `cargo fmt --all --check`; `cargo check -p spall_voxel --bin
region_bench`; `cargo run --release -p spall_voxel --bin region_bench`; and
`git diff --check` passed. No test suite was run. Next: exercise a continuous
structural dependency across a region boundary, then prototype active-region
selection and byte accounting without dropping distant authoritative edits.

Reproduce with:

```sh
cargo run --release -p spall_voxel --bin region_bench
```

## Increment 4 — structural connectivity across coarse regions (2026-09-23)

Added `region_topology_bench`: a 97-cell beam plus one-cell anchor spans three
resident bricks and crosses the boundary between two regions at two bricks per
axis. The existing global structure analysis read all three bricks and
reported one component containing all 97 cells, all supported. The observed
result confirms that coarse grouping can remain an index over the existing
authoritative volume without cutting the structural graph at a region
boundary. It does not exercise region unloading/reloading, dependency-driven
streaming, or distant-edit durability.

Checks: `cargo fmt --all --check`; `cargo check -p spall_structure --bin
region_topology_bench`; `cargo run --release -p spall_structure --bin
region_topology_bench`; and `git diff --check` passed. No test suite was run.
Next: implement a bounded active-region selection prototype and measure its
resident sets while keeping backing edits/topology durable across eviction.

Reproduce with:

```sh
cargo run --release -p spall_structure --bin region_topology_bench
```

## Increment 5 — bounded active-region selection and durable eviction round trip (2026-09-23)

Added `ActiveRegions`, which selects cubic regions around one or more brick
centers, applies separate enter/retain radii, and fails atomically when the
active-region limit is exceeded. `RegionLayout::active_bricks` expands that
selection only when the full brick footprint is within a caller-provided cap.
The shared residency cache now accepts an explicit active brick set and keeps
that set for subsequently loaded bricks, so new loads inherit active status.

`active_region_bench` exercises the existing server backing/eviction path on
the separated-regions fixture. One active two-by-two-by-two-brick region
represents an 8-brick slot footprint but contains 4 resident bricks in this
fixture. Starting from 9 resident bricks, reducing the cache limit to those 4
evicted 5 bricks, including an edited distant brick. The dirty revision was
persisted before eviction; after moving interest to the distant region, that
brick reloaded at revision 16 and the edited cell remained modified air. The
reloaded cache had 4 resident bricks, 1 currently active brick, and 65,536
resident dense bytes (262,144-byte all-dense upper bound for the one active
region). The benchmark uses deterministic in-memory backing: it exercises the
acknowledged brick eviction/load contract, not a cold process restart or disk
failure/crash recovery. It also does not integrate region selection into the
serve loop or validate active-region hysteresis under player motion.

Checks: `cargo fmt --all --check`; `cargo check -p spall_voxel`; `cargo check -p
spall_server --bin active_region_bench`; `cargo run --release -p spall_server
--bin active_region_bench`; and `git diff --check` passed. No test suite was
run. Next in the broader T24 measurements: render LOD seam behavior, then
generation-version policy and the remaining merge/split, body-transfer,
structural-growth, and measured-envelope work.

Reproduce with:

```sh
cargo run --release -p spall_server --bin active_region_bench
```

## Increment 6 — render-only 2:1 heightfield seam spike (2026-09-23)

Added an isolated `spall_mesh::lod` helper that builds vertical transition
quads between aligned fine and coarse heightfield edge profiles. It accepts
integer edge heights and returns ordinary render mesh quads; it has no access
to `Volume`, physics, or structural state. A CPU probe used eight fine edge
intervals against four coarse intervals (2:1). Six intervals differed, with a
sum of 10 unmatched vertical cell-faces (`0.625 m²` at 25 cm cells). The
transition produced six quads / 12 triangles. Quad bounds and directions were
checked, and the emitted patch area equaled the measured boundary difference
(zero residual for this profile).

This is a narrow heightfield boundary prototype, not a general 3D LOD solution
or integrated renderer. It does not cover caves, overhangs, multi-chunk mesh
ownership, ambient occlusion continuity, GPU captures, transition cost under
edits, or authoritative geometry. A broader strategy/design review is still
needed before adopting LOD for general voxel terrain.

Checks: `cargo fmt --all --check`; `cargo check -p spall_mesh --bin
lod_seam_bench`; `cargo run --release -p spall_mesh --bin lod_seam_bench`; and
`git diff --check` passed. No test suite or GPU capture was run. Next: resolve
world generation-version policy for new regions adjacent to existing regions,
then continue merge/split, body transfer, structural growth, and measured
envelope work.

Reproduce with:

```sh
cargo run --release -p spall_mesh --bin lod_seam_bench
```

## Increment 7 — world generation versioning audit (2026-09-23)

The repository has no procedural terrain generator: current world setup builds
fixed scenes from explicit voxel edits. The existing `generator_version` is a
single world-level compatibility field. It is written to checkpoint metadata,
checked for exact equality on restore, and checked for exact equality in the
network handshake. Server and client fixtures currently hard-code version 1.
No region or brick records identify which generator version produced them, and
there is no path that regenerates evicted geometry.

That means the present contract is coherent for baked/edited world data but
does not support mixed generator versions within one world. The conservative
current behavior is to keep the world-level version pinned and reject a
whole-world mismatch; never regenerate persisted or modified terrain because a
new generator is available. Per-region version coexistence requires an explicit
product decision that procedural generation is part of G5, followed by a
contract for boundary continuity or an explicit region migration. This audit
does not select that direction or add per-region metadata.

Evidence is a source audit of `spall_server::PersistConfig`,
`StoredWorldMeta`, `validate_world_meta`, `spall_protocol::Handshake`, the
server/client handshake builders, and the current fixture-based scene setup.
No code or runtime behavior changed in this audit, and no tests were run.

## Increment 8 — region merge and body-transfer design boundary (2026-09-23)

The existing `BodySpatialIndex` is only a spatial query index over one
`SimWorld`. `one_body_keeps_one_identity_while_crossing_partition_boundaries`
already checks that a single body is referenced by every intersected partition
before and after moving, while `body_count()` remains one. This establishes
same-world partition indexing; it does not transfer a body between simulation
regions or merge their physics origins.

The actual ownership boundary is still one private `PhysicsWorld` inside each
`SimWorld`. Dynamic bodies retain one authoritative `Volume`, stable entity and
volume IDs, world-space `BodyPose`, linear/angular velocity, collider revision,
and an opaque handle owned by that physics world. There is no current
multi-origin router or API to stage a body's collider in another `PhysicsWorld`
and atomically retire the old handle. Reusing `BodySpatialIndex` as if it
implemented that transfer would leave ownership and solver state unresolved.

Before implementing cross-origin transfer, the prototype needs explicit
invariants: each stable body/volume pair has exactly one authoritative owner;
the voxel geometry and world-space pose are unchanged at handoff; linear and
angular velocity are preserved; the destination collider is valid before the
source collider is retired; and failure leaves exactly one usable owner. A
region merge must also reconcile terrain/brick revisions and structural graph
dependencies before either region stops simulating. The existing occupied-cell
split ledger is not sufficient by itself because body transfer preserves the
same occupied geometry while changing its physics-frame representation.

This is a design and implementation boundary, not a merge prototype or a claim
that the T24 acceptance criterion is met. The next concrete implementation
step is a two-origin transfer spike at the simulation/physics boundary, with a
canonical geometry digest and before/after world-space pose and velocity
measurements, followed by an atomic failure-injection scenario. Only after that
should region merge add residency and structural-graph reconciliation. The
residency paths remain separately implemented, so this increment does not
combine their policy APIs.

Checks: source audit of `BodySpatialIndex`, its existing partition-crossing
scenario, `SimWorld` ownership and physics-handle fields, and the architecture's
split ledger; `git diff --check` passed. No code or runtime behavior changed;
no tests were run. The T24 acceptance criterion remains open.

## Increment 9 — two-origin physics body-transfer spike (2026-09-23)

Added `spall_physics`'s standalone `physics-transfer` scenario. It stages a
64-cell voxel body in a destination `PhysicsWorld` whose local origin is 112 m
from the source origin. The source and destination represent the same
world-space body pose, orientation, linear/angular velocities, and voxel-grid
geometry. A destination with an invalid cell size is rejected before it creates
a body. A second injected rejection after destination collider creation retires
that staged collider and confirms the source remains active. On retry, the
destination state is compared before the source is retired. The transferred
body then advances for 60 ticks beside a control body that stayed in the source
origin.

Measured by `physics-transfer`: geometry digest `cdfc59fa55414211` for 64 solid
cells; staged world-pose, linear-velocity, angular-velocity, and rotation
errors were all zero at the transfer boundary. After 60 ticks, transferred
position differed from the control by `0.0006857 m`, and velocity difference
was zero. After handoff there was one active body in the destination and none
in the source. Both injected rejection paths preserved source ownership.

This is an adapter-level feasibility spike, not authoritative body transfer:
the harness supplies a cloned occupancy grid and kinematic snapshot directly
to two physics worlds. Stable `EntityId`/`VolumeId` ownership, `SimWorld`
registries, transaction publication, cross-region terrain/residency, and
structural graph reconciliation are not involved. The source and staged
destination collider coexist briefly before retirement; the harness does not
prove that production callers can keep that interval unpublished or roll back
every possible failure in collider construction. Region merge/split remains
unimplemented, and the T24 acceptance criterion remains open.

Checks: `cargo fmt --all`; `cargo check -p spall_physics --bin
physics-transfer`; `cargo run --release -p spall_physics --bin
physics-transfer` (passed, values above); `git diff --check` passed. No test
suite was run. Reproduce with:

```sh
cargo run --release -p spall_physics --bin physics-transfer
```

Next: carry this staged handoff through `SimWorld` stable ownership and
tick-boundary publication, with failure rollback, before implementing region
merge, residency reconciliation, and structural graph stitching.

## Increment 10 — SimWorld integration gate audit (2026-09-23)

Tracing the handoff into `SimWorld` exposed two prerequisites the adapter spike
does not cover. First, each `SimWorld` owns its own `IdRegistry`; the registry
contract guarantees uniqueness only within one world. Two separately-created
regions can therefore allocate the same `EntityId` and `VolumeId`. Moving one
body while retaining those IDs is unsafe unless region worlds draw IDs from a
shared authority or the coordinator proves and reconciles disjoint ranges.
Second, the physics frame is currently implicit: terrain colliders, body spawn
and split creation, physics-to-authority pose sync, and player sweeps all use
world coordinates directly as `f32`. There is no origin field in `SimWorld` to
convert all of those paths consistently.

These are coordinator-level contracts, not a missing helper on `SimWorld`.
Adding a body-transfer method now would either preserve IDs that may already
collide or rebase one body's collider while the rest of that simulation still
uses a different coordinate frame. Both would violate the stable-identity and
single-frame assumptions in the existing code. The next implementation unit
must establish one region coordinator with a shared authoritative ID registry
and explicit origin per physics region, then route body ownership and tick
publication through that coordinator. The transfer spike remains useful as
the destination-staging metric, but it is not sufficient to integrate safely.

Evidence: source audit of `SimWorld`'s private `registry`, `physics`, body and
volume-owner maps; `IdRegistry`'s per-world monotonic counters;
`spawn_body`/restore and split-collider creation; `step_physics` pose sync; and
player sweep paths. No code or runtime behavior changed. No tests were run.
`git diff --check` passed. Region-coordinator design and implementation are
still required before `SimWorld` handoff or the region-merge criterion can be
claimed.

## Increment 11 — origin-aware SimWorld slice (2026-09-23)

Added `PhysicsOrigin` as an explicit world-`f64` to local-physics-`f32` frame.
`Simulation::new_with_physics_origin` and the corresponding `SimWorld`
constructor now localize terrain collision grids by shifting their grid-cell
origin, and localize body poses and player sweeps. Physics-to-authority body
pose synchronization and contact-point conversion restore the origin. Split
child creation and terrain collider publication validate localized positions
before publishing the transaction. Existing constructors retain origin-zero
behavior. IDs remain centralized in one `SimWorld` registry.

`region-origin-bench` compared a falling 64-cell dynamic body on a terrain slab
at origin zero with the same scene translated 100 km in world X and simulated
relative to a 100 km local origin. Both runs retained one contact pair and
finite state through 240 physics steps. After removing the 100 km world offset,
position error was `0 m` and velocity error was `0 m/s`. The active-region
eviction benchmark retained its previous result: 9 resident bricks reduced to
4, 5 evicted, then 4 resident after reload; the edited cell reloaded as
modified air at revision 16. The physics transfer spike also still passes.

This slice gives one `SimWorld` one explicit local physics frame. It does not
yet coordinate several simultaneous `PhysicsWorld`s, assign bodies/players to
different frames, resolve cross-region contacts, or move a body between
`SimWorld` instances. The safe architecture direction remains one authoritative
ID/geometry owner with region-local physics worlds beneath a coordinator; do
not create independent authoritative worlds with overlapping ID registries.
Region merge and transfer acceptance remain open.

Checks: `cargo fmt --all`; `cargo check -p spall_sim --bin region-origin-bench`;
`cargo check -p spall_server --bins --all-features`; `cargo run --release -p
spall_sim --bin region-origin-bench` (passed, measurements above); `cargo run
--release -p spall_server --bin active_region_bench` (previous residency
measurements reproduced); `cargo run --release -p spall_physics --bin
physics-transfer` (passed, previous measurements reproduced); `git diff
--check` passed. No test suite was run. Next: introduce the shared-authority
region coordinator and route each body/character to a region-local physics
world, preserving one registry and explicit cross-region contact/merge rules.

Reproduce the origin scenario with:

```sh
cargo run --release -p spall_sim --bin region-origin-bench
```

## Increment 12 — stable-identity region merge coordinator (2026-09-23)

Added `spall_sim::RegionCoordinator`, a deterministic ownership control plane
keyed by stable world entity IDs rather than solver-local body handles. Regions
carry explicit `PhysicsOrigin`s. A merge selects the lower region ID as the
survivor, rewrites each retired entity's owner exactly once, and returns both
origins plus the transfer count for tick-boundary application. Reassigning an
entity to a second region is rejected; repeating assignment to its current
region is idempotent.

`region-coordination-bench` created two regions 100 km apart and merged 512
entities split evenly between them. Measured result: 2 regions became 1, all
512 IDs remained present with unique survivor ownership, 256 owner entries
transferred, and a sample point converted back to the same world position from
the retired and survivor frames. Merge preflight now requires every transferred
entity's authoritative `f64` world position and confirms it is representable in
the survivor frame before changing ownership. Missing-pose and out-of-range
injections both failed atomically with the original two-region ownership
unchanged. The planner also requires an explicit finite separation and merge
threshold: a measured 100 km separation was rejected atomically against a 2 m
threshold, while an approaching 0.5 m pair was eligible for preflight.

At Increment 12 this was a control-plane prototype: live region worlds and
collider handoff were not yet present. Increments 13–14 below add those
adapter/coordinator layers and the character-query measurement. Production
`SimWorld` integration and structural reconciliation remain open.

Checks: `cargo fmt --all`; `cargo check -p spall_sim --bin
region-coordination-bench`; `cargo run --release -p spall_sim --bin
region-coordination-bench` (passed, including both atomic rejection cases);
`git diff --check` passed. No test suite was run.

Reproduce the control-plane scenario with:

```sh
cargo run --release -p spall_sim --bin region-coordination-bench
```

## Increment 14 — separated-player rebased query sweeps (2026-09-23)

`PhysicsRegionSet::sweep_character` now routes each character to its region's
world, creates/rebuilds that player's terrain query window against the
authoritative shared voxel volume, localizes the derived grid by the region
origin, and sweeps at local coordinates. The benchmark places two independent
resident floor patches and players 100 km apart, with a separate query cache
for each player. Across 60 ticks both remained grounded for all 60 and finite.
Near/far travel was 4.422164551913816 m and 4.422164551913738 m; measured
travel delta was `-7.82e-14 m` from floating-point roundoff.

This covers separated-player precision through the actual region-set character
query path, including per-region query-window construction. It remains separate
from `SimWorld::advance_players`; production player registration, the shared
body/terrain contact set, and server tick integration are still open.

Checks: `cargo fmt --all`; `cargo check -p spall_physics --bins`; `cargo check
-p spall_sim --bins --all-features`; `cargo check -p spall_server --bins
--all-features`; `cargo run --release -p spall_physics --bin
region-player-bench` (passed with the measurements above); `git diff --check`
passed. No tests were run.

## Increment 15 — distant support edit, disk eviction, and reload

Added `region-support-reload-bench`, combining `ResidencyController`, the
SQLite `DiskBrickBacking`, and `StructureIndex` on a 97-cell support bridge
spanning three bricks. `DiskBrickBacking` now also implements the server
controller's `ResidencyBacking` interface over the same SQLite table, so this
scenario uses the production disk codec/store rather than a parallel fixture
format.

Measured sequence: all 97 cells were initially supported; removing the remote
anchor made 96 beam cells unsupported. The edited anchor brick was persisted
before eviction. While it was absent, streamed structural analysis classified
64 resident cells as unknown instead of assuming air. After dropping the
controller, reopening the SQLite file, and loading the brick, its revision
matched the edited revision and the anchor sampled as modified air. Complete
analysis then resolved to 96 unsupported cells and zero unknown cells. The
scenario cleans up its uniquely named temporary SQLite files.

This proves a distant durable edit changes structural support across a
region/brick boundary after reload. It does not run the full server movement
loop or automatically evict from player interest; the bounded residency
controller is driven directly in this scenario.

Checks: `cargo fmt --all`; `cargo check -p spall_server --bin
region-support-reload-bench`; `cargo run --release -p spall_server --bin
region-support-reload-bench` (passed with the measurements above). No tests
were run.

## Increment 16 — render seam and authoritative voxel invariance

Extended `lod-seam-bench` with a seeded authoritative voxel volume. It records
the resident brick content hash and a solid-cell sample before generating the
2:1 render transition, then checks both after seam generation. The hash and
sample remain identical (`authoritative_volume_hash_unchanged=true`); the seam
function still has no `Volume` parameter and operates only on edge profiles.
The existing seam measurement remains 6 transitions over 8 fine/4 coarse
intervals, 10 cell-faces / `0.625 m²` stitched, residual boundary area 0.

This verifies the current seam helper cannot mutate authoritative voxel data
through its API and that the benchmark's volume remained unchanged. It does not
prove an integrated renderer/LOD switch because the current seam helper is not
yet wired into neighboring chunk mesh selection or GPU rendering.

Checks: `cargo fmt --all`; `cargo check -p spall_mesh --bin lod_seam_bench`;
`cargo run --release -p spall_mesh --bin lod_seam_bench` (passed, including
unchanged authoritative hash/sample). No tests were run.

## Increment 18 — bounded physics-region debris envelope

Added `region-scale-bench` as a concrete, collision-producing workload across
multiple local worlds: 8 regions at 100 km spacing (700 km between the outer
origins), 8 dynamic 64-cell bodies and one 8,192-cell floor collider per
region, for 64 dynamic bodies, 8 fixed terrain colliders, and 69,632 solid
cells total. After 120 full region-step cycles, every body remained finite,
population matched, and all 8 regions reported at least 8 body/floor contact
pairs. One release measurement was `3.0189 ms` for the 120 step cycles plus
64 body-state reads per cycle (`0.02516 ms` per step cycle) on the current
Windows x86_64 workstation. The timer excludes world/collider construction and
is one sample, not a p95 or saturation result.

Together with `region-player-bench`, the evidence demonstrates two capsule
players at 100 km separation and the above 8-region debris/floor fixture. These
are separate adapter scenarios; they do not establish one integrated server
limit, a maximum player/debris count, memory headroom, or sustained-load
envelope. No larger scale claim is made.

Checks: `cargo fmt --all --check`; `cargo check -p spall_physics --bin
region-scale-bench`; `cargo run --release -p spall_physics --bin
region-scale-bench` (passed with measurements above); `git diff --check` passed.
No tests were run.

## Increment 19 — T24 acceptance audit and durable empty-brick revision fix

The disk-backed support scenario exposed a durable-format edge case during
reload review: a known-empty modified brick was previously returned through
the controller's revision-less `KnownEmpty` path. `DiskBrickBacking` now
returns an all-air `StoredBrick` carrying the saved revision and edited bit,
so a distant edit remains version-identical after reload. The SQLite support
scenario still passes with the exact revision and modified-air sample.

T24 evidence is bounded to measured fixture sizes. Demonstrated physics
fixtures span 700 km between eight region origins, exercise 64 dynamic voxel
bodies against eight terrain colliders, and separately exercise two grounded
players 100 km apart. The persistent support fixture spans three 8 m bricks
and restores the edited revision after a SQLite reopen. The render seam
fixture is one synthetic 2:1 heightfield transition with an unchanged
authoritative voxel hash. These are fixture bounds, not a production
radius/height limit or a maximum world, player, debris, or storage claim.

Remaining acceptance gaps:

| T24 area | Evidence / status |
| --- | --- |
| Multiple rebased physics origins; separated-player precision | Adapter scenarios pass at 100 km separation; no integrated server `SimWorld` region routing. |
| Approach/merge and split without duplicated bodies | Explicit transfer, split, merge guard, and 512-ID atomic preflight pass; no continuous approach-triggered merge, bulk transactional migration, recentering, terrain/structural reconciliation, or production routing. |
| Streaming and distant persistent edit | SQLite eviction/reopen restores exact revision and support effect; no long-session churn/growth curve or movement-driven server streaming run. |
| LOD seam authority | Synthetic 2:1 helper closes its seam and leaves voxel hash unchanged; no general cave/overhang or renderer/GPU integration. |
| Far structural graph traversal | Three-brick/two-coarse-region graph proves boundary connectivity only; no physically far or large graph growth measurement. |
| Generation and versioning | No procedural terrain generator exists; world-level generator version is pinned and mismatch fails closed. No honest generation throughput/seam measurement can be produced before a generator contract is assigned. |
| Measured world envelope | Reported only as fixture spans and populations; no product radius/height, active-region cap, topology-metadata size, memory ceiling, p95/p99, or long-duration envelope is established. |

Reproducible adapter evidence commands are listed in `docs/validation.md`.
T24 cannot claim the G5 larger-world feasibility gate passed until these open
integrations and measurements are resolved. This audit satisfies the ticket's
reporting requirement while keeping gate failures visible.

Checks for this increment: `cargo check -p spall_server --bins --all-features`;
`cargo run --release -p spall_server --bin region-support-reload-bench` (passed,
exact revision restored); `git diff --check` passed. No tests were run.

## Increment 13 — multiple live region physics worlds and routed body transfer (2026-09-23)

Added `PhysicsRegionSet` in `spall_physics`. Each region owns a local Rapier
world at an explicit `PhysicsOrigin`; body IDs carry the region namespace and
are rejected when used against the wrong world. The set steps all regions in
stable namespace order, converts world-space body placement into local `f32`,
and reports both local solver state and restored world translation.

Added stable entity-to-region/body routing to `spall_sim::RegionCoordinator`.
Its transfer operation builds the caller-provided authoritative collider in
the destination frame, restores pose and linear/angular velocity, validates
position, velocity, quaternion, and finite state, then retires the source and
updates ownership. Merge refuses a retiring region that still owns live
physics bodies; after those bodies are transferred to the survivor, the empty
region can be removed. The live coordinator scenario transferred a 64-cell
voxel body from origin 100000 m to 100112 m and back, each pose transfer had
0 m world-space error, exactly one active owner was retained, and merge reduced
the two-region state to one region. The independent `region-physics-bench`
stepped three local worlds for 20 ticks before transfer and 60 after; compared
with uninterrupted control, final position error was `0.0006933 m`, linear
velocity error `0 m/s`, angular velocity error `0 rad/s`, rotation error `0`,
and one active body existed at the destination after transfer.

Added the `region-player-bench` character-query scenario using the same
`PhysicsRegionSet`: two terrain patches and players 100 km apart, with one
`CharacterQueryCache` per local world. Over 60 ticks both players remained
grounded all 60 ticks and finite. Near/far travel was 4.422164551913816 m and
4.422164551913738 m respectively; delta was `-7.82e-14 m` (floating-point
roundoff). This includes query-window rebuild/localization and actual capsule
sweeps. It is still an adapter-level prototype rather than the production
`SimWorld` player path.

This is still not a production `SimWorld` integration. Player queries, per-
region terrain colliders, contact collection/damage, edit commits, and central
registry allocation do not yet route through these types. The merge test uses
an explicit handoff to the survivor's existing origin; origin recentering and
bulk region merge with many bodies remain open. It proves adapter-level
simultaneous region stepping and per-body transfer, not a player/debris/world
scale envelope.

Checks: `cargo fmt --all`; `cargo check -p spall_physics --bins`; `cargo check
-p spall_sim --bins --all-features`; `cargo check -p spall_server --bins
--all-features`; `cargo run --release -p spall_physics --bin
region-physics-bench`; `cargo run --release -p spall_physics --bin
region-player-bench`; `cargo run --release -p spall_sim --bin
region-coordination-bench`; `cargo run --release -p spall_physics --bin
physics-transfer`; `cargo run --release -p spall_sim --bin region-origin-bench`;
`git diff --check` all passed. No test suite was run.

## Increment 17 — live region split and merge ownership checks

Extended `region-coordination-bench` to exercise a live split: two bodies begin
in one namespaced physics world, one is rebuilt/transferred to a newly created
region, and both stable entity IDs remain associated with exactly one active
body in the expected region (1 body per region). The existing live merge path
also verifies it refuses to retire a region while its body remains there, then
succeeds after that body is handed to the survivor. This is an explicit
per-body split/merge prototype; bulk transactional partitioning and automatic
split thresholds are not implemented.

Checks: `cargo fmt --all`; `cargo check -p spall_sim --bin
region-coordination-bench`; `cargo run --release -p spall_sim --bin
region-coordination-bench` (all three scenarios passed: live transfer+merge,
live split, and 512-ID merge preflight). No tests were run.
