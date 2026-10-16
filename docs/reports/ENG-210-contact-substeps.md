# ENG-210 fast animated-part contact response candidate

This report adds a bounded experiment to the articulated-rig feasibility work.
It uses the existing `spall_physics::PhysicsWorld` and Rapier 0.35.3 pipeline.
There is no second solver, hand-authored impulse, extra anchor, or change to the
source-cell collider/query owner. `PhysicsConfig::solver_substeps` defaults to
1; experimental values are bounded to 1..=64. On each outer `step`, every
dynamic body and the entire world pipeline advance `dt / N` exactly N times,
so total elapsed simulation time remains the configured outer `dt`. Pending
position-based kinematic targets interpolate linearly in origin and by
shortest-arc quaternion rotation. Public pose tokens/query epochs advance once
after the outer boundary, after all N solver steps.

Before any authored pose or root target is staged, the world bounds the maximum
surface travel of every changed kinematic part as origin displacement plus
shortest-arc angle times its collider's local enclosing radius. A request is
rejected with `WorkBudget` if that bound exceeds 2.5 cm per configured solver
substep (with a 1 micrometre floating-point allowance). Root+clip combined
updates preflight the complete set before mutating any target. This is an
admission bound, not a claim that finite substeps exactly solve continuous
collisions or provide a population-performance guarantee.

## Preserved original failure and candidate result

The original unswept 0.5 m / one 16.6667 ms step was rerun unchanged. Native
Voxels had one contact pair but zero positive impulse with CCD both off and on;
Merged Cuboids had no contact pair and zero impulse with CCD both off and on.
This remains a failing original case. Merely enabling dynamic-body CCD does not
fix a kinematic target that moves through the pair between discrete steps.

With the same geometry, mass, 0.5 m displacement and 16.6667 ms elapsed time,
the same-world 20-substep candidate produced finite contact response in all
four representation × dynamic-CCD variants. Accumulated positive normal
impulse was 483.4784 N·s; measured dynamic momentum was 483.47845 kg·m/s,
velocity 30.942623 m/s, final x 0.25906596 m, mass 15.624999 kg, and kinetic
energy 7487.4575 J. The last substep had no active contact pair, so impulse is
aggregated across substeps rather than sampled only at the final state. Because
the prescribed kinematic surface injects work, this is not an energy
conservation result. Pipeline time was 2.90–3.60 ms (wall time 2.92–3.63 ms)
in this debug test run. These numbers are feasibility evidence, not an
accepted runtime target.

At 20 substeps, the unchanged 0.6 m / 60 outer-step / 1 second slow test gave
both exact collider representations identical results: 9.912996 N·s accumulated
impulse, 0.6344317 m/s probe velocity, 0.38361242 m final x, and pose token 60.
It used 1,200 shared-world solver steps. A zero-motion case also produces zero
impulse and zero velocity. Over-budget 0.8 m linear and 180-degree pose targets
are rejected before mutation; current source query and token remain unchanged.

The earlier ENG-209 slow baseline remains preserved under its original case
name and parameters: root-only motion yielded about 10.10 N·s while authored
part-local motion yielded about 0.674 N·s for the same 0.6 m / 60-step path.
The 9.913 N·s result above is a new candidate workload with 20 solver
substeps; it does not replace or rename that baseline. A new default-one-step
actual-asset run is retained at
[`ENG-210-animal-contact-substep-workload-default.json`](ENG-210-animal-contact-substep-workload-default.json).

## Actual authored assets and admission limit

The bounded release probe ran all authored clips/phases for trout, crow and
deer with 64 substeps, both Native Voxels and Merged Cuboids, root yaw 10°,
and the existing independent continuous-clearance oracle. All 48 transitions
were admitted and clear by the oracle; blocked transitions and representation
disagreements were both zero. There were 38,520 source-cell ray checks, 66 miss
checks, zero mismatches and zero timed misses. Source parts/cells and total
live body/collider counts were trout 5/136/6 bodies/5 colliders, crow
6/217/7/6, deer 9/285/10/9. Merged primitive counts were 17, 14 and 21;
Native Voxels used 5, 6 and 9 primitives respectively. Full transition +
query + step costs are retained per case in
[`ENG-210-animal-contact-substep-workload-64.json`](ENG-210-animal-contact-substep-workload-64.json).
Its p50/p95 microseconds by representation were trout native 2214.6/2362.9,
merged 1628.9/1716.0; crow native 2485.0/2622.7, merged 2797.8/3008.2; deer
native 4292.5/5293.9, merged 5150.4/5298.3. The probe has 100 repeats and 10
warmups per phase. These are one-animal measurements; no population target is
agreed.

The 64-step setting is necessary for the sampled authored poses under this
admission rule. Contextualized release runs rejected earlier: 20 substeps first
rejected the Crow `flap` sampled pose at tick 0; 32 substeps rejected Crow
`flap` tick 12. Exact command output is kept in
[`ENG-210-contact-substeps-20-attempt.txt`](ENG-210-contact-substeps-20-attempt.txt)
and [`ENG-210-contact-substeps-32-attempt.txt`](ENG-210-contact-substeps-32-attempt.txt).
This is explicit bounded rejection, not a slower or shortened successful
workload.

## Gate status

This demonstrates a physically mediated response candidate for the unchanged
fast case inside one existing Rapier world, while preserving the original CCD
off/on failure evidence and the slow baseline. It does not establish that 64
global substeps are affordable in a populated world, nor that the configured
2.5 cm surface-travel bound is a universal collision-accuracy guarantee. The
fast prescribed kinematic motion produces very large dynamic energy and no
recoil is inferred from source-part hierarchy. Source-cell damage attribution
remains unaccepted: Rapier manifold identity does not justify rounding a
contact point to a source voxel. Root rotation, exact transition terrain
blocking, source-cell damage resolution, and population costs remain separate
gates. No runtime lifecycle, corpse aggregate, networking, storage or UI was
implemented.

A separate fast-angular impulse case remains unproven. The configured
conservative bound rejects a 90-degree part-local turn for the one-cell probe
fixture: its Rapier local AABB bound is 3.464 m, so `radius × angle` is 5.44 m,
above the 1.60 m budget at 64 substeps. A 20-degree turn is admitted, but the
attempted probe placement did not contact the moving collider and is not
response evidence. The authored 10-degree root-yaw transition matrix is
validated for sweep/query parity and admission, but it does not measure a
dynamic impact. Thus the translation result does not close the angular-contact
gate; the next experiment needs a contact fixture whose actual native and
merged collision envelopes are independently verified before measuring solver
response.

Checks run:

- `cargo test -p spall_physics --lib --test articulated_review` — 83 passed,
  1 existing ignored; all 8 review cases passed.
- `cargo test -p spall_physics articulated_part_body_fast_single_step_contact_is_measured_without_sweep -- --nocapture`
  — original fast-case failure measured for CCD off/on and both representations.
- `cargo test -p spall_physics articulated_substeps_ -- --nocapture` — fast
  response and slow/zero/admission cases passed; raw values above.
- `cargo fmt --all -- --check` — passed.
- `cargo clippy -p spall_physics --all-targets -- -D warnings` — passed.
- `cargo clippy -p sandbox --example animal-collision-probe -- -D warnings`
  — passed.
- `cargo build -p sandbox --release --example animal-collision-probe` and
  `animal-collision-probe.exe --bounded --kinematic-parts --solver-substeps=64`
  — completed with the raw results linked above.
