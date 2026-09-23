# ENG-80 residency-safe terrain collider lifecycle

Status when written: experimental and default-off. The user directed
provisional default adoption on 2026-09-23 after this lifecycle port; see
`ENG-75-76-79-80-g4-rubble-lane.md` for the decision and later evidence.
The design and checks below describe the original residency-safe port.

The validated ENG-75/76/77/79 integration head does not contain the earlier
`terrain_bricks` prototype or its server option. The safe port therefore keeps
the existing whole-terrain collider unchanged while all terrain is resident,
then lazily installs fixed per-resident-brick derived colliders at the first
residency eviction. Eviction removes only the affected brick collider. A
durable reload validates the candidate brick before publication, reinstalls
only that brick's collider, and clears its retained digest afterward. A
commit rebuilds resident brick colliders from the same exact occupancy used by
the authoritative volume; absent bricks are never treated as air.

`SimWorld::validate_terrain_brick_colliders` checks that every resident solid
brick has a collider built from its current revision and that an evicted brick
has no active collider. Logical hashes, conservation, durable backing
validation, and reload/retry ordering remain unchanged. The default-off,
fully-resident path does not install the per-brick representation.

Evidence from the focused port checks:

- `cargo test -p spall_voxel --lib logical`: 11 passed.
- `cargo test -p spall_sim --test logical_commit --test logical_reload`: 9
  passed, including eviction-order/hash stability, wrong-backing atomicity,
  reload/retry, and collider eviction/reload lifecycle plus stale-collision
  assertions.
- `cargo test -p spall_server --test residency_pass`: 14 passed, including
  residency-on/off hash and recovery equivalence, durable disk backing, and
  checkpoint fail-closed cases.

The full-horizon repeated networked pair required by ENG-80 was not run in this
port. Dormancy churn and sustained tick/physics p95 therefore remain open, as
does any adoption decision. The mode remains experimental/default-off pending
the networked evidence and the ENG-79 dormancy-policy gate.
