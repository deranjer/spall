# G3 open-items handoff — 2026-09-11

Session context: "get PRs up for the open items" in `docs/reports/G3.md`. Four
increments were dispatched in parallel as isolated-worktree agents, resumed
once after an earlier kill. Three finished and have open PRs; the fourth
(row 8b) was mid-verification when the session hit its Claude monthly spend
limit (resets 1:20pm America/New_York) and stopped. This doc is the handoff
so that row can be picked back up without re-deriving the diagnosis.

## Status summary

| Row | Item | Status | PR |
| --- | --- | --- | --- |
| 2 | Full-envelope (>100 m) player separation + region traversal | **done** | [#99](https://github.com/deranjer/spall/pull/99) → `main` |
| 8b | Prediction-safe traversal under `--loss-percent 2` | **unfinished — see below** | none yet |
| 11 | Join-budget size/wall-clock capture (≤16 MiB / ≤30 s) | **done, partial evidence** | [#97](https://github.com/deranjer/spall/pull/97) → `main` |
| 12 | G4 eight-client workload scene | **done, partial evidence** | [#98](https://github.com/deranjer/spall/pull/98) → `main` |

All three open PRs are unreviewed and unmerged. **All three used "increment
13"** in `docs/reports/G3.md` — an expected numbering collision (agents were
told not to worry about it) that needs manual renumbering when reconciling
these into one sequence, along with likely conflicts in `docs/reports/G3.md`
itself and possibly shared harness code (`tools/xtask/src/session.rs`,
`ClientSummary`/`ServeSummary` fields) since all three branched independently
from the same base (`main` @ `3481a68`, which already includes PR #96 /
increment 12 and PR #83).

New open items filed by the row-2 agent while investigating (not yet fixed,
just discovered and documented in its PR / `docs/reports/G3.md`):
- **Row 15** — a freshly-spawned player on a *bounded* volume doesn't respond
  to horizontal input for several hundred ticks if spawned within a few
  metres of `x=0`. Reproduces on the already-merged 18 m separated-regions
  scene too; never triggered before because no prior scenario scripted
  movement on a bounded-volume scene. Mitigated in PR #99 (spawn moved to
  `x=7m`), root cause (suspected broad-phase/query-pipeline warm-up
  interaction) **not fixed**.
- **Row 16** — the client's own movement-summary metric
  (`distance_travelled_m`) is intermittently unreliable: 1 of 3 identical
  runs under-reported it despite a correct final position and matching
  hashes everywhere (a predictor-bookkeeping race, not a sim/replication
  bug). Given the same "known intermittent, rerun once" treatment as the
  existing `spall_net::separate_process_transport` flake.

## Row 8b — where the work is and what's left

**Worktree:** `G:\Programming\voxel_engine\.claude\worktrees\agent-a3d696aac8161ec90`
**Current branch:** `worktree-agent-a3d696aac8161ec90` (needs renaming — see
Git/PR steps below)
**Base:** `main` @ `3481a68` (already has PR #96/increment 12 + PR #83)
**State:** all changes below are uncommitted in the worktree's working tree.
Run `git status` / `git diff` there first to confirm nothing has moved since
this was written.

### Diagnosis already done (do not re-derive)

Root-caused the `t23-g3-traversal --loss-percent 2` failure to **three**
separate real defects in how the client handles an impaired transport during
scripted movement, not a fixture-tolerance problem:

1. **One failed baseline transfer used to kill the whole control-record
   loop.** `receive_baseline_body` returning `None` (bulk stream, decode, or
   `BaselineEnd` hash-check failure — expected occasionally under loss) used
   to `break` out of the loop that reads *every subsequent* control record on
   that connection, so one dropped repair patch also silently ate every
   later repair response, including ones for requests sent well after the
   failure. Fixed: count it (`baseline_transfer_failures`) and keep reading;
   the requester (residency pass cooldown, or a gapped transaction's own
   retry) already re-requests on its own schedule.

2. **Prediction advanced over not-actually-resident ground.** The collider
   body on `ClientPhysics` persists across a residency eviction gap (kept
   empty for a later refill), so `has_terrain()` alone stayed true even when
   the brick under the player's own feet hadn't reloaded yet after a lossy
   repair round trip — the mover predicted through that as confirmed air,
   turning one delayed reload into unbounded free-fall. Fixed: `ClientPhysics`
   now tracks which bricks its collider was actually built from
   (`resident_bricks`) and exposes `covers(feet_m)`; the mover loop only
   predicts/sends input when `covers()` is true for the player's predicted
   position, holding (not falling) through a gap the same way it already
   holds before the player exists. A new `lenient_occupancy()` builds the
   collider over the bounding box of whatever *is* resident, treating a
   non-resident cell as "unknown, no collision" instead of failing collider
   extraction outright over a routine one-or-two-brick residency gap.

3. **Movement scripts were timed off the wrong clock.** Scripts are authored
   relative to "ticks since this client's player went live", but were being
   fed the server's absolute tick. A clean/fast join has the two coincide; an
   impaired join (baseline transfer + its ack + the resulting keyframe
   snapshot, each its own round trip under loss) stretches the join
   handshake well past when the script expects to start, stranding the
   mover. Fixed: `Predictor::script_origin_tick` anchors on the tick of the
   *first authoritative snapshot* for that player (the true "player exists"
   moment), and `scripted_input` is fed `tick - script_origin_tick`.

Supporting changes:
- `spall_server/src/serve.rs`: the server no longer declares the run
  quiescent while any client is still mid-baseline (`LateJoin::any_joining`)
  — a bulk transfer under loss can legitimately outlast `quiescence_ticks` of
  otherwise-idle simulation, and ending the run early used to strand that
  client's connection before it ever got to replicate or run its script.
- `spall_client/src/residency.rs`: `RELOAD_COOLDOWN_STEPS` 24 → 8 (faster
  repair-request retry cadence, since each individual request is now cheaper
  to lose without consequence per fix #1 above).
- `spall_client/src/net.rs`: the settle-detection window widened 180 → 360
  script-relative ticks (a late reload near the end of the script needs more
  genuinely-neutral time to settle before the client stops sending input);
  the at-rest check for the movement summary now reads the *authoritative*
  state rather than the predicted one (prediction only advances while
  `covers()` holds, so it can be legitimately frozen mid-settle at the
  moment the run ends if that coincides with a stall).
- `spall_sim/src/sim.rs`: a `SPALL_DEBUG_PLAYER` env-gated per-tick player
  position/velocity/grounded eprintln, added for diagnosis. Harmless to keep
  or trivial to strip.
- `fixtures/scenarios/t23-g3-traversal.json`: `server_ticks` 820 → 1100 (more
  headroom for the slower, loss-impaired run).

At the point of the kill, the agent had just finished **clean** (no-loss)
regression passes and was starting **"the critical loss test with more
runs"** — i.e. it had not yet confirmed `--loss-percent 2` passes reliably
across multiple runs/seeds. That confirmation, and everything downstream of
it, is what's left.

### What's left to do

1. `cd` into the worktree above. Confirm the diff described here still
   matches (`git status`, `git diff`) — nothing else should have touched it.
2. Run `cargo xtask scenario --name t23-g3-traversal --loss-percent 2`
   several times (at least 3, ideally with different seeds if the harness
   supports `--seed`) and confirm it passes consistently. Inspect
   `baseline_transfer_failures` in the summary to sanity-check it's
   non-zero-but-recovering under loss rather than zero (which would suggest
   the loss injection itself isn't exercising the fixed path).
3. Confirm no regression: rerun `t23-g3-traversal` with no loss, plus
   `t23-g3` and `t23-g3-residency`.
4. Add or finish a CPU-level test exercising the loss-tolerant retry path
   deterministically (simulated drops), not just the multi-process scenario
   — this was called for in the original task and there's no evidence yet
   it was written. Look for it under `crates/spall_client/tests/` or
   wherever residency/repair-retry tests already live.
5. Decide whether to strip or gate the `SPALL_DEBUG_PLAYER` eprintln more
   tightly before committing (it's harmless behind the env var, but check
   it matches the codebase's existing logging conventions rather than a raw
   eprintln if there's a precedent — e.g. `tracing`).
6. Run the full required checks: `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --all-features` (the `spall_net` two-process test
   is the known intermittent flake — rerun once if it alone fails).
7. Append a new `## Increment N` section to `docs/reports/G3.md` — **check
   the file first**, since PRs #97/#98/#99 all landed "increment 13"
   independently; pick the next number after whatever's highest when you
   look, a further collision with a sibling PR is fine and will be
   reconciled at merge time. Update the row 8b line in the Open Items table
   with the real outcome. Add the loss-percent-2 command to `## Reproduce`.
8. Also note in that section, if true: filed rows 15/16 above are
   independent of this fix and remain open regardless of row 8b's outcome.
9. Git: `git branch -m feat/eng-30-t23-inc-impaired-traversal`, commit
   (conventional message, e.g. `fix(t23): loss-tolerant client residency
   reload retry (ENG-30, increment N)`), ending the commit message with:
   ```
   Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
   Claude-Session: https://claude.ai/code/session_013kiw56wqmGoiD6mcpqaZjx
   ```
   Push: `git push -u origin feat/eng-30-t23-inc-impaired-traversal`. Open a
   PR: `gh pr create --base main --head feat/eng-30-t23-inc-impaired-traversal
   --title "T23: loss-tolerant residency traversal (ENG-30, increment N)"
   --body "..."`, body ending with:
   ```
   🤖 Generated with [Claude Code](https://claude.com/claude-code)

   https://claude.ai/code/session_013kiw56wqmGoiD6mcpqaZjx
   ```
10. Loopira: `mcp__loopira__add_work_log` on the "Voxel Engine" project
    citing ENG-30 — changed files, checks run + results, the three root
    causes and fixes, before/after evidence for `--loss-percent 2`,
    remaining risks, PR URL. Do not change ENG-30's ticket status.

## After all four land

Someone (a coordinator pass, not a worker) needs to:
- Reconcile the "increment 13" collision across PRs #97, #98, #99, and
  whatever number row 8b lands as, into one consistent sequence in
  `docs/reports/G3.md` once merge order is decided.
- Resolve merge conflicts in `docs/reports/G3.md` and any shared harness
  files touched by more than one PR.
- Decide whether rows 15/16 (from PR #99) get their own fix tickets or ride
  along as noted follow-ups.
- Re-run the full regression set once everything is merged to `main`, since
  each PR was only checked against `main`+its own changes, not against the
  combination of all four.
