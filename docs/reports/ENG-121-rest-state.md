# Water integration and rest-state investigation - 2026-10-02

## Integration

Remote main `941d7eb` already includes ecology, domain growth and solver
performance through PR182. Its tracked tree is identical to the reported water
branch (`459975d`). The primary checkout was fast-forwarded to main. The four
staged jitter files in `crates/spall_client/src/` (`net.rs`, `predict.rs`,
`replica.rs`, `window.rs`) were copied without changing the source worktree.
They advance detached geometry around its centroid and batch minor background
sky occupancy changes.

## Bounded rest-state diagnostic

```powershell
cargo run --release -p spall_fluid --example water_rest_probe
```

The example uses actual voxel boundaries, majority coarsening and the production
two-phase multigrid solver. Each case has 48 x 24 x 24 quarter-metre terrain
cells, 16 x 8 x 8 fluid cells at 0.75 m, a level surface at 3.75 m, closed
sides/bottom and an open top. Run 120 accepted 50 ms steps (six simulated
seconds), default pressure tolerances, no sources/edits/sleep/damping. Seed
water voxel amounts into whole-cell fractions, reporting dropped solid-cell
water, as the production seed does. Any solver error aborts the run.

Measured on this Windows machine:

| Case | Partial seed cells | First-step KE | Final KE | Peak liquid speed | Max fraction drift |
| --- | ---: | ---: | ---: | ---: | ---: |
| Flat floor | 0 | 1.07e-15 J | 6.95e-16 J | 9.34e-8 m/s | 3.04e-11 |
| Grid-aligned submerged stairs | 0 | 3.32e-16 J | 2.27e-16 J | 1.34e-7 m/s | 3.42e-11 |
| Unaligned submerged stairs | 48 | 1,907 J | 12,413 J | 3.16 m/s | 0.186 |

Every pressure substep converged. Absolute water-plus-outflow accounting error
was below 2e-13 m3 in every case. Unaligned stairs seeded 130.5 m3 and reported
7.875 m3 dropped into majority-solid cells; final liquid speed was 2.03 m/s.
Step medians were 0.51-0.71 ms. The cases have different bottom geometry and
seeded volume: this isolates a representation failure, not its contribution
to generated-world cost. It is not a substitute for the 512/1024 scenario.

## Interpretation and next assignment

Resolved staircase terrain remains still. The failure appears when coarsening
leaves partially solid cells classified as fluid: solid capacity is omitted,
yet seed water occupies less than the whole cell. `relative_inverse_face_density`
interprets the remainder as ambient air. This introduces horizontally varying
mixture density underneath a level lake. A hydrostatic pressure gradient cannot
cancel gravity while also having zero horizontal gradient in that representation.
This is a diagnosis from code and controlled cases, not a shipped correction.

Next unblocked assignment: **ENG-122**, preserve submerged solid capacity and
face boundaries during coarsening. A capacity fraction alone does not establish
impermeable faces or consistent transport. Preserve volume, intact thin walls,
supported openings, displacement/trapped water, sealed air, durable recovery and
validated immutable worker results. Do not fill lost seed volume or relax
pressure/sleep thresholds to conceal the imbalance.

One correction to the earlier cost description: two-phase `project` includes
every non-solid water **and air** cell in pressure rows. It does not restrict
rows to wet cells. Wet patterns affect coefficients and convergence, but this
is not an active-liquid-only pressure grid.

## Checks and limitations

- `cargo xtask check`: format and strict all-targets/all-features workspace
  Clippy passed; tests failed at `sandbox::dam_gate_replication` final world
  hash equality. All 728 gate cells opened on the client. An isolated rerun
  failed the same assertion. Investigation found the client stopped at tick 200
  while the server ran to 300; the failed trace had 9 client-applied versus 11
  server-committed topology transactions. A further isolated jitter-tree run
  and an unmodified-main control passed, confirming sensitivity to timing.
  The test now waits for server shutdown (`run_ticks: 0`), retaining its
  60-second timeout, bounded server and exact hash assertion. The final full
  `cargo xtask check` rerun passed: **1,098 tests passed, zero failed**, including
  formatting and workspace/all-targets/all-features Clippy with `-D warnings`.
- `cargo test -p spall_fluid -p spall_sim -p spall_protocol -p spall_client
  --all-features`: 381 passed, including the jitter, growth, hydrostatic, sealed-air,
  conservation and displacement tests.
- `cargo clippy -p spall_fluid --example water_rest_probe -- -D warnings`: passed.
- `cargo run --release -p spall_fluid --example water_rest_probe`: passed with
  the measurements above.

Interactive game-window acceptance and new generated 512/1024 measurements
were not run. ENG-120/121 remain in progress. This pass adds a diagnostic rather
than changing the production fluid representation or numerical tolerances.
Raw probe records: `ENG-121-rest-state.jsonl` beside this report.
