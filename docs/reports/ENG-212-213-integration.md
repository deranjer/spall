# ENG-212/213 coordinator integration (2026-10-16)

Accepted for bounded CPU feasibility through local commit 30e859b. This does
not accept shipping fracture, live actors, population or weather-combined play.

ENG-212 atomically converts the live rig to one dynamic frozen-pose compound,
retaining source VolumeIds/cells/materials/revisions. Source mass and sampled
motion define full inertia and transfer momentum. Edits preserve surviving
world-point velocities through COM changes and report removed material mass;
additions/material substitutions reject without an incoming-momentum policy.
Empty parents retire by flattening frozen survivor poses without moving cells.
Independent review verified fixes to COM velocity, mass accounting and parent
retirement. A closed-form rotated-box/nested-cube oracle verifies every tensor
entry. Disconnected material is not accepted as permanently anchored together.

ENG-213 uses current BVH candidates bounded over the full part motion, then
casts exact source-cell Cuboids directly against selected live terrain shapes.
Candidate collection is bounded before growth by the existing one-million cast
cap. Unsupported queries reject before pose staging. Independent review found
no remaining concrete blocker after those corrections; geometry was not reduced.

## Exact integrated checks

```powershell
cargo test -p spall_physics -p spall_asset -p spall_sim -p sandbox --lib --test articulated_review --test corpse_mass_review --test animal_assets --test animal_habitat_world
cargo check --workspace --all-targets --all-features
cargo clippy -p spall_physics --all-targets -- -D warnings
cargo clippy -p sandbox --example animal-collision-probe -- -D warnings
cargo fmt --package spall_physics --check
rustfmt --edition 2024 --check examples/sandbox/examples/animal_collision_probe.rs
git diff --check
cargo build --release -p sandbox --example animal-collision-probe
.\target\release\examples\animal-collision-probe.exe --corpse-bounded
.\target\release\examples\animal-collision-probe.exe --bounded --kinematic-parts --solver-substeps=32
.\target\release\examples\animal-collision-probe.exe --bounded
.\target\release\examples\animal-collision-probe.exe --generated-1km
```

All passed: **242 tests, 7 ignored** (93 physics, 13 articulated review,
one independent tensor oracle, 96 simulation, 13 asset, 23 sandbox library,
three animal integration). Ignored diagnostics are not passes. Compiler cache
hard-link warnings used copy fallback. Workspace compilation and scoped lint
passed. Eighteen authored live-rig-to-corpse transfer/edit cases passed.
Candidate and default probes each passed 38,520 rays and 66 misses, zero errors.
Both generated-world and corpse probe modes remain available after the merge.

The ENG-213 integrated full-world run used natural seed 1, 4096 cells/edge
(1024 m), **123,892 fixed terrain colliders** and all 60 sampled animal phases
across both representations, all accepted. Median crow transition was
0.877-0.879 ms; deer 5.935-6.020 ms. The 30-sample trio-plus-dynamic-probe run
at 32 substeps measured wall p50 0.467 ms/p95 0.840 ms, with one cold sample
of 540 ms. Warmup samples remain included in the raw distributions. One cold
trout query took 39 ms. No weather or water stepping was measured; initial
water-plan data only validated the initial trout habitat. These are bounded
observations, not a sustained population SLA.

Evidence: .local/eng212-213-integrated-tests.log,
.local/eng212-213-workspace-check.log, .local/eng212-integrated-corpse.json,
.local/eng212-213-integrated-candidate.json and integrated-default.json,
.local/eng213-integrated-full-world.json, with separate stderr logs. The full
world run checked the integrated locality series before corpse merge; the
combined build then reran corpse and small/default matrices. Committed worker
reports retain raw timings and earlier failures. No full-workspace test run,
GPU capture, actor network/recovery or combined-weather gate is claimed.

Changed code: physics articulated.rs/world.rs/mass.rs/lib.rs, two review test
files, sandbox animal_collision_probe.rs and scoped ENG-212/213 reports. No
runtime actor, network, save or UI implementation is added.

## Next launch prerequisite

**ENG-214:** explicit corpse connectivity and source-cell split ownership.
PART is not a bond graph; disconnected source cells cannot silently stay
anchored together. Atomic one-to-many splits must retain exact provenance and
physically justified motion. Host/lifecycle/provenance interfaces depend on that
reviewed decision. Live habitat behavior, two-client replication, save/restart,
destruction and combined weather/GPU acceptance follow. The user chose waiting
for fully simulated animals; the combined game instance is not ready or launched.