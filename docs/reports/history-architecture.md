# Architecture history (removed grid-water solver)

Text relocated verbatim from `docs/architecture.md`. Historical record of the removed grid MAC/PLIC water solver (ENG-103, ENG-105, ENG-122). Removed in ENG-169 (2026-10-11); current water is the stacked-column model in `spall_fluid::columns` (see docs/water-agent-handoff.md and ENG-169.md). Commands and APIs named below no longer exist.

<!-- moved verbatim from docs/architecture.md lines 47-114 -->

ENG-103 introduces `spall_fluid -> spall_core, spall_voxel` as a CPU-only
water crate. Its dense two-phase MAC/PLIC solver uses variable-density
pressure projection, multigrid preconditioning and isothermal sealed air.
Evidence is in [`docs/reports/ENG-103.md`](ENG-103.md).

ENG-122 changes authoritative gameplay to water-only MAC/PLIC flow with freely
displaced air. Air has atmospheric pressure at reconstructed water surfaces,
including enclosed pockets, and no mass, compression or back-pressure. The
pressure system contains liquid pressure samples only; water in partial cells
remains in conservative transport. Three layers of extrapolated water velocity
support surface transport. Legacy two-phase models remain explicit experiments.
Atmospheric pressure distances use a valid intersection in the air-centred
cell first, then a validated wet-cell fallback. Matrix assembly and velocity
correction share that distance; tiny bulk fraction deficits do not select a
different boundary or change canonical water amounts.
Construction, domain growth and canonical recovery use the same water-only
policy. Canonical water amount/trapped ledgers and their versioned wire format
are unchanged; loading an older save uses the current numerical policy (air
mass and runtime pressure/velocity were never canonical state). This deliberate
model change supersedes sealed-air realism for gameplay, not terrain ownership,
worker validation or mass/recovery invariants.

The ENG-122 shallow-film closure and zero-air dual momentum trial are an
explicit diagnostic opt-in only (`set_experimental_surface_films`). They fail
energy, stability and partial-layer rest gates and are not selected by any
gameplay constructor, growth or recovery path. Its half-cell inertia support
does not yet match reconstructed liquid geometry. The existing water-only
policy still has unresolved shallow drainage. Its wider-pool rest failure was
repaired by the atmospheric surface-distance correction;
see `docs/reports/ENG-122.md` before treating numerical check success as
hydraulic acceptance.

ENG-105 lets `Simulation` own up to eight explicitly authored, disjoint bounded
water domains, with at most 4,194,304 fluid cells across the group and 32 million
voxel cells per domain. A connected pressure system belongs in one domain;
independent domains do not exchange water across their declared sides. Side
boundaries are authored closed walls; the configured open top has accounted
outflow. This bounds the integration without introducing an artificial wall
where an in-domain terrain brick is unloaded: any unknown brick suspends the
whole domain, preserves its mass, and requires a fresh boundary when residency
returns. It does not implement seamless fluid exchange across streaming grids.

The order is terrain commits, boundary refresh/displacement, accepted water
step, then Rapier. Placement displaces through face-connected capacity, retaining
any excess at the source in an explicit trapped-volume ledger. Reopening space
redistributes the ledger without crossing walls or deleting mass. Water and
voxel occupancy stay separate. Rapier remains the sole body solver; bodies are
currently ignored by water and receive no buoyancy or drag.

Inline water steps at the fixed 60 Hz tick. Worker mode solves one immutable
clone at a time, with a fixed fluid dt. Only the owner can install a result at a
tick boundary after validating boundary revision and spring rate. Busy, stale,
unresident or stability-budget rejected steps report skipped fluid time; server
dt never changes. Quiet regions sleep after 60 accepted steps below 1e-5 m/s
face speed, without active sources or drains. Committed boundary changes,
residency restoration and active gated springs wake them; flowing cells within
one pressure domain remain part of that domain's solve.

Clients receive quantized presentation snapshots and changed-brick deltas;
normal clients never advance the authoritative solver. A bounded token bucket
paces ordered updates, with periodic full repair and independent late-join
baselines. Canonical exact fractions, ledger and source/outflow accounting are
checkpointed and journaled atomically with the terrain/pose batch. Restart
preserves these amounts and resets velocities, pressure, sleep and caches to
rest. See [`docs/reports/ENG-105.md`](ENG-105.md).

