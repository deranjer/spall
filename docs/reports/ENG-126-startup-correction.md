# ENG-126 portable large-world startup correction

Historical diagnosis. The subsequent versioned capacity and network integration
is recorded in [current integration evidence](ENG-126-large-world-integration.md).
The old 4M gate described below is no longer the current capacity.

The packaged editor reported success immediately after spawning the launcher,
discarded its child handle, hid the console, and did not capture diagnostics.
The user's failed sessions contained only a join token: the server exited
before readiness or creation of `server.jsonl`. Waiting longer cannot recover
an exited process.

## Root cause and capacity blocker

Reproduced seed 1 at 4096 cells (1024 m) with the original packaged server:
exit code 2, `water domain 47421308 voxel cells exceeds the 4000000 budget;
nothing was truncated`. The size dropdown previously confused syntactically
valid preview dimensions with playable-world capacity. ENG-126 is still
in progress: successful large-world play requires a separate water capacity
and integration decision. The correction preserves the limit, terrain, water,
world generation version, and authoritative simulation contracts.

## Changes

- `crates/spall_worldgen/src/generate.rs`: check exact wet-column water bounds
  and margins before expensive terrain generation, without allocating water
  cells. Retain the final full-plan gate. Tests compare preflight with real
  water plans; the existing generation golden digest remains unchanged.
- `tools/spall_editor/src/worldgen_panel.rs` and `ui.rs`: retain the preview,
  show its capacity error, disable launch for over-capacity worlds, and show
  launch progress. The default remains 1024 cells (256 m); all authored size
  choices remain previewable.
- `tools/spall_editor/src/main.rs`: retain/poll the launcher, capture stdout
  and stderr, show failed-exit diagnostics, allow retry after exit, and prevent
  duplicate launches. Read only the last 8 KiB, at most four times per second.
- `tools/xtask/src/play.rs`: capture server output including failures before
  `server.jsonl` exists; include the server error in launcher failure output.
- `tools/package-portable.ps1`: remove the misleading five-minute advice and
  document capacity errors and diagnostic locations. Refreshed portable binaries.
- `docs/tasks.md`, `docs/worldgen.md`, `docs/validation.md`, and this report:
  correct the earlier size-support claim and document evidence.

## Exact checks and measured results

- `cargo check -p spall_editor -p xtask`: passed.
- `cargo test -p spall_editor --bin spall-editor`: 16 passed, 1 large check ignored.
- `cargo test -p spall_editor`: 23 library + 16 executable tests passed; includes
  a real failing child, diagnostics shown in editor state, and retry enabled.
- `cargo test -p spall_worldgen`: 4 unit + 15 integration tests passed; large
  preflight and measurement tests ignored by default. Golden generation digest passed.
- `cargo test -p xtask`: 34 passed, 1 diagnostic ignored.
- `cargo clippy -p spall_worldgen -p spall_editor -p xtask --all-targets -- -D warnings`: passed.
- `cargo fmt -p spall_worldgen -p spall_editor -p xtask -- --check`: passed.
- `git diff --check`: passed.
- `cargo test --release -p spall_worldgen large_world_fails_capacity_before_terrain_allocation -- --ignored --nocapture`:
  passed, 0.64 s test runtime, exact 47,421,308-cell capacity failure.
- `cargo test --release -p spall_editor --bin spall-editor over_capacity_preview_is_visible_but_cannot_launch -- --ignored --nocapture`:
  passed, 0.55 s test runtime; preview retained with explicit launch error.
- `& ./tools/package-portable.ps1`: release editor, launcher, server/client
  builds and packaging passed.

Actual packaged process checks, each hidden and bounded, with only owned
processes supervised:

1. `dist/spall-portable/xtask.exe play --portable --worldgen showcase --seed 1
   --worldgen-size 4096 --ticks 1 --startup-timeout-ms 5000
   --output .local/runs/eng126-portable-failure`: exited 3 in 832 ms with the
   exact water-capacity reason forwarded to launcher stderr, no client launched.
2. `dist/spall-portable/sandbox-server.exe --serve --listen 127.0.0.1:0
   --join-token-file .local/runs/eng126-default-world/join.token --worldgen showcase
   --seed 1 --worldgen-size 1024 --ticks 1 --min-clients 0
   --addr-out .local/runs/eng126-default-world/server.addr
   --log-json .local/runs/eng126-default-world/server.jsonl`: exited 0 in 3231 ms;
   generation, bind, readiness, and one server tick passed. Stdout/stderr were
   captured in that directory. This is a server startup check, not GPU gameplay.

No frame-time or large-world success target was achieved by these rejection
tests. Manual editor/GPU interaction remains unrun; seed 1 was used because
the user's exact seed/size was not provided. No full-workspace gate was run.
Next unblocked assignment: ENG-126 capacity/integration decision for the
advertised large sizes; no additional implementation ticket started.
