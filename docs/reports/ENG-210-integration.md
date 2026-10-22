# ENG-210 coordinator integration — 2026-10-16

Accepted for bounded CPU feasibility, not production population or live actors.
The reviewed local series is integrated through `aabc2d3` (worker fix
`26a294a`). Engine defaults retain one solver step; experimental substeps are
explicitly selected. No default runtime, weather, network or save policy changed.

## Result and limits

- One kinematic rigid body per source part in the existing Rapier world restores
  prescribed animated-surface response. One conceptual owner retains exact
  per-part volumes, integer cells, material and revision provenance.
- Combined clip/root updates validate before scheduling and publish at the
  outer step boundary. Exact occupied-cell query Cuboids sweep the defined
  linear part-origin/shortest-arc rotation path without creating voxel bodies.
  The independent continuous plane-clearance oracle certifies the 48 sampled
  actual-asset transitions; the earlier rotating aggregate-Voxels discrepancy
  remains retained as historical evidence.
- Optional global substeps advance the entire existing solver by one total
  outer `dt`. The unchanged fast translation and angular fixtures produce
  physical solver response, including energy injected by prescribed kinematics.
  A source-corner travel bound rejects over-budget targets before mutation.
  This does not guarantee arbitrary thin/high-speed contact or animal recoil.
- Solver-point witnesses preserve source identity and sampled pose provenance.
  Only unique exterior face interiors resolve to a source cell; ambiguous,
  penetrating and edge contacts remain unattributed. Point impulse is mapped
  from the solver, never redistributed from a pair total. No gameplay damage
  thresholds or edits are implemented. Output is capped at 4096 with overflow.
- Root-only spherical sweeps remain conservative and can overblock. The
  continuous cell path is a distinct, more expensive operation. Neither is a
  navigation or habitat policy. Original one-step failures are not erased.

Independent read-only review found one topology race: a source rebuild could
publish a new topology while a pose-only update was pending. The final fix
rejects that rebuild. Its two-representation regression proves no revision,
geometry, query or pending-state mutation; the staged pose commits under its
original topology, then the rebuild succeeds. The reviewer verified this
specific blocker closed before coordinator acceptance.

## Checks on the integrated checkout

```powershell
cargo test -p spall_physics -p spall_asset -p spall_sim -p sandbox --lib --test articulated_review --test animal_assets --test animal_habitat_world
cargo check --workspace --all-targets --all-features
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo clippy -p sandbox --example animal-collision-probe -- -D warnings
cargo fmt --package spall_physics --check
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
git diff --check
cargo build -p sandbox --release --example animal-collision-probe
.\target\release\examples\animal-collision-probe.exe --bounded --kinematic-parts --solver-substeps=32
.\target\release\examples\animal-collision-probe.exe --bounded
```

All passed. Combined tests: **233 passed, 7 ignored**: 90 physics unit,
8 collision review, 96 simulation, 13 asset, 23 sandbox library and 3 animal
integration tests. Ignored diagnostics are not passes. Workspace all-target,
all-feature compilation and strict scoped lint passed. Existing incremental
cache hard-link warnings used copy fallback without failing checks.

Both optimized probes passed **38,520 source rays and 66 miss checks** with
zero mismatches or timed misses. Candidate accepted all 48 sampled authored
transitions with zero representation disagreements. Six authored contact cases
retained 32 points each with nine positive mapped point impulses and no overflow.
The fixture selects an exposed face; this is not proof of every possible contact.

Local integrated-run evidence: `.local/eng210-integrated-tests.log`,
`.local/eng210-workspace-check.log`, `.local/eng210-integrated-candidate32.json`,
and `.local/eng210-integrated-default.json`, with separate probe stderr logs.
The committed scoped reports/raw JSON retain implementation-phase measurements.
Full transition timings summarize eight heterogeneous intervals per case, rather
than a population throughput trial. Timing distributions for repeated pose/query
work are separate. No whole-world frame-time acceptance follows from these data.

Changed implementation: `crates/spall_physics/src/{articulated,world,lib}.rs`
and `examples/sandbox/examples/animal_collision_probe.rs`. Scoped reports and
raw measurements are `docs/reports/ENG-210-*`; shared animal/lifecycle/task
documentation records the accepted boundary and next prerequisite.

## Next prerequisite for the requested 1 km launch

**ENG-212:** an editable multi-volume dynamic corpse aggregate preserving the
same source cells and death poses. Then implement the reviewed host/ID and
provenance contract, atomic lifecycle/source ownership, durable replicated
state, sandbox habitat/locomotion policy, and the two-client/restart/destruction
proof in `docs/animal-lifecycle.md`. Initial proof is one trout, crow and deer;
many-animal capacity needs a separate measured target.

No full-workspace test run, GPU/weather combined traversal, actor replication,
storage migration, corpse transfer, far-origin precision or 1 km population gate
was run or accepted by ENG-210. The user explicitly requested waiting for fully
simulated animals; the combined launch remains held rather than substituting
the visual zoo.
