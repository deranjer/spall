# Water physics: agent handoff (column rewrite)

Status: design handoff dated 2026-10-10, replacing the previous brief that led
to the ENG-103..ENG-125 MAC/VOF solver. This is a target design, not
implemented code. Loopira tickets for the work items below are listed in
[Tracking](#tracking). Read `AGENTS.md`, `README.md`, `docs/architecture.md`,
`docs/protocol.md` and `docs/validation.md` first.

## Why the rewrite

The production water today is `spall_fluid::grid_mac`: a 3D two-phase MAC
Navier-Stokes solver with VOF transport, multigrid-preconditioned CG pressure
and cut-cell shorelines (~10.5k lines), plus ~2.5k lines of experimental
ENG-122 modules (`component_fluid`, `phase_graph`, `phase_pressure`,
`phase_water`, `cut_cell`). It is wired in through `AuthoritativeWater` in
`crates/spall_sim/src/water.rs`. Measured problems (`docs/reports/ENG-121.md`,
`ENG-121-rest-state.md`, `ENG-122.md`):

- **It does not scale.** One dense domain box covers all water and is
  coarsened (2..8 voxels per fluid cell) to fit a 50,000-cell budget
  (`WATER_FLUID_CELL_BUDGET` in `examples/sandbox/src/worldgen_scene.rs`).
  Cost follows domain size and pressure iterations, not moving water.
- **Rest is not rest.** Staircase terrain drives 1-3 m/s parasitic currents, so
  a lake never sleeps and an idle lake costs ~15 ms per 50 ms fluid step.
- **Coarsening breaks gameplay.** Thin walls leak (a cell is solid when half of
  its voxels are), 5-6% of seeded shoreline water is lost, and the open domain
  top drained a lake (ENG-127).
- **It walls up.** In play, water stands as a vertical wall next to empty space
  and never levels out. The cause has not been verified; candidates are the
  stationary zero-outflow cut caps (commit `ed024a9`) and the 60-quiet-step
  sleep freezing a stalled state. It is a symptom of the architecture, so do
  not spend time fixing it in `grid_mac`.
- **Tuning doesn't converge.** ENG-122 spent ~20 increments on cut-cell
  pressure edge cases.

## Priorities (user, 2026-10-10)

In strict order:

1. **No major visible failures.** Water must never wall up, hang in the air,
   leak through intact walls, or appear/vanish. Minor physical inaccuracy is
   acceptable. Water only needs to be approximately right.
2. **Very simple physics that scales to essentially massive worlds.** Cost must
   follow *moving* water, not world size or total water volume.
3. **Acceptable performance** inside the existing server tick budget.

Visual quality comes last (see W4). This supersedes the old brief's position
that simple neighbour equalization is insufficient. Momentum (dam-break surges)
is still wanted, and the pipe model below provides it cheaply. Navier-Stokes
accuracy, vortices, sealed-air pressure and the diving-bell behaviour are
**explicitly dropped**.

Gameplay goals are unchanged and must stay reachable: dams (build, fill, break),
canals that redirect water and change connected levels, swimming with currents,
boats. Full-world destruction, authoritative multiplayer and CPU-only dedicated
servers remain hard requirements.

## Model: stacked water columns with virtual pipes

### Representation

- Work at **terrain-cell resolution horizontally** (0.25 m, `CellScale::Quarter`):
  one column per terrain (x, z). There is no coarsening, so a one-voxel wall
  always blocks water.
- Each column is split into **spans**: maximal vertical runs of open (non-solid)
  cells, each with a solid floor `y0` and ceiling `y1` (the world top counts as
  open sky). A column can hold several spans: a river above a flooded tunnel,
  caves, stacked pools.
- Each span holds a water **depth** `h`, with `0 <= h <= y1 - y0`, stored as a
  continuous value in cell units (f32 is fine for runtime; see Persistence for
  canonical encoding). The surface is `s = y0 + h`.
- Two spans in 4-neighbour columns are **connected** when their open intervals
  overlap. The overlap window `[max(y0a, y0b), min(y1a, y1b)]` is the "pipe"
  between them. Diagonal neighbours are not connected.
- Water state is separate from solid material occupancy, per the existing
  ownership rules. Spans are derived data: rebuild them from the voxel volume
  for affected columns on terrain edits. Do not persist them as identities.
- Storage is sparse per **region** (suggested: 32 x 32 columns, matching the
  brick footprint). Only regions containing water, or adjacent to water that
  could flow in, are allocated.

### Step (virtual pipe model)

Every connection holds a persistent flow `q` in cells^3/s. Per substep `dt`:

1. **Head.** `H = s` for a span that is not full. For a full span (`h` at its
   ceiling, as in a flooded tunnel), `H` is a pressure head. Approximate it
   with a few Jacobi relaxation sweeps over the connected full-span subgraph,
   seeded from neighbouring free surfaces, and clamp it to at least `y1`.
   Approximate is fine. Sealed air is not modelled: a dead-end pocket floods.
2. **Flow update.** `q += dt * g * A * (Ha - Hb) / L`, with `L` = 1 cell and
   `A` = the wetted area of the overlap window (width 1 cell; height is the part
   of the window below the higher of the two water levels). Then apply damping
   `q *= (1 - k*dt)`. A connection with no wetted area has `q = 0`.
3. **Outflow limit.** If a span's total outflow over `dt` exceeds its volume,
   scale all its outgoing `q` down proportionally. This is what keeps volume
   non-negative and conserved exactly.
4. **Apply.** Compute every flow from the previous state, then apply the
   volume changes (Jacobi, not Gauss-Seidel). This keeps the step deterministic
   and independent of iteration order, and allows parallelism per region.
5. **Overflow and falling.**
   - Water above a span's ceiling does not exist; full spans pass extra water
     through their pipes via the head term.
   - Water leaving through a pipe into a lower span's column lands on that
     span's floor in the same step. Instant waterfalls are acceptable.
   - Water crossing the top of a column with open sky is impossible, since
     depth is capped at open sky.

Stability: the explicit pipe model needs `dt <= C * L / sqrt(g * h_max)` per
region. Use adaptive substeps per active region and report the substep counts.
Deep resting water sleeps, so deep-water substeps only cost while it moves.

**Why it cannot wall up.** Any head difference across a wetted pipe always
produces flow. The only stationary state is equal surface heights across every
wetted connection. Hydrostatic balance holds exactly in this formulation, so
staircase terrain cannot create parasitic currents. Thin films below a minimum
depth (suggested 1/64 cell) may stop moving to avoid endless trickles. That can
leave small puddles but never a wall, and it must be accounted, not deleted.

### Scale: regions, sleep and wake

- A region is **active** or **asleep**. It sleeps when, for N consecutive steps,
  every `|q|` and every per-step volume change is under a threshold. Sleeping
  regions cost nothing per tick.
- **Wake** a region on: a terrain edit inside it or touching its border
  columns; a neighbour region sending non-zero flow across the shared border; a
  source/sink being enabled; a body interaction (later).
- **Streaming.** A non-resident neighbour region is a frozen, *closed*
  boundary. It is never a drain, never empty space and never a permanent wall.
  When it becomes resident, both sides wake and exchange normally. Do not guess
  missing geometry.
- **Worldgen** seeds generated lakes and oceans at rest (equal surface per
  connected body), so they start asleep. An idle ocean must cost ~0 ms.
- **Threading.** Active regions step in parallel from an immutable snapshot of
  their own state plus border spans of their neighbours. Only the owning thread
  applies results at the tick boundary. Border flow is computed once per shared
  connection, so both sides agree and volume is conserved. Results must be
  bit-identical regardless of thread count (the existing vendored Rayon path
  already provides this; see `docs/dependencies.md`).

### Terrain edits and displacement

The order stays: terrain commit -> span rebuild for affected columns -> water
remap -> fluid step.

- **Remap rule.** Conserve volume exactly. When spans split or shrink, refill
  the new spans in that column bottom-up. If water no longer fits (a solid was
  placed inside water), push the excess to wetted neighbouring spans and then
  upward. This is the displacement behaviour the user chose in ENG-105: a block
  placed into water always succeeds. If displacement can't resolve locally,
  keep the excess in a per-region ledger that is released on later steps, and
  report it. Never delete water silently.
- **Excavation** that opens a new span or connection wakes the region; water
  flows in naturally.
- Edits use committed terrain only. Never flow through an intact wall using
  stale span geometry.

### Queries for later gameplay

The model directly answers: surface height at (x, z) for a span, submersion
depth of a point, and local flow velocity (`q / A` averaged over a column's
pipes). Swimming, buoyancy (displaced volume below the surface) and boats build
on these later. They are not part of this rewrite. Bodies are still ignored by
water, as they are today.

## Integration constraints (unchanged in spirit)

- Place it in `crates/spall_fluid` as a new module (suggested `columns`). It
  must not depend on GPU, window, network or game packages. Keep crate
  dependencies acyclic.
- **Authority.** The server owns water. `AuthoritativeWater` in
  `crates/spall_sim/src/water.rs` keeps its role and public shape where
  practical (`tick`, `frame`, `canonical_state`, `restore`, growth hooks may be
  deleted since columns have no domain box). `WaterFrame` changes from
  coarse-cell fractions to region/span data. Update all callers together.
- **Persistence.** Use a new explicit versioned DTO (`WaterState` v3 or a new
  per-region record) in `spall_protocol`. Encode depth as a fixed-point integer
  in little-endian, never f32 bytes or Rust layout. Per region: span floors and
  ceilings are not stored (they are derived from terrain), but each stored depth
  is keyed by its column and floor `y0` so restore can validate it against
  terrain. Pending-displacement ledgers and source/sink state are stored too.
  Recovery restores canonical depths. Never re-simulate from edits.
- **Replication.** Per-region baselines plus quantized depth deltas at a reduced
  rate, interest-managed by region. Clients render and may interpolate. There is
  no client fluid lockstep. Late join receives region baselines.
- **Sources and sinks.** The existing springs, drains and gated sources keep
  their semantics. Each one becomes a per-span volume rate.
- Water metrics stay structured: active regions, active spans, substeps,
  step time, volume, ledger, and skipped time (busy, stale, residency).

## Work items

Each item is one or more reviewable increments with tests. Keep `grid_mac` as
the live path until W3 passes. Do not delete it early.

### W0: decision, docs and fixture

- Update the water sections of `docs/architecture.md` (lines ~30-96, ~397-435)
  to describe the column model as the target and `grid_mac` as being retired.
  Add the decision to `docs/tasks.md` if the task list tracks water.
- Capture today's walled-water symptom as a reproducible fixture: a generated
  world seed and size, or a small hand-built basin with a dug side pocket.
  Record it against `grid_mac` as the failing baseline.

### W1: column solver, standalone

`spall_fluid::columns`: span extraction from `spall_voxel` bricks, connection
graph, pipe step, outflow limiting, full-span head relaxation, thin-film
handling, conservation accounting. All headless CPU tests, with fixtures small
enough for CI:

1. **Settles flat.** An irregular blob of water released on stair-stepped
   terrain reaches rest within a bounded simulated time. Afterwards, **no
   wetted connection has a head difference above 0.1 cell**, and no span holds
   water above a neighbour's open, lower surface. This is the anti-wall gate.
2. **Dam break.** Removing a dam produces a moving surge with a peak flow
   speed you report, downstream accumulation, and eventual equal levels.
3. **Canal.** Digging a channel between two pools at different levels brings
   them to a common level. Filling the channel back in stops exchange without
   deleting water.
4. **Thin wall.** A one-cell wall, including one crossing a region and brick
   boundary, holds indefinitely.
5. **Stacked water.** A flooded tunnel beneath a river: both are represented,
   and the tunnel stays full under the river's head.
6. **U-tube.** Two shafts connected by a flooded tunnel equalize approximately.
7. **Conservation.** Total volume plus ledgers stays constant to within f32
   rounding (declare the tolerance) through flow, edits and displacement.
8. **Determinism.** Identical results across thread counts and runs.

### W2: regions, sleep and threading

Region storage, active/asleep tracking, wake rules, parallel regional step,
frozen non-resident boundaries, worldgen seeding at rest. Headless bench
against the showcase world at 1024 and 4096 sizes (release build, report the
hardware):

- Resting world: water step ≈ 0 ms. Target is < 0.1 ms.
- Dam break with ~100k active columns: target is < 5 ms per 50 ms fluid step on
  the reference machine (Ryzen 7 9700X).
- Memory per stored column.

These are targets. Report measured numbers against them and don't round
missed targets into passes.

### W3: production swap

Replace the `grid_mac` engine behind `AuthoritativeWater`, including edits and
displacement, worker execution, sources and sinks, the new persistence DTO, and
replication baselines plus deltas. Port the existing water tests in
`crates/spall_sim/tests` (`authoritative_water`, `water_regions`;
`water_growth` is deleted or replaced, since there is no domain growth) and
`crates/spall_server/tests` (`water_persistence`, `water_replication`).
Acceptance:

- Save/restart and late join reproduce matching canonical water.
- The W0 fixture no longer walls up.
- An interactive play session on a generated world is user-checked. Report it
  as unrun until the user confirms.

### W4: rendering

Mesh water from span surfaces: a top quad at the exact surface height, and side
faces only where a neighbour's surface is lower or the neighbour is dry. This
should remove the bottom z-fighting, since water faces never coincide with
terrain faces. Client interpolation between replicated depths. Visual style is
open (`crates/spall_render/src/water.rs`, `crates/spall_client/src/water_look.rs`).

### W5: removal

Delete `grid_mac` (and `grid_mac/`), `component_fluid`, `phase_graph`,
`phase_pressure`, `phase_water`, `phase_fixtures`, `cut_cell`, the
`grid_fluid_scenario` binary, the `--grid-fluid-demo` and phase-demo client
paths, the obsolete water probes in `examples/sandbox/examples` and
`crates/spall_fluid/examples`, and the coarsening budget in `worldgen_scene.rs`.
Git history keeps them. `cargo xtask check` must pass.

## Explicit non-goals

No Navier-Stokes, no 3D vortices, no sealed-air pressure, no particle water, no
GPU solver, no body coupling in this rewrite, no second physics engine, no
per-voxel entities. Do not reintroduce coarsened domain boxes or global
cell budgets.

## Superseded

ENG-120 (domain growth), ENG-121 (MAC performance), ENG-123, ENG-124 and
ENG-125 (cut-cell geometry and transfer) are superseded by this design. The
uncommitted ENG-127 open-top leak fix in `worldgen_scene.rs` is obsolete under
W5. It may be committed as a stopgap for the live solver before W3.

## Tracking

Loopira project "Voxel Engine". Each ticket is blocked by the previous one.

| Work item | Ticket |
|---|---|
| W0 decision, docs and fixture | ENG-164 |
| W1 column solver, standalone | ENG-165 |
| W2 regions, sleep and threading | ENG-166 |
| W3 production swap | ENG-167 |
| W4 rendering | ENG-168 |
| W5 removal | ENG-169 |

ENG-120, ENG-121, ENG-123, ENG-124 and ENG-125 were canceled as superseded on
2026-10-10.
