# Validation history (removed grid-water solver)

Text relocated verbatim from `docs/validation.md`. Historical record of the removed grid MAC/PLIC water solver (ENG-103, ENG-105, ENG-122). Removed in ENG-169 (2026-10-11); current water is the stacked-column model in `spall_fluid::columns` (see docs/water-agent-handoff.md and ENG-169.md). Commands and demos named below no longer exist.

<!-- moved verbatim from docs/validation.md lines 3-130 -->

ENG-122 closeout (2026-10-05): user accepts current gameplay appearance/feel and
explicitly approved moving unfinished exact-geometry/recovery/precision gates to
ENG-123/124/125. PR187 is open. Historical measurements/failures below retain
their original meaning; experimental reconstructed support remains opt-in.

## Generated-world water default

The latest paced trench check passes flow at 512 but **fails at 1024**: the
worker advances at about 0.90x realtime, yet no water crosses the old domain
edge. This local water-only default has not passed large-world flow acceptance.
The 144-cell reproduction is `cargo run --release -p spall_fluid --example
shallow_shore_probe`; it currently exits 1 because a 0.20 m water layer above
an open lower trench has no pressure/velocity support. Run its known failing
drainage invariant with `cargo test -p spall_fluid --example shallow_shore_probe
-- --ignored --nocapture`. This is failed evidence until a physical fix lands.
Run `cargo run --release -p sandbox --example water_trench_probe -- 512`
or `-- 1024`; the latter currently exits 1 for the failed flow check. Exact
workload, limits and raw evidence are in `docs/reports/ENG-122.md`.

The shallow-shore probe now records mechanical-energy growth and includes a
wider partial-layer rest gate. The rest gate now passes normally after the
atmospheric surface-distance correction; only shallow drainage remains a
known failing ignored gate. Four 600-step analytical cases at two resolutions
and time steps also verify pressure, rest and conservation with nearly full
bulk cells. The new regression fails against the original distance lookup.
`SHALLOW_SHORE_FILMS=1` selects a diagnostic closure with zero-air accepted-
transfer momentum. It still fails rest, energy and the unchanged stability
budget, and is not a gameplay default. Run all its gates with
`cargo test -p spall_fluid --example shallow_shore_probe -- --include-ignored`.
Clear the environment variable before testing the default. Do not rerun the
large-world performance gate as acceptance until these small physics gates pass.

The selected performance default is adaptive coarse MAC water with original
velocity sampling, multigrid pressure, bounded conservative water transport,
freely displaced air and the current game water look. Air is atmospheric at
water surfaces even inside enclosed pockets; no gas mass or compression is
simulated. Normal `cargo xtask play`
needs no water-method flag. Seed-1 showcase uses 0.75 m cells at 512 and 1 m
at 1024, a 50,000-cell initial target, growth capped at twice that budget and
a 20 Hz worker. The maximum coarsening fallback can exceed the initial target;
domain limits still apply. Hand-authored scenes keep their declared resolution.

Historical trench measurements: median 15.0/25.5 ms per water step and
0.91/0.88 times realtime (512/1024). These are prior release measurements,
not a fresh benchmark or a guaranteed frame rate. Thin walls may leak, narrow
trenches diffuse, some shoreline seed water is dropped and reported, and
parasitic rest currents remain. These accepted default tradeoffs do not mark
ENG-122's full exact-geometry/hydrostatic gates passed. Sealed-air compression
is now deliberately outside the gameplay model; historical two-phase failures
remain recorded. The fine-phase
comparison and conservative momentum remain experimental options in the local
demo. Details and replacement criteria: `docs/reports/ENG-122.md`.

## Gameplay-water canal viewer (2026-10-05)

> Removed in ENG-169 (2026-10-11) with the grid solver: `--grid-fluid-demo` and
> its flags no longer exist. The text below is the historical record.

The existing `--grid-fluid-demo` now uses the gameplay water policy: original
velocity sampling, multigrid pressure, conservative water transport and freely
displaced air. It draws the shared smoothed water surface and underwater look
from presentation-only byte fractions. The quarter-metre fixtures and their
edit/displacement rules remain unchanged; this local viewer is separate from
the authoritative game and does not enable experimental reconstructed support.

