# ENG-214 integration: exact connectivity and editable physical components

2026-10-16. Coordinator acceptance for bounded CPU feasibility through
`8e620d1`, following the physical split/edit integration through `f4ea2ed`.
The nine-pose actual-animal gate now passes. This does not launch live animals
or accept authoritative lifecycle, multiplayer/recovery, GPU or population cost.

## Connectivity decision and evidence

The canonical geometry is the affine image of each original source cell using
the stored normalized-f32 part quaternion coefficients, interpreted as exact
inputs. This is an explicit source geometry definition, not a claim that every
Rapier floating-point contact calculation has identical bits. Same-volume
connectivity is integer face adjacency. Cross-volume connections require exact
common-lattice face contact, interval-certified positive-volume intersection,
or a certified positive-area shared face. PART hierarchy supplies no bonds.

The interval predicate uses all fifteen parallelepiped SAT directions: six face
normals and nine edge crosses. It does not assume orthonormal floating-point
axes or normalize near-parallel axes. Strict certified separation rejects a
connection; strict overlap on every nonzero axis proves interior intersection.
Exact-zero axes are skipped only when algebraically justified. Uncertainty
remains a rejection, never a tolerance weld.

The final deer walk failures were exactly coincident world-Z supporting faces,
with strict overlap of their projected polygons. The added face proof requires
both cells to have a certified pure common-axis edge, exactly zero axial
components on the other edges, exactly equal singleton plane endpoints, and
positive rank. Four existing in-plane SAT normals must all prove strict
projected overlap. That proves positive material face area. Edge/point contacts
do not satisfy it. No additional axes or higher work cap are introduced.

Interval multiplication now conservatively widens tiny products whose exact
FMA residual could underflow to zero. The regression uses
`a=b=(1+2^-52)*2^-500`: the rounded product loses `2^-1104`, and even its FMA
residual becomes zero. Exact zero operands remain exact; nonfinite products
invalidate the certificate.

An independent Python Fraction oracle projects all transformed polygon corners
using exact rational arithmetic on canonical binary32 inputs. It reuses neither
engine interval arithmetic nor support radii. Of 1593 nonboundary volume cases,
1561 classify correctly and 32 conservatively reject; no wrong connection or
separation occurs. Another 4779 exact shared-face cases across all three world
axes classify correctly. Additional rotated edge/point controls never connect.
The generator and static CPU fixtures are committed in the physics tests;
Python is not required to run those Rust tests in CI.

## Work bounds and physical ownership

All existing caps remain: one million total dense entries, one million source
cells, one million cross-volume pairs, fifteen million SAT axes and at most
256 components. Every cross-volume comparison counts, including broadphase
rejects. Per-cell interval boxes and conservative AABBs are cached once. AABB
disjointness can reject; AABB overlap never authorizes connectivity. Caching
reduces repeated box construction and full SAT work without changing source
cells, poses, pair caps or topology semantics.

The accepted physical ownership/edit contract remains in
`ENG-214-physical-review.md`: one logical corpse owns disjoint original-cell
partitions, lifetime-reserved monotonic component IDs, immutable density
bindings, atomic validated installation, and removal-only keyed edits and
re-splits after independent motion. Source coordinates/materials/revisions,
point velocities and momentum remain preserved under the specified open-system
removal rule. Failed/stale work leaves the original owner intact. Sibling
components stay unchanged, and full removal retires bodies/colliders.

## Integrated checks and measurements

```text
cargo test -p spall_physics -p spall_asset -p spall_sim -p sandbox --lib --test articulated_review --test corpse_mass_review --test fracture_review --test fracture_commit_review --test fracture_motion_review --test fracture_sat_contract --test fracture_exact_oracle --test animal_assets --test animal_habitat_world
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

Measured: 268 tests passed, seven existing manual/extended tests ignored;
workspace compilation, scoped strict lint, formatting and release runs passed.
The existing eighteen corpse cases pass. Movement/query candidate and default
retain 38,520 rays plus 66 guaranteed misses each, with zero mismatches or timed
misses; candidate representation disagreements remain zero.

The unchanged nine-pose survey passes with exact source partitions:

| Asset/pose | Components | Planning microseconds |
| --- | ---: | ---: |
| Trout idle / swim 0 / swim 60 | 1 / 1 / 1 | 288 / 212 / 202 |
| Crow idle / flap 0 / flap 48 | 7 / 7 / 7 | 583 / 958 / 941 |
| Deer idle / walk 0 / walk 48 | 3 / 1 / 1 | 885 / 745 / 713 |

Four multi-component poses perform actual physical splits. Commit measures
747-1061 microseconds. All 936 source records are recaptured exactly, and all
936 original nearest-hit baseline comparisons survive transfer. Query batches
measure 125-136 microseconds. Each split world then runs thirty seeded
translation/rotation steps; all fragment states remain finite and original
source partitions remain exact. P95 full-step costs are 11.5-16.7 microseconds
in these small zero-gravity fixtures. Independent tests separately cover
unequal-density closed-form mass/momentum, moving source queries, terrain
contacts/witnesses, stale/forged plans, bridge edits/re-splits and full retirement.

Raw final measurements: `ENG-214-final-survey.json`. Initial uncached interval
results (seven passes, two rejected deer poses and roughly 19-24 ms planning)
are retained in `ENG-214-interval-initial-survey.json`. Earlier five-pose failures
remain in the planner/physical reports. Contract tests formerly requiring
ambiguity for now-certified overlaps/separations were explicitly reconciled;
their geometry was not altered. No source assets or hidden connections changed.

## Limits and next task

Disconnected crow/deer source art is reported honestly; the engine separates
actual connected regions instead of inferring anatomical joints. The predicate
can still reject other uncertain geometries. This gate does not prove arbitrary
continuous-contact classification, population performance, damage networking,
durable component counters, or shipping source-art quality. The small-fixture
times are measured observations, not frame/population targets. The full 1024 m
locality gate remains the separate ENG-213 evidence; no combined weather/animal
GPU or network launch was run here.

Next unblocked task is ENG-215: authoritative host identities, bounded state
DTOs and a scheduling seam. Atomic lifecycle/source ownership, current-habitat
movement, two-client late join/reset and durable crash/recovery/destruction
integration remain required before the requested fully simulated animal launch.
