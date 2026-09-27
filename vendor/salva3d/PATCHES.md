# Local Salva instrumentation

This directory contains the Salva `salva3d` 0.10.0 package sources for the
bounded `spall_fluid` feasibility prototype. The upstream package version is
unchanged. The only Rust behavior change is telemetry in
`src/counters/solver_counters.rs` and `src/solver/pressure/dfsph_solver.rs`:
pressure/divergence iteration counts, last stopping residuals, and whether the
configured stopping rule was met in the last substep.

The vendored manifest retains the 3D f32 CPU build and optional Rayon parallel
execution. Unused optional Rapier/parry/testbed/sampling integrations were
removed from this local prototype manifest because those features are not
compiled by `spall_fluid`; restore and review them before using this fork for
Rapier coupling. The `parallel` feature uses same-version local Rayon 1.12.0,
rayon-core 1.13.0, crossbeam-deque 0.8.7, and crossbeam-epoch 0.9.20 sources
under `../parallel/` so the comparison can build without writing to Cargo's
outside-workspace cache. No workspace dependency version was upgraded.

Salva's `Timer` still has disabled clock calls and is not a timing source.
Measured time is the wall-clock duration around every complete `LiquidWorld`
step, including all configured fixed substeps.