Launch: `cargo run -p sandbox --features client --bin sandbox-client --release -- --grid-fluid-demo`.
1 selects closed reservoirs; C opens the canal; X closes it before breach;
B breaches the dam; R resets; Space pauses; 2/3 select canal/breach scenes;
4/5/6 select spreading basin/tunnel/resting basin. WASD/QE fly, click captures
mouse look, Escape releases it, and F12 saves `.local/screenshots/canal-water.png`.
Closing an occupied gate may reject placement, preserving water and geometry.
Reset preserves camera and restores initial water and gate state.

Bounded GPU check: `sandbox-client.exe --grid-fluid-demo --frames 60 --grid-fluid-demo-capture .local/screenshots/eng122-canal-gameplay-water.png`.
CPU checks cover all six scenes for 600 steps, volume plus outflow accounting,
open/close/breach/reset/pause, and rendering noninterference. No generated-world
performance or full ENG-122 geometry acceptance is claimed from these fixtures.

Room variant: add `--grid-fluid-demo-room` or press7. The lower room has a
solid0.25m ceiling and a presentation-only front-wall cutaway; the HUD reports
water below the floor. H opens a real floor tile as a positive control; R restores
it. CPU evidence:600 steps intact0m3;600 steps after opening1.200796876m3.
The shared renderer now honors wet-column bottoms so dry rooms below pools do
not receive false underwater tint. This resolved fixture does not validate
coarse large-world thin floors. Final checks/captures: reports/ENG-122.md.

## ENG-122 windowed water comparison

> Removed in ENG-169 (2026-10-11): `--phase-fluid-demo` and its flags no longer
> exist. The text below is the historical record.

Run from the ENG-122 checkout:

```powershell
$env:CARGO_TARGET_DIR='G:/Programming/voxel_engine/target'
cargo run -p sandbox --features client --bin sandbox-client --release -- --phase-fluid-demo
```

This local experimental viewer uses the same quarter-metre channel, low-dam
and full-wall fixtures as `phase_dynamics_probe`. It starts paused with the
original velocity sampling and **water-only freely displaced air**, explicitly
shown on screen. Space runs/pauses, N advances 10 ms, G cycles water-only,
incompressible air and compressible air, R resets, and 1/2/3 select the scene.
M switches momentum method in the air comparisons only. Conservative momentum
keeps G within the two air comparisons; returning to original sampling permits
water-only again. Switching resets and
pauses the water while preserving the camera for the same scene. Click for
mouse look, WASD/QE fly, Escape releases the cursor, and F12 saves
`.local/screenshots/phase-water.png`.

The existing compressible-air conservative channel rejects step 27; its last
accepted state and failure remain visible. The incompressible comparison is
separate evidence and does not satisfy that gate. Water uses the game's shared
smoothed surface, tint and underwater caustics. A presentation-only byte-fraction
snapshot feeds that renderer; it does not change solver fractions and is not an
exact reconstructed PLIC surface. This viewer is separate
from the authoritative generated game, with no save/wire activation.

For bounded graphical checks, use `--frames 60 --phase-fluid-demo-autoplay
--phase-fluid-demo-capture PATH.png`; add `--phase-fluid-demo-momentum` and/or
`--phase-fluid-demo-compressible-air` to select the initial configuration.
`--phase-fluid-demo-two-phase-air` explicitly selects the old incompressible
air comparison; momentum also selects an air comparison. No flag is needed for
the new water-only default. Gameplay construction, growth and recovery use it;
no canonical save/wire format change is made.
The viewer limits work to two fixed steps per frame and drops backlog;
displayed simulation time counts only accepted steps, not wall-clock time.

---

<!-- moved verbatim from docs/validation.md lines 616-629 -->

### ENG-103 grid-fluid basin candidate gate

For the isolated MAC feasibility prototype, report the raw maximum cell speed
for diagnosis, but evaluate basin stability using volume-weighted speed
statistics over water cells with fraction `C >= 1e-3`. In the final third of the
declared 3-second basin run, require weighted p95 <= 0.5 m/s and <=1% of
eligible water volume above 0.5 m/s, alongside the existing <=1 J kinetic-energy
rise, <0.15 m surface-p95 drift, and zero intact-solid crossings. Always include
the same speed metrics at `C >= 0`, `1e-6`, `1e-4`, `1e-3`, and `1e-2` so this
occupancy choice can be sensitivity-checked. `C=1e-3` is 0.1% of one 0.25 m
cell (15.625 mL); this gate changes only how basin residual motion is assessed,
not solver fractions or mass accounting. It is a feasibility-candidate
criterion and does not establish production readiness.