ENG-104's `sandbox-client --grid-fluid-demo` viewer was removed with the grid
solver (ENG-169).

---

<!-- moved verbatim from docs/architecture.md lines 432-471 -->

### ENG-122 reconstructed support diagnostic (2026-10-04)

The opt-in `MacGridWorld::set_cut_surface_support` backend constructs PLIC
liquid centroids, wetted Cartesian faces and embedded free surfaces, with
reduced-potential pressure and accepted-transfer water-only momentum. It has
no air mass or hidden pressure anchors. Gameplay keeps the existing backend:
shallow drainage/energy, nearly saturated bulk rest and the 600-step dynamic
channel pass. Embedded normal corrections now survive between steps in a
derived cell velocity, transported with the exact accepted water transfers
using the same convex water-only upwind solve as momentum. Full-cell samples
come from corrected Cartesian velocities; initialization uses liquid centroids.
Actual boundary changes invalidate this derived state. Identical boundaries
preserve the predictor and pressure; trapped-water release still runs when
needed. Worker revisions continue to reject stale jobs. It is not additional fluid
mass, a second physical momentum ledger or canonical save state. The existing
paired path repair runs before low-order predictor validation as well as after
FCT. Previous physical pressure seeds reduced-potential CG, including active-set
re-solves, within the original total iteration budget. Closed-wall
ghost samples retain their vertical level; complementary-air reconstruction
resolves nearly full cells. Embedded and exposed Cartesian closing interfaces
use bounded displacement flux with unknown contact pressure instead of
unconditional atmospheric pressure. Actual exposed polygons determine their
centroids. This bounded active-set prototype is not full dynamic acceptance.
The diagnostic uses symmetric Gauss-Seidel preconditioned CG on the actual
liquid graph, independently of legacy multigrid. Apparent convergence is checked
against the true matrix residual; additional correction uses the remaining
original budget. Each non-null positive liquid row must also satisfy a
diagonal-scaled potential defect criterion: a tiny row cannot pass with a
large stale pressure just because its global flux residual is small. Symmetric
local corrections consume that same budget. Anisotropic subnormal wedges use a scaled analytic offset and
per-axis normalized centroid weights, without deleting positive fragments.
Retained flux and three cell-velocity arrays are counted and budget checked.
Whole transient geometry/RSS remains unmeasured. The prior pressure checkpoint
passed original paced trenches in both sizes: 1024 at 0.89x real time and 512
at about 0.54x. The subsequent faster transport diagnostic fails a later 512
final-state stability gate; it remains unaccepted (see the checkpoint below). The 512 performance, full transient
memory and local momentum-accuracy gates remain open. No default promotion
follows from the passing motion gates. See docs/reports/ENG-122.md for the
regularized inertia, unchanged gates and precise integration blocker.

---

<!-- moved verbatim from docs/architecture.md lines 628-837 -->

### ENG-122 worker observability and pressure gauge reuse (2026-10-04)

Water tick metrics distinguish fixed-step skips for a busy worker, a stale
revision/rate result, and a stability-budget rejection; residency pauses count
owner ticks separately. Their totals survive replacement during domain growth.
These counters change no scheduling, revision validation or step acceptance.
The reconstructed diagnostic pressure solve builds closed-component row lists
once per contact active set. Gauge applications reuse those lists, preserve
per-component row summation order, and leave atmospheric/open rows untouched.
The first matched 512 profile spends similar accepted-step time in pressure and
transport and shows no stability rejections. This allocation reduction does not
establish a meaningful speedup or full memory acceptance. Transport profiling
is the next ENG-122 item; gameplay backend and numerical limits are unchanged.
See docs/reports/ENG-122.md for final runs and checks.

### ENG-122 transport profiling and experimental sparse sweeps (2026-10-04)

Accepted-step transport metrics separately time reconstruction, face transfers,
limiting, paired repair, interface transport and staggered momentum. The baseline
512 profile places 46% of transport time in momentum and 25% in the interface
predictor, which share the water-only implicit upwind solve. Its iterative work
lists include every non-null mass/incoming-transfer row and nonzero incoming
lane. Only exactly null rows and exactly zero transfers skip redundant sweep
work; original validation, tolerance, iteration budget and reaction/outflow
ledgers remain. New list capacities are included in scratch accounting. The
reconstructed path also bypasses ordinary PLIC/donor computations that were
replaced by its paired physical transfers. This is no geometry/topology or
water-volume approximation. A pinned dense test oracle checks bit-identical
velocities, convergence and ledgers across 96 small scenarios. Runtime/canonical
DTOs, worker authority and the gameplay default remain unchanged.

