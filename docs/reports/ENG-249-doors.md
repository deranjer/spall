# ENG-249: proximity door interaction

Delivered an E Open / E Close label projected beside a nearby visible cabin
door, and server-authoritative opening/closing through existing topology
transactions. The server derives reach from its player and ignores claimed aim.
Panel damage rotates with the panel; the hinge remains fixed and records state.
Terrain, players and debris obstruct a conservative rectangular swing region.
Player/body occupancy is rechecked before publication. No separate state store
or replication path was added.

Changed files: `crates/spall_voxel/src/{doors,lib}.rs`,
`crates/spall_sim/src/{intent,stage,commit}.rs`,
`crates/spall_protocol/src/{records,handshake}.rs`,
`crates/spall_server/src/serve/actions.rs`, client window
`{options,app,handler,renderer,admin_menu}.rs`, sandbox
`src/{game,editor_scene}.rs` and `src/bin/sandbox-client.rs`,
`tests/{cabin,cabin_doors_network}.rs`, cabin generator/assets, README,
architecture/protocol/validation docs and these reports.

Measured checks:

- `cargo run -p spall_editor --example cabin`: passed; generated and reopened.
- `cargo test -p sandbox --features client --test cabin --test cabin_doors_network`:
  five CPU cabin tests passed; network test initially denied Windows access.
- `cargo test -p sandbox --features client --test cabin_doors_network` with host
  access: passed (1 test, 15.36 s), including late join, request deduplication,
  open/close and exact replica convergence with deliberately bogus claimed aim.
- `cargo test -p spall_server --lib door_tests`: passed (1 test).
- `cargo test -p spall_sim -p spall_voxel -p spall_protocol -p spall_server --lib`:
  passed (380 tests, 8 ignored, zero failures).
- `cargo check -p sandbox --features client`: passed.
- `cargo fmt --all`: passed.
- `cargo xtask check` with host access: proportionality, formatting and strict
  workspace all-target/all-feature Clippy passed. Workspace tests failed in
  `animal_death::a_hammer_strike_kills_a_deer_and_leaves_an_editable_carcass`:
  carcass cell `(128,166,116)` was never cut (40.65 s). This is outside the cabin
  scenario; its baseline failure status has not been established. Full suite
  is not green. Earlier restricted runs failed before this test's server bound.
- Bounded graphical cabin launch passed with host access; exterior and spawn
  captures inspected. A simultaneous second xtask invocation encountered a
  Windows executable lock; the capture was rerun after validation exited.
- `cargo xtask play --editor-scene fixtures/cabin --shots 'walk;11,2.6,11,180,0'
  --shots-dir .local/cabin-final-captures --ticks 1800
  --output .local/runs/cabin-final-visual`: exited 0; both captures inspected,
  including the E Open label and furnished interior. Raw PNGs/logs remain local.
- `git diff --check`: passed.

Integration decision: retain the scoped implementation with its passing focused
and affected-library evidence; report the full-suite failure explicitly rather
than expanding this content ticket into animal gameplay changes.

Rule 1 additions and runtime callers: public `spall_voxel::doors` types/helpers
are called by server action resolution, simulation staging/commit and client
prompt rendering; `EditKind::Door` is supplied by sandbox's tool catalog;
`ActionKind::Interact` is sent by the client E handler and consumed by server
resolution; `InteractiveOptions.door` is set by sandbox-client; `CABIN_DOOR`
is used by the material manifest, importer, tool catalog and client options.
The generator is an example, not a production runtime config or harness feature.

Wire version 18 -> 19 for the new action discriminant. Save/SPVOX versions are
unchanged. No proportionality exceptions or allowlist additions.

Limits: quarter-metre, identity-transform doors turn instantly from +X to +Z;
the conservative swing rectangle may reject a turn that a precise arc could
allow. Furniture is modular in the editor and flattened into destructible
terrain on import; runtime pickup/moving furniture is not implemented. No
hands-on E-key playtest has been claimed. Next unblocked work is investigating
the full-suite carcass test failure; further furniture interactions need a
separate ticket scoped to the desired player behavior.