---

<!-- moved verbatim from docs/validation.md lines 655-714 -->

### ENG-103 grid-fluid basin candidate gate

For the isolated MAC feasibility prototype, report the raw maximum cell speed
for diagnosis, but evaluate basin stability using volume-weighted speed
statistics over water cells with fraction `C >= 1e-3`. In the final third of the
declared 3-second basin run, require weighted p95 <= 0.5 m/s and <=1% of
eligible water volume above 0.5 m/s, alongside the existing <=1 J kinetic-energy
rise, <0.15 m surface-p95 drift, and zero intact-solid crossings. Always include
the same speed metrics at `C >= 0`, `1e-6`, `1e-4`, `1e-3`, and `1e-2` so this
occupancy choice can be sensitivity-checked. `C=1e-3` is 0.1% of one 0.25 m
cell (15.625 mL); this gate changes only how basin residual motion is assessed,
not solver fractions or mass accounting. It is a feasibility-candidate
criterion and does not establish production readiness.

`--loss-percent 2` variant additionally asserts the clients observed motion
datagrams delivered out of `snapshot_seq` order.

The sibling `body-rest-on-structure` fixture covers a **detached body coming to
rest on remaining structure**: it severs the seam column so the cross-brick beam
detaches, excavates only the floor ends *outside* the beam's span, then runs the
server with `--await-body-settle` so physics keeps stepping past edit-quiescence
until the beam sleeps. It passes only if — on top of the hash-agreement and
replay checks above — the server reports the detached beam at rest: every
detached body asleep, final linear speed `<= body_settle_speed_epsilon_m_s`, its
origin held still (`< 1 mm/tick`) for `>= body_settle_min_stable_ticks` ticks,
and end-of-run contact penetration `<= body_settle_max_penetration_m` (it settled
*on* the floor, not through it). CPU-side proof:
`spall_sim` `body_rest_on_structure` integration test.

Neither fixture yet covers the full 60-second / 10-requests-per-second
sustained two-client workload or the 64-brick collapse stress case; those
remain explicit follow-up evidence pending a sustained-request-rate driver
(the oversized-split path itself is available — T17 / ENG-64). The full
`64 x 32 x 64 m` envelope itself is built (ENG-62 / T11a increment 3):
`spall_sim::fixtures::g1_full_envelope_setup` (`--scene g1-full-envelope`) is
real, resident, walkable terrain across the whole footprint — not just
isolated structures — plus a hollow tower/bridge spanning brick boundaries, an
excavatable ramp, and a moving hollow test-volume body, standing up and
stepping well inside the 60 Hz tick budget. `cargo xtask scenario --name
g1-full-envelope` runs it networked (two real `--late-join` clients); `cargo
xtask capture --scene destruction` (below) renders the authoritative cut
sequence offscreen through the T12 pipeline with real GPU pass timings, and
`--scene destruction-networked` renders the same sequence from a real
network-replicated client instead of the authoritative sim. Measured results:
`docs/reports/G1.md`.

For overload tests, requests beyond admission capacity may return busy. The report must distinguish requested, rejected, queued, and committed counts. Rejecting every expensive action does not satisfy the gate: all named mandatory edits must complete.

**Commit latency + admission counts (ENG-62 / T11a increment 1).** `serve`
(`ServeSummary` v3) measures server commit latency — admission of an
`ActionRequest` to commit of its transaction — and reports a nearest-rank p95
per commit shape (single-brick commit, ordinary structure split, large
collapse) plus the requested / rejected / queued / committed breakdown. A
scenario file's optional `latency_targets` block makes `cargo xtask scenario`
assert each exercised bucket's p95 against the G1 targets (`<= 100 ms` /
`<= 500 ms` / `<= 2000 ms`); the session summary's `admission` object carries
the counts, the measured p95s, and `latency_targets_met`. The large-collapse
bucket stays unexercised until the oversized-split (T17 baseline-blob) path
lets a big detach commit — see `docs/reports/G1.md`.