Measured transport cost falls, but a faster 512 worker run stalls at 33 s of
fluid time with nine required substeps against eight. A dense-reference clone
continuation from a different slower-worker trajectory passes through 35.85 s,
so the failure cannot be attributed solely to elapsed fluid time. The diagnostic
is unaccepted; no CFL/occupancy filtering, speed clipping or budget increase is
used. Next ENG-122: reproduce the edit/growth-dependent high Cartesian/interface
velocity state and stabilize physical pressure/momentum coupling before claiming
a performance gate or promoting this backend. Exact runs and remaining risks
are in docs/reports/ENG-122.md.

ENG-122 exact-state diagnostics (2026-10-04): the failing 512 transition now
replays byte for byte from an explicitly versioned, little-endian numerical
snapshot that retains warm pressure and interface predictors. This diagnostic
is separate from canonical saves/wire/owner installation, bounded to one million
cells, and accepts only the strict reconstructed water-only model. Rebuilt cut
fluxes and unused legacy caches are omitted. Optional trench capture perturbs
pacing and reports no timing acceptance. A tracked 100-cell crop fails its third
50ms step, but its new domain walls change pressure connections and it is never
original workload acceptance. The coupling instability and unchanged full
512 gate remain open; no gameplay/default/numerical budget changes. A trial
removing only the embedded-cap inertia floor failed full-state pressure
convergence and was dropped. Next ENG-122: physically matched pressure/momentum
response and pressure conditioning using the crop plus exact full replay.

ENG-122 scaled pressure/surface-normal checkpoint (2026-10-04): embedded normal
predictors derive from accepted-transfer canonical momentum, with actual cap
half-cell inertia and diagonally equilibrated equivalent SPD pressure equations.
Physical residual, local potential/velocity error, closed gauge and original
iteration/substep/CFL budgets remain enforced; every positive fraction stays in
the model. Physical warm pressure and owner/save/wire contracts remain unchanged.
The old full replay and numerical-overflow snapshot advance 100 steps, but the
unchanged fresh 512 trench rejects at 12.10 s (9 versus 8 substeps); 1024 passes
52.20 s. Reconstructed support remains diagnostic, ENG-122 stays in progress.
The crop binary has an open top, correcting the preceding closed-crop wording;
CI now checks accounted exterior water plus a separate sealed 100-step variant.
Next: consistent Cartesian patch pressure/face momentum in vanishing fragments;
original fresh gates, local momentum accuracy and full scratch peak remain open.
See docs/reports/ENG-122.md for provenance and measured results.

ENG-122 donor-consistent cut-row boundary checkpoint (2026-10-04): exposed
Cartesian patches of partial rows cannot donate nonexistent upstream liquid;
zero transfer is enforced inside the existing pressure contact solve. Wet
edges and embedded cap displacement retain their signs; full-row atmospheric
boundary retreat remains free and has an explicit uniform-translation invariant.
All positive fractions, numerical budgets, canonical amounts and owner/default
contracts remain. Both original fresh 512/1024 trenches pass final advance and
flow with no stability skips, ratios 0.8768/0.8661 and medians 33.176/34.926 ms.
Water delivered beyond the 1024 old edge falls to 1.83481 m3; motion thresholds
pass, local pressure/momentum/interface accuracy remains unaccepted. A removed
Cartesian inertia-floor trial was rejected and fully dropped. 96 release fluid
and 6 shallow tests pass; full checks and evidence in docs/reports/ENG-122.md.
Keep ENG-122 diagnostic/in_progress. Next: boundary momentum/pressure-work and
interface-motion accuracy, 1024 delivery comparison, complete scratch peak and
sustained/game-window gates before promotion; exact capacity/recovery remain.

