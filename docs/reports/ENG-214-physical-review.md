# ENG-214: bounded physical split and fragment-edit review

2026-10-16. Integrated through `f4ea2ed` on `release/remaining-stack`.
This accepts the bounded CPU implementation described here, not completion
of ENG-214 or the combined weather/animal launch.

## Physical ownership and edits

One logical articulated owner retains the original source VolumeIds. Each
physical component owns a disjoint mask of original source cells, retaining
part IDs, coordinates, materials and revisions. Cropping a derived mask changes
its storage bounds, never its source coordinates. Component IDs pair the
logical corpse key with an owner-allocated monotonic serial; lifetime key
reservations prevent reuse after retirement. Physics handles stay private.

Snapshot preparation targets one physical component. Commit checks current
topology, pose epoch, source generation and complete source geometry, recomputes
the canonical partition, and prepares all child shapes, masses and IDs before
swapping bodies. Forged, stale, over-budget or invalid-mass proposals leave the
old owner intact. Independently moving fragments are not re-welded by matching
integer coordinates in the original source volume.

Child bodies inherit the current rigid velocity field at their own centres of
mass and retain angular velocity. Creation, transfer, split and edits use an
immutable per-material density binding. A changed callback cannot silently
change surviving mass. Keyed removal-only edits preserve point velocities,
account for removed mass, update the original volume revision across sibling
shards, and support subsequent splits and complete retirement. Whole-part
legacy edits reject split owners. Secondary fragments retain live ray-query,
contact-impulse and source-witness routing.

Review found and corrected double transformation of Rapier's world COM,
uninitialized mass/COM caches, secondary-body contact omission, validation
after mutation, retired-key reuse, changed-density mass drift and stale primary
collider references. Independently authored tests use closed-form unequal-mass
cube velocities and linear/angular momentum, then query moving source cells.
They also verify non-primary terrain contacts, unchanged sibling motion through
bridge removal/re-splitting, exact source conservation and final retirement.

## Integrated checks

```text
cargo test -p spall_physics -p spall_asset -p spall_sim -p sandbox --lib --test articulated_review --test corpse_mass_review --test fracture_review --test fracture_commit_review --test fracture_motion_review --test animal_assets --test animal_habitat_world
cargo check --workspace --all-targets --all-features
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo clippy -p sandbox --example animal-collision-probe -- -D warnings
cargo fmt --package spall_physics --check
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
cargo build --release -p sandbox --example animal-collision-probe
target/release/examples/animal-collision-probe.exe --fracture-bounded
target/release/examples/animal-collision-probe.exe --corpse-bounded
target/release/examples/animal-collision-probe.exe --bounded --kinematic-parts --solver-substeps=32
target/release/examples/animal-collision-probe.exe --bounded
git diff --check
```

Measured: 261 tests passed, seven existing manual/extended tests ignored;
workspace compilation, scoped strict lint and formatting passed. The eighteen
corpse cases passed. Candidate movement/query regression retained 38,520 rays,
66 guaranteed misses, zero mismatches/timed misses and zero representation
disagreements. The unchanged default probe also completed successfully.
Logs and raw regression output are under `.local/eng214-integrated-*`.

## Actual animals and remaining failures

The unchanged nine-pose survey is committed as
`ENG-214-physical-survey.json`. Trout's three sampled poses each retain one
136-cell component (127-187 microseconds planning). Crow idle prepares seven
components containing 177/5/5/5/5/10/10 cells (466 microseconds planning).
Physical commit takes 575 microseconds, produces seven bodies and eleven
colliders, and recaptures all 217 source cells exactly once. All 217 original
nearest-hit ray results are preserved across the transfer; these are baseline
comparisons, not 217 uniquely visible cells.

An exploratory probe incorrectly expected every buried cell to be visible from
outside the animal. It failed on crow cell `[-3,-1,-1]`, where the correct
nearest hit is `[-3,-1,1]`. The corrected probe separately checks complete source
partitions and before/after nearest-hit equality. The failure was an invalid
test oracle, not evidence of lost source cells.

Crow flap at ticks 0/48 and deer idle 0/walk 0/48 still reject ambiguous
rotated-cell contacts (1.03-1.52 milliseconds planning). Successful survey
completion is not acceptance of those five poses. The crow's disconnected
idle geometry is a source-art finding, not accepted anatomy. No source assets,
bonds, workload sizes or geometry thresholds were changed to hide failures.

Next unblocked work remains ENG-214: a reviewed certified rotated-cell
connectivity predicate and actual asset evidence. A separate interval-based
predicate implementation is underway; it is not integrated or accepted here.
Runtime actors, authoritative lifecycle, replication/recovery, population
performance and the 1024 m weather/fully simulated animal launch remain gated.
GPU and combined-world performance were not measured in this CPU pass.
