# ENG-216: bounded animal source ownership and lifecycle

Coordinator acceptance: 2026-10-16 local date. Implementation and independent
review tests are integrated through `cd0b6e3`. This accepts the reviewed in-memory
test seam, not production animal registration or the 1024 m launch.

## Implementation and review

Test-only SimWorld storage owns original source Volumes separately from ordinary
Bodies, keyed by CreatureKey and part ID with a unique reverse VolumeId index.
Simulation coordinates cloned actor/source allocators, host state and physics
preflight before publishing birth. Caller-built fixture volumes must match the
IDs reserved by the authoritative allocator. Birth requires an alive, playing,
stationary record and exact world/catalog material identity; every occupied cell
must use a known material mapped by the selected asset definition.

Death preserves source IDs, cells, materials and revisions, freezes the clip and
retains applied local poses plus their parent hierarchy. A source freshness bundle
contains the runtime state stamp, full canonical source hash and sorted brick
revisions. Same-maximum source replacement/reload changes therefore reject.
Normal apply_edit already advances a volume-wide revision high-water mark; the
same-maximum fixture deliberately uses supported canonical brick insertion.

Review found that copying the living root twist into the corpse record lost
momentum from independently moving parts. An independent fixture reproduced a
stationary record versus a roughly 15 m/s physical corpse. The checked physics
transfer now exposes its exact computed resultant twist to the owner validator
after preparation and before any physics ownership mutation. The candidate
corpse record is validated there; both DTO and physics install the same f32 twist.
The existing transfer entrypoint delegates with unconditional acceptance and
retains its original physics calculation. A refusing owner validator leaves the
living rig, source ownership, actor state and counters unchanged.

## Evidence

Coordinator-owned tests in `crates/spall_sim/src/creature_lifecycle_review.rs`
cover corpse-as-birth rejection, foreign material manifests, unknown/unmapped
source materials, initial twist mismatch, both allocator exhaustion paths,
populated-world reset, exact transferred motion and prepublication owner refusal.

Worker fixtures cover invalid geometry and foreign IDs, stale/pending death,
same-maximum canonical source replacement and a three-part nested hierarchy under
a rotated/translated root with mixed materials. The nested fixture independently
composes expected cell positions, preserves before/after source-hit identities,
checks retained parent/local metadata and derives expected mass from cell volumes
and authoritative material densities.

Exact integrated checks:

| Command | Measured result |
| --- | --- |
| `cargo test -p spall_sim -p spall_physics` | 364 passed, 0 failed, 10 ignored across 44 test/doc-test suites |
| `cargo check --workspace --all-targets --all-features` | Passed |
| `cargo clippy -p spall_sim -p spall_physics --all-targets -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `git diff --check` | Passed |
| `cargo build --release -p sandbox --example animal-collision-probe` | Passed |
| `target/release/examples/animal-collision-probe.exe --corpse-bounded` | Passed all 18 existing trout/crow/deer cases |

Logs and release evidence: `.local/eng216-integrated-tests.log`,
`eng216-workspace-check.log`, `eng216-clippy.log`, `eng216-release-build.log`,
`eng216-corpse.json`. The original motion failure and eight-test checkpoint are
retained as `eng216-review-motion-failure.log` and `eng216-review-eight-pass.log`.
The final integrated suite includes all nine independent review tests. Build
output includes filesystem hard-link cache fallback warnings. Ignored checks are
existing occupancy/collider timing, body proximity scaling, full-world
stage/commit/startup/collider probes, giant collapse and two demo frame dumps.
They were not measured by this pass.

Changed files: `spall_physics/src/{articulated,lib,world}.rs` and
`tests/articulated_review.rs`; `spall_sim/src/{creatures,sim,world}.rs` and
`creature_lifecycle_review.rs`; `docs/reports/ENG-216-design.md`, this report,
`docs/animal-lifecycle.md` and the task queue. The worker design task-queue conflict
was resolved by retaining the coordinator's newer reviewed scope.

## Limits and next work

Mutation entrypoints and source storage compile only in spall_sim unit-test builds.
The source-only v1 hash has its own domain and excludes lifecycle/pose state;
legacy world_hash refuses populated test worlds. Ordinary owner bytes, production
body lookup, protocol tags, transaction ordering and saved schemas are unchanged.

There is no separated untrusted prepared-plan API, actor-targeted source edit or
fracture synchronization, whole-actor removal, production species construction,
ordered lifecycle journal/replay, replica baseline or checkpoint/recovery support.
Bindings are lifetime metadata; empty-part and terminal removal behavior remains
a following implementation requirement. This does not enable live population.

Next work must extend the existing ordered transaction/replay and canonical
actor/source state contracts, then persistence and multiplayer consumers before
production registration. Species movement/current habitat behavior and combined
1024 m weather/population proof follow. GPU, two-client animal, durable animal
recovery and population performance gates are unrun here.
