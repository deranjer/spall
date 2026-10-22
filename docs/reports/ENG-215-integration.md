# ENG-215: animal identities and bounded scheduling

Coordinator acceptance: 2026-10-16. Implementation and independent review tests
are integrated through `32cf744` on `release/remaining-stack`.

## Delivered scope

`spall_core` now provides stable creature keys and a checked monotonic allocator
with versioned counter capture/restore. Reset advances the world epoch without
rewinding creature IDs. Invalid deserialized epochs and exhausted counters reject.

`spall_protocol` adds standalone versioned creature state and definition-reference
validation. Species/version resolves to exactly one asset identity; asset, clip,
material manifest/mapping, source bindings, poses and lifecycle state are checked.
Encoded state is capped at 64 KiB and source bindings at 256. The bounded binding
deserializer rejects oversized sequence hints before reading elements.

`spall_sim` adds an owner-thread host and static game-rules proposal interface.
Scheduling follows published terrain/water/weather/snow state and precedes physics.
Each batch is limited to 64 actor snapshots, 1,024 source bindings and 64 intents.
All proposal tokens are validated before acceptance. Scheduling proposes changes;
it does not mutate actors. Production registration remains unavailable; fixture
registration is test-only and consumes the exact next allocator key after preflight.

## Review corrections

Coordinator tests exposed cross-species asset selection, conflicting species
definitions, oversized decoded binding vectors, invalid deserialized world epochs,
playing corpse state, replacement identity loss and fixture allocator reuse.
The integrated implementation rejects these cases. A replacement simulation with
its own creature identity state is explicitly rejected before any mutation.
Corpse clips must be frozen and cannot accept phase-advance proposals.

Independent integration tests live in
`crates/spall_protocol/tests/creature_protocol_review.rs` and
`crates/spall_sim/tests/creature_host_review.rs`. The six tests pass, including an
oversized 5,000-binding payload below the byte limit and failed reset atomicity.

## Exact checks

Run from the integrated main checkout:

| Command | Result |
| --- | --- |
| `cargo test -p spall_core -p spall_protocol -p spall_sim` | 334 passed, 0 failed, 9 ignored across 36 test/doc-test suites |
| `cargo check --workspace --all-targets --all-features` | Passed |
| `cargo clippy -p spall_core -p spall_protocol -p spall_sim --all-targets -- -D warnings` | Passed |
| `cargo fmt --check` | Passed |
| `git diff --check` | Passed |

Logs: `.local/eng215-integrated-tests.log`, `.local/eng215-workspace-check.log`,
`.local/eng215-clippy.log`. Build output reports filesystem hard-link cache
fallback warnings. The nine ignored tests are existing collider timing, body
proximity scaling, full-world stage/commit/startup/collider probes, giant collapse
and two demo frame dumps. They were not measured by this pass.

Changed implementation files: `spall_core/src/{ids,lib}.rs`,
`spall_protocol/src/{creature,lib}.rs`, `spall_sim/src/{creatures,lib,schedule,sim,world}.rs`,
the two review tests above and `docs/animal-lifecycle.md`.

## Remaining launch requirements

This accepts the host foundation only. Standalone DTO roundtrips and counter
snapshots do not establish wire replication or durable recovery. There is no
production animal birth, source-volume ownership transaction, death transition,
species controller or live population in this slice. Source bindings currently
describe the complete asset; retirement semantics require explicit lifecycle review.

The next dependency-ready work is atomic actor lifecycle/source ownership, including
review of canonical owner/hash and transaction formats before changing shared
interfaces. Species movement with current habitats, multiplayer late join/reset,
save/crash/recovery, damage/destruction and integrated 1024 m weather/world proof
remain required. GPU and population performance gates are unrun here.