---

<!-- moved verbatim from docs/validation.md lines 1138-1169 -->

## ENG-105 authoritative water integration

Run `cargo xtask check` for formatting, strict workspace clippy with all targets
and features, and the complete CPU test suite. Targeted evidence commands:

```powershell
cargo test -p spall_protocol water -- --nocapture
cargo test -p spall_fluid boundary_ -- --nocapture
cargo test -p spall_sim --lib water::tests -- --nocapture
cargo test -p spall_sim --test authoritative_water -- --nocapture
cargo test -p spall_server --test water_replication -- --nocapture
cargo test -p spall_server --test water_persistence -- --nocapture
cargo test -p spall_store --test durability -- --nocapture
cargo test -p spall_server budget_bounds_sustained_egress -- --nocapture
cargo test --release -p spall_sim --test water_regions -- --nocapture
```

The three-region run uses three complete ENG-103 scale-1 reservoirs, each
24 x 12 x 8 fluid cells, at 0.25 m resolution, fixed 1/60 s outer dt, eight
maximum substeps, 600 ticks. The two-client QUIC run joins its second client
during flow, cuts the dam with an actual action, and resets all three regions.
Protocol tests independently inject gaps, reordered/duplicated bricks, malformed
headers and bounded decompression overruns, then install repair. Exact save
recovery tests include retained volume and an interrupted journal commit; store
tests reject missing/corrupt canonical rows and verify schema-1 copy migration.

Report measured timings and application bytes separately from targets; hardware
render captures, IP/QUIC retransmission bandwidth, a long production soak and
seamless cross-grid exchange are not established by these bounded CPU tests.
See [ENG-105 evidence](ENG-105.md).

---

<!-- moved verbatim from docs/validation.md lines 1221-1473 -->

### ENG-122 reconstructed liquid support gates (2026-10-04)

Run `cargo test -p spall_fluid --lib cut_surface --release` for centroid and
positive-fragment geometry, and `cargo test -p spall_fluid --example
shallow_shore_probe --release` for unconditional opted-in support drainage,
5% energy, volume, bounds, rest and full-wall regressions. For the identical
600-step diagnostic evidence, set `SHALLOW_SHORE_SUPPORT=1` and run `cargo run
-p spall_fluid --example shallow_shore_probe --release`; its exit status covers
both shallow flow and energy. Unset the variable for the gameplay baseline.

The previously ignored rest gate is repaired and enabled:
`cargo test -p spall_fluid --lib reconstructed_support_preserves_nearly_saturated_surface_rest --release -- --nocapture`.
Eight 600-step cases pass, with peak speed 9.15e-12 m/s versus 1e-7 and maximum
volume error 3.31e-13 m3 versus 1e-10. Existing gameplay rest regressions remain
enabled. Test unchanged reference scenes using `cargo run -p spall_fluid
--example phase_dynamics_probe --release -- <channel|low-dam|full-wall> 600
water-support`. All three pass 600 steps; channel peak face speed is 5.08 m/s.
Accepted-transfer predictor conservation/energy and tiny moving-liquid
regressions run in `cargo test -p spall_fluid --lib cut_surface --release`.
This also covers 600-step exact equivalence under unchanged boundary refreshes,
preconditioner symmetry/positivity on closed and tiny graphs, and the extracted
anisotropic subnormal fragment from the 512 trench. Real-geometry invalidation
and trapped-water release remain enabled regressions in the full fluid suite.
Build `cargo build -p sandbox --example water_trench_probe --release`, then run
the built example sequentially with `512 water-support` and `1024 water-support`.
Omitting `water-support` retains the gameplay baseline. Keep other builds and
owned demos stopped during the timed runs. The diagnostic configuration survives
domain growth. Original seed, cuts, vegetation, pacing and limits are unchanged.
After pacing, an untimed cloned final grid must accept one 50 ms step; this
prevents earlier flow from masking later stability skips. 1024 exits 0, but
512 exits 0 in two repeated runs: four final substeps within the budget of eight.
The small force-free pressure-guess regression covers 1e-40, 1e-120 and 1e-300
fractions; global flux convergence alone used to generate 66.7 m/s motion.
Pass `trace` after `water-support` for wet-face pressure diagnostics; trace
timings are not performance acceptance. Occupancy
filters in the trace are diagnostic only, never CFL acceptance. A passing
workspace check and these motion passes do not authorize gameplay promotion while
512 runs at about 0.54x real time. Viewer/GPU,
local momentum accuracy and complete memory/performance acceptance remain open.
No workload, threshold, water amount or velocity cutoff was changed.
Evidence and remaining risks: docs/reports/ENG-122.md.