ENG-122 wet-donor pressure-work checkpoint (2026-10-04): supersedes the prior
blanket zero contact with an actual neighboring-water per-patch inflow bound;
aggregate paired transport bounds remain authoritative. Uniform interior
partial-water translation now retains local momentum/energy and zero pressure
work in both directions (parent loses 13.20% x momentum). Both unchanged fresh
512/1024 trenches pass final 50 ms advance with zero stability skips; delivery
42.11817/4.97810 m3, ratios 0.8791/0.8753, medians 31.370/33.750 ms. All positive
fractions and original budgets/default/owner/save/wire contracts remain.
Signed-interface/phase-flux separation trial fails sealed crop 9/8 substeps and
is dropped. Local moving-tiny-volume accuracy is not accepted: channel's
normalized divergence reaches 1.38816e237 /s despite bounded speed/accounting.
97 release fluid/6 shallow checks and replay/reference gates pass. See
reports/ENG-122.md for exact commands, full-check status, risks and provenance.
Next unblocked task remains ENG-122 interface/phase-transfer accuracy, complete
scratch peak and sustained/game-window gates; exact capacity/recovery remain.

ENG-122 stationary-cap and flux-audit checkpoint (2026-10-05): free caps with
exactly zero net Cartesian outflow use the exact stationary continuity normal,
within the existing pressure-response accuracy. Nonzero outflow and contact
caps retain their pressure solution/displacement; all positive amounts and
original budgets/default/owner/save/wire contracts remain. A 27-cell regression
fails on parent normal drift1.38778e-17 m/s and passes five positive amounts
through subnormal C=bits7; tracing has exact three-step state/600-step metric
noninterference coverage. Both original fresh 512/1024 trenches pass final50ms
and flow with zero stability skips, ratios0.8991/0.8963, delivery42.11817/4.98630
m3. No causal speedup or whole-memory saving claimed. Giant normalized channel
divergence stays1.38816e237/s: trace identifies 1.00974e-28 m3/s cancellation
residue at C4.65533e-264 (relative1.2455e-16), not complete local accuracy.
Broader cap reconstruction fails the existing closing-gap accuracy and is
dropped. Release fluid99/shallow6 and reference/replay gates pass. Exact
commands/evidence/full-check status in reports/ENG-122.md. Keep experimental
and in_progress; next unblocked ENG-122 is nonzero-flow/closing-interface
pressure-normal/accepted-momentum consistency, then full scratch and sustained
windowed gates; exact capacity/thin-wall/recovery/physical acceptance remain.

ENG-122 closing-cap audit checkpoint (2026-10-05): diagnostics now examine all
free atmospheric caps' pressure/Cartesian-continuity normals, distinct from the
worst volume-normalized row. The unchanged 24-cell/600-step air-gap reproduction
has peak mismatch 9.98263e-5 m/s; channel peaks 0.0312354 m/s on a nearly full
cell with a tiny air cap. A stronger local residual gate passes the small test
and 99 release fluid invariants but exhausts original channel/world budgets;
trial dropped, numerical physics/default and all positive amounts preserved.
New full600-step trace noninterference test and identical46 non-timing channel
properties pass; release fluid100 pass. Full-check evidence in reports/ENG-122.md.
Original fresh trenches are prior ed024a9 evidence, not rerun measurements in
this diagnostics-only increment. Remains experimental/in_progress. Next
unblocked ENG-122: cancellation-resistant closing-interface pressure/normal
consistency, then accepted momentum, scratch and sustained windowed gates;
exact capacity/thin-wall/displacement/recovery and physical acceptance remain.

ENG-122 pressure-impulse checkpoint (2026-10-05): reconstructed-support pressure
jumps retain affine/product roundoff and use fused velocity impulses. Verified
residuals evaluate projected Cartesian faces with range-safe equilibrated caps,
within original stopping/contact/iteration/substep/CFL budgets. A real 1 Pa edge
force disappears in the original expression; final preserves it and exact affine
rest in both directions. Private NaN pressure norm no longer reports a finite
value; regression covers overflow/underflow and nonfinite rows. All positive
amounts and default/owner/save/wire behavior preserved. Release fluid102/shallow6,
reference600/600 each and exact1000/100/100 captured replays pass. Original fresh
512/1024 trenches pass final50ms with zero stability skips; ratios0.8948/0.8663,
medians24.076/27.961ms, delivery42.02757/4.88146m3. No causal speedup claimed.
Closing-cap accuracy remains open (gap peak1.00112e-4m/s, channel0.0476035m/s,
all audited channel projections exceed1e-8); normalized divergence4.63193e73/s
still not physical acceptance. Trace remains before accepted transport; new
projection_residual diagnostic explicitly labels its evaluation. Exact commands,
full-check result and evidence in reports/ENG-122.md. Next unblocked ENG-122:
shared accurate pressure-correction/flux and accepted-momentum consistency for
closing caps, then scratch and sustained windowed gates; exact capacity/thin
walls/displacement/recovery remain. Keep experimental and in_progress.

