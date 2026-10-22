# ENG-214: bounded connectivity planner review

2026-10-16. Partial implementation only; ENG-214 remains in progress.
Integrated planner and independent review tests: `0b522a9`, `39cf525`.

## Reviewed behavior

The immutable planner retains source VolumeId, part ID, cell coordinate,
material and revision. Same-source solid cells connect across integer faces,
including material boundaries. Cross-source connectivity is certified only
for exactly proved common-lattice signed-permutation frames. Edge/corner
contact does not connect cells. General rotations can prove distant separation
with a conservative enclosure; uncertain contact between otherwise disconnected
components rejects the plan. Ambiguous pairs within an already certified
component cannot alter the partition and are counted separately.

The planner admits at most one million total dense entries, one million solid
cells and one million cross-volume pair comparisons, with at most 256 parts.
Admission precedes per-cell extraction. Same-volume pairs are excluded by
grouping, rather than spending unbounded quadratic work before a budget check.
Checked source-coordinate arithmetic rejects unrepresentable origins.
Signed-axis cell minima include the negative-axis unit-cell corner shift.
Error-free difference/product checks prevent rounded translation equality from
being accepted as an exact common-lattice proof.

This is deliberately a limited geometric predicate. Ordinary floating-point
quarter-turn quaternions may not satisfy the exact signed-permutation test.
Generic rotated overlap is not certified as material connectivity. PART
hierarchy supplies no bonds, and no source assets have been changed to bypass
the remaining failures.

## Checks

Coordinator checks in the isolated implementation worktree:

```text
cargo test -p spall_physics --lib --test articulated_review --test corpse_mass_review --test fracture_review
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo fmt --package spall_physics --check
git diff --check
cargo build --release -p sandbox --example animal-collision-probe
target/release/examples/animal-collision-probe.exe --fracture-bounded
```

Measured: 120 tests passed, one existing manual timing test ignored; strict
physics lint, formatting and diff checks passed. Seven independently authored
review tests cover mixed-material bridge removal, checked coordinate failure,
81 half-turn overlap/face/edge/corner configurations, large-origin fractional
offsets, single-source pair-budget exclusion, cumulative sparse-grid dense
admission and distant arbitrary rotation. Independent arithmetic/bounds review
found no concrete false-weld blocker within the stated predicate scope.

The fresh release survey examined nine actual-asset poses:

| Asset | Result | Measured planning time |
| --- | --- | --- |
| Trout, idle and two swim poses | 3/3 prepared; all 136 cells in one component | 123-211 microseconds |
| Crow, idle | Prepared; 217 cells in seven components | 476 microseconds |
| Crow, two flap poses | 2/2 rejected as ambiguous | 1.05-1.15 milliseconds |
| Deer, idle and two walk poses | 3/3 rejected as ambiguous | 1.30-1.38 milliseconds |

These are bounded CPU planning measurements, not population or world-tick
budgets. A successful probe process means the survey completed and recorded
failures; it does not mean every asset passed. The crow's seven idle components
are a real source-connectivity finding, not an accepted physical outcome.
Earlier exploratory survey results are superseded by this stricter predicate.

## Remaining work

No physical split is accepted by this report. ENG-214 still requires atomic
one-to-many body installation, stable component IDs, exact source conservation,
mass/momentum and live query identity, stale/failed commit isolation, and
continued component-aware editing/re-splitting after fragments move. Preparing
a partition alone does not provide these behaviors. Actual animated crow/deer
connectivity also remains unresolved; source corrections require explicit
review and visible material geometry rather than hidden anchors.

The active next task remains ENG-214. Host lifecycle, replication/recovery and
the combined 1024 m weather/fully simulated animal launch remain gated by the
reviewed ownership and destruction implementation.
