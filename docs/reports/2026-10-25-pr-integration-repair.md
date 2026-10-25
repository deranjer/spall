# PR integration repair — 2026-10-25

The merged-state badge on a stacked PR did not establish main ancestry.
#239, #240 and #241 had merged into #238's branch; #245 had merged into
#243's branch. Repaired and merged #238 as `d317816d`, then retargeted and
merged #243 as `7b20ce07`. Both remote and local main now contain every
audited merge and head commit from #233–#237, #239–#241 and #245.

The remaining PRs #242, #244, #246 and #247 were repaired against that main
and remain open for review. Their bases are main.

## Changes

- #238: integrated pacing and world items; wire 23, save 9.
- #243: preserved compass/note/fire pack actions and per-player retry IDs;
  replies translate back to client IDs. Its network fire test now also proves
  that a second player's identical request ID places a separate fire.
  Wire 24, save 10; #245 flame visuals are included.
- #242: combined tool selection with the pack actions, translated immediate
  tool answers and refusals, and disambiguated the two snow-depth queries.
  Preserved both tool and fire hooks, configuration literals and network tests.
- #244: preserved the configured day clock and generated item scatter alongside
  the winter weather/population profile.
- #246: retained per-player request routing and health publication after
  creature damage. Replaced the removed fixed-clock temperature call with the
  host's configured day clock, including recovery. Wire 25, save 10.
- #247: retained both ice and world-item fields, initialization and reset logic.
  Inherits wire 24/save 10; this repair adds no ice format change.

## Checks actually run

All four remaining branches passed `cargo fmt --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, and
`cargo run -p xtask -- proportionality` (equivalent to `cargo xtask proportionality`).
The existing ungated store fault-module warning remains; no allowlist grew.

| Branch | Exact test command | Passed / ignored |
| --- | --- | --- |
| #238 | `cargo test -p sandbox --features client --test world_items --test world_scatter --test progression_network --test cabin_doors_network --test tower` | 15 / 1 |
| #238 | `cargo test -p spall_server --test world_item_persistence` | 1 / 0 |
| #238 | `cargo test -p spall_client --test snow_prediction --test ladder_prediction` | 3 / 0 |
| #243 | `cargo test -p sandbox --features client --test world_items --lib` | 74 / 0 |
| #243 | `cargo test -p spall_server --test world_item_persistence` | 1 / 0 |
| #242 | `cargo test -p spall_server -p spall_sim -p spall_client -p sandbox --features client --lib` | 503 / 9 |
| #242 | `cargo test -p sandbox --features client --test world_items` | 6 / 0 |
| #244 | `cargo test -p spall_protocol -p spall_sim -p sandbox --features client --lib` | 274 / 6 |
| #246 | `cargo test -p spall_sim -p spall_protocol -p spall_server -p spall_client -p sandbox --features client --lib` | 579 / 9 |
| #247 | `cargo test -p spall_sim --test ice` | 6 / 0 |
| #247 | `cargo test -p spall_server --test ice_persistence` | 1 / 0 |
| #247 | `cargo test -p sandbox --features client --test appearance --test appearance_evolution` | 9 / 0 |

#238 also passed `cargo check --workspace --all-targets --all-features` and
formatting. #243 passed workspace check, clippy, formatting and proportionality.
Tests requiring local server/filesystem resources ran outside the sandbox after
an initial access-denied failure. Initial builds exhausted G:; only this run's
incremental cache was removed and its build output moved to the task's writable
C: directory. Workspace packages were cleaned between branches to avoid stale
artifacts from different worktree paths. Raw logs remain ignored locally.

## Scope and remaining work

The new public `Simulation::set_day_clock` has a production caller in
`spall_server::serve::tick_loop`; its private state feeds authoritative warmth.
The renamed `SnowState::stored_depth_q16_at` retains its production caller in
`Simulation::snow_depth_m_at`. No new crate, module, feature, config field or
protocol record was introduced by these repairs. Only the existing wire
number was renumbered for the combined layouts; the feature branches' existing
save changes remain 9 and 10. No proportionality exception was taken.

These are measured CPU correctness/build results, not performance or visual
acceptance. No GPU capture or hands-on playtest ran. Existing feature limitations
in the PRs remain. Hosted CI runs separately. Next integration choices are the
now-ready open PRs; ENG-251 remains the scenario assembly follow-up.