### ENG-122 worker profile (2026-10-04)

The unchanged paced trench example also emits `skip_reasons` and stage timing
sums/medians/p95s over accepted worker steps. `busy`, `stale` and `stability`
count skipped fixed fluid steps; `residency` counts paused owner ticks. Totals
survive growth. Stage timings are microseconds except the explicitly labeled
substep entry. Counters are observability, not relaxed motion gates. Controlled
worker tests cover busy/stability rejection, unchanged committed state, growth
carry-over, stale placement/ledger protection and residency accumulation.
Closed gauge tests cover independent offsets, component means, tiny values,
preserved pressure differences and untouched open rows. The original final
50 ms shadow advance, flow/conservation/rest/energy gates remain required.
See reports/ENG-122.md and ENG-122-worker-profile-* for final evidence.

### ENG-122 transport speed/stability distinction (2026-10-04)

`stage_profile_us_except_substeps` now includes reconstruction, face-flux,
limiter, paired-repair, interface and momentum timings. Limiter timing excludes
repairs; stage totals cover accepted steps only. Original paced gates and all
positive-water support rules are unchanged. Faster transport is experimental:
one optimized 512 run fails the final 50 ms stability gate after 33 s of fluid
time, despite conserved water, flow and converged accepted pressure substeps.
An existing dry-cell velocity also limits CFL; occupancy-filtered diagnostics
are not acceptance. The 1024 run passes. Earlier both-size motion passes apply
to their earlier source/worker trajectories, not this increment's acceptance.

The optional third argument `tail` runs at most 100 additional 50 ms steps on
an untimed clone of the committed final grid. Its separate continuation record
credits no owner fluid time or performance, and its failure exits nonzero.
Example: `water_trench_probe.exe 512 water-support tail`. It is a bounded
reproduction, not a replacement workload or an increased substep limit.
A prior-dense numerical build from a4c0a04 plus instrumentation/tail passes
100 extra cloned steps to 35.85 s; this is a different paced worker trajectory,
not proof that the optimized failed state is stable. New small tests cover dry
padding/zero-lane equivalence, tiny positive masses, pinned dense numerical
compatibility and independence of water transfers from discarded donor samples.
Current raw evidence and integration decision: reports/ENG-122.md.

ENG-122 exact failing-step replay (2026-10-04): optional
`SPALL_WATER_REPLAY_PATH=.local/coupling-512.water-replay` with
`water_trench_probe.exe 512 water-support` captures the final numerical grid and
last CFL-admissible precursor. It reports `timing_acceptance:false`; cloning
perturbs pacing. `water_step_replay.exe FILE STEPS [trace]` is bounded to 1000
50ms steps and accepts only version1 strict reconstructed-water snapshots;
`SPALL_WATER_REPLAY_OUTPUT` saves an accepted result for byte comparison.
Full precursor32.95s ->33.00s matches all7070989 captured bytes and then fails
nine-versus-eight substeps. Run the retained reduced diagnostic with
`water_step_replay.exe crates/spall_fluid/fixtures/eng122-trench-crop-v1.water-replay 100`:
it currently accepts two steps then fails ten-versus-eight. New walls alter
pressure connections; it is not original trench acceptance. Its explicit ignored
acceptance test can be run with `cargo test -p spall_fluid --lib
captured_trench_crop_advances_without_velocity_instability --release -- --ignored`;
this currently fails, remains open, and is not counted as a CI pass. Three replay
invariants validate bitwise next-state preservation, unchanged rejection state
and malformed input rejection. See reports/ENG-122.md and coupling-replay evidence.

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