ENG-122 face-summation checkpoint (2026-10-05): optional cap audit separates
compensated Cartesian-flux summation loss from remaining pressure/normal error,
using the same rounded installed patch products on the originally selected
worst cap. No solver/default/amount/owner/save/wire changes or new retained
arrays. Analytic cancellation test and existing 600-step exact trace-state
noninterference test cover the diagnostic. Channel600/600 and exact512
precursor1000/1000 pass. All599 selected channel caps still exceed1e-8 after
compensation; worst ordinary0.04760348394m/s changes only2.0628964e-7m/s,
leaving0.04760369023m/s. Thus summation alone is insufficient. Shared split
pressure/compensated geometric-flux/stricter cap-response trial was rejected:
channel600 and release102 pass, but unchanged512 precursor accepts78/1000,
then exhausts400 iterations with global residual5.9167570e-15 and cap-response
defect-2.1260983e15m/s at C=1.0245860e-140. This is a convergence defect, not
an accepted installed speed. Trial/debug prints removed. No amount filtering,
extra budgets, velocity clipping or accuracy exemptions. Fresh paced trenches
are prior checkpoint evidence; no rerun/windowed/physical acceptance claimed.
Full xtask check passes:1218/0/63 across137 suites, including strict Clippy;
release fluid103 and shallow6 pass. Exact commands: reports/ENG-122.md. Next unblocked
ENG-122: shared local pressure/flux agreement for nearly closed caps and tiny
moving fragments within original budgets; then accepted momentum, scratch,
sustained windowed gates. Capacity/thin-wall/displacement/recovery remain open.
Keep reconstructed support experimental and ENG-122 in progress.
ENG-122 incremental pressure/flow checkpoint (2026-10-05): opt-in reconstructed
support now solves fresh pressure increments, applies them to compensated patch
velocities/fluxes, verifies actual flow residuals, then resets the increment.
Cold-starts each contact solve; rounded pressure remains a cache/diagnostic.
Existing global/local pressure/1e-8 response checks remain; every positive free
cap additionally verifies net flux/area within5e-9 m/s before f64 installation.
Original400/1000 iterations,32 contacts,8 substeps/CFL, topology, amounts and
owner/save/wire/paired transport/default contracts remain. No floors/anchors/
clipping. Krylov keeps progress through global convergence; zero/nonfinite dots
use budgeted relaxation. Batching avoids costly every-iteration face work.
Exact stationary-cap rule uses verified projected Cartesian sum; rounded output
can falsely sum to zero. Analytic sub-ulp transfer/cap-normal regression protects
this distinction; truly zero flow retains exact stationary normal.
Original final512/1024 trenches pass3276/3270 ticks and final50ms, zero stability/
residency skips, ratios0.8984/0.8855, medians25.887/32.156ms, delivery42.36514/
5.23509m3. Sequential paced runs without concurrent owned builds/simulations.
No controlled speedup or whole-memory saving claimed. Cold flow workspace
conservative partial bounds: final shadows268608/374720 bytes (other projector
allocations excluded). Library104, shallow6, references600 each and exact
replays1000/100/100 pass. Closing-gap600 stored error3.97882e-9m/s meets1e-8;
channel internal error4.99853e-9 meets5e-9 but stored error6.70132e-6 stillfails
1e-8 on587/599 selected rows. Unfiltered normalized divergence1.21397e104/s
remains enormous; no complete physical acceptance or default promotion.
Full-check result/exact commands/initial replication failure and retries in
reports/ENG-122.md; failed scalar/unbatched/rounded-zero trials preserved.
Next unblocked ENG-122: preserve projected small transfers through stored face
aggregation and accepted momentum, then full scratch and sustained/windowed
checks. Capacity/thin walls/displacement/recovery remain open. In progress.
Final ENG-122 incremental-flow validation: cargo xtask check1219 passed/0 failed/63 ignored
across137 suites, including formatting and strict workspace Clippy. Initial dam-replication
admin-ack failure and isolated retry pass are preserved; no networking/fixture/budget changes.

---
