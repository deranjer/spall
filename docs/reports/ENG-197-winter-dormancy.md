# ENG-197 — winter dormancy

Implementation report for WX1 on the frozen `LivingState` chronological clock.
Vegetation save and visual frame version 4 persist active growth separately
from chronological age. Spring, summer, and autumn contribute growth, biomass,
seed-production, and cleared-cover regrowth credit; winter contributes zero.
Deferred organism visits compute active time over the complete interval, so
fair-budget scheduling can cross one or more season boundaries without losing
or duplicating credit. Terrain eligibility and support checks remain before
credit is applied and continue to run in winter.

Cleared-cover records now carry accumulated active milliseconds. Cover remains
stateless outside this existing bounded clearing memory, and clients render the
same accumulated value supplied by the authoritative visual frame.

Version 1, 2, and 3 vegetation saves migrate deterministically. Versions 1 and
2 preserve their existing carpet conversion and skeleton checks; version 3 has
an explicit legacy decoder. Chronological age is retained. Active age is
reconstructed from the saved age and last update interval, and seed progress
from the last seed/update interval. Cleared records convert their old
chronological timestamp to active elapsed time. Historical winter credits
cannot be identified exactly in the old organism fields, so old plant active
progress is an estimate; future progress and all version 4 continuation are
exact.

## Verification

- `cargo test -p spall_ecology --lib` — 36 passed, 0 failed.
- `cargo test -p sandbox --test vegetation winter_freezes_wood_biomass_seeds_and_cleared_cover_then_spring_resumes_once -- --exact --nocapture` — passed.
- `cargo test -p sandbox --test vegetation authoritative_growth_and_checkpoint_recovery_keep_the_same_living_state -- --exact --nocapture` — passed.
- `cargo check -p spall_client` — passed.
- `cargo xtask check` — passed: formatting, workspace clippy/build, and all CPU tests; ignored tests remained unrun by their suite annotations.

GPU captures and hardware performance gates were not run; this change adds no
rendering workload or performance claim. Existing ignored timing tests are not
part of the measured result.
