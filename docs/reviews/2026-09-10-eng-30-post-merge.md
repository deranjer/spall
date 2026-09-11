# ENG-30 post-merge review and follow-ups

Reviewed 2026-09-10. Scope: T23 / ENG-30, increments 1–11, PRs
[#84](https://github.com/deranjer/spall/pull/84) through
[#94](https://github.com/deranjer/spall/pull/94).

## Disposition

The stack is merged; **ENG-30 remains in progress**. GitHub reports all eleven
PRs merged between 23:58:11 and 23:58:19 UTC into
`eaf0b47d9481dae13fc74481103207fae993e189`. Remote `main` resolves to that
commit. The reviewed checkout is `b33496b` on
`feat/eng-30-t23-inc11-client-session-residency-slice-e2`;
`git diff --stat HEAD origin/main` is empty after fetching main. Thus the
reviewed tracked content matches the merge without switching branches.

The stack delivers separated-region destruction, replay, cold restart and
reconnect, expanded crash injection, explicit late-join failure reporting,
logical topology hashes, and opt-in server/client residency. These are useful
correctness increments, not full G3/G4 acceptance. Historical measurements live
in [G3.md](../reports/G3.md); they were not remeasured in this review.

## Priority follow-ups

### P1 — Failed backing validation must leave geometry unchanged

Source finding: [SimWorld::reload_brick](../../crates/spall_sim/src/world.rs)
inserts the loaded brick into the live volume **before** calling
`clear_evicted_after_reload`, which validates the retained digest. A revision
or content mismatch returns an error but leaves the wrong brick resident and
the original digest retained. The logical view rejects this duplicate
membership; `world_hash` and `total_solid_cells` use `expect` on that invariant.
This makes the error path capable of turning a rejected reload into a later
panic instead of preserving the prior authoritative state.

The existing `a_wrong_backing_record_is_refused_and_keeps_the_digest` test
passes, but checks only the error and retained digest. Its comment explicitly
acknowledges that the wrong brick was installed. It does not check residency,
hash, conservation, or recovery after rejection. The later panic was inferred
from source, not executed in this review.

Acceptance: validate a candidate before publication, or restore the original
nonresident state on failure. Extend the regression to require unchanged hash,
solid count, revision and nonresidency after wrong-revision and wrong-content
loads, followed by a successful retry with correct backing. Keep the digest
until the valid install completes. This is the next recommended T23 fix.

### P2 — Make residency/traversal evidence an enforced scenario requirement

[session.rs](../../tools/xtask/src/session.rs) does not deserialize the client's
residency counters or assert residency activity. The
[traversal fixture](../../fixtures/scenarios/t23-g3-traversal.json) requires
only 4 m of movement alongside ordinary movement, transaction, hash, replay and
restart checks. It does not assert a return crossing, that the edited brick
was evicted when the edit arrived, or that a repair actually reloaded it.
Consequently the scenario can remain green if residency stops running while
the fully resident movement and convergence paths still work.

Acceptance: record and assert server/client eviction, completed reloads (not
only requests), outbound and return waypoints, and the target brick's
evicted-at-edit state. Assert preserved post-edit content/revision after repair
and restart. A run with either residency pass disabled must fail these specific
requirements. Preserve paired on/off hash and transaction-order comparisons.

### P2 — Reconcile the frozen residency contract with the delivered scope

[G3-residency-hash.md](../reports/G3-residency-hash.md) requires exact-revision
durable acknowledgment before eviction, dependency-aware pins, and checkpoint
capture that does not reload the entire cache. The delivered
[ResidencyPass](../../crates/spall_server/src/residency_pass.rs) uses a complete
`MemoryBacking`, evicts by player interest and hysteresis without consulting a
durable acknowledgment, and reloads everything before periodic and shutdown
checkpoints in [serve.rs](../../crates/spall_server/src/serve.rs).

This proves logical eviction behavior, but not bounded total memory or the
complete frozen lifecycle contract. Keeping backing geometry in memory does
not establish memory savings. The historical six-brick-budget run itself
reports 24 budget-miss ticks. The report's blanket “not gate blockers” label
for these deferrals must not be read as an accepted waiver of the contract.
No such waiver is granted by this review.

Acceptance: reconcile T18's controller with the live pass, add exact-revision
durable backing, propagate failed reload/capture errors, and capture immutable
live-plus-backed checkpoint state without a full-cache reload. Measure backing,
digest, job and capture memory plus peak process memory and budget pressure.
Alternatively record an explicit, scoped integration decision explaining which
gate criteria remain unmet; do not mark the existing contract fulfilled.

### P2 — Bound client reload work and protect prediction under impairment

[ClientResidencyPass::step](../../crates/spall_client/src/residency.rs) has a
per-brick cooldown but can emit a request for every eligible brick in one
iteration. This is not a global per-step request/byte budget. `over_budget`
exists but is not called by the network mover path. Prediction still depends
on resident geometry; the radius-one loopback lane does not prove safety when
reload traffic is delayed or a player crosses interest rapidly.

Acceptance: globally cap and fairly drain reload work, report client budget
misses and completed loads, and test rapid traversal with delayed/lost repairs.
Pin or obtain the geometry required by the predicted swept region before
using it; retained hashes alone cannot supply collision cells. Missing cells
must not be treated as air. Keep engine/client boundaries intact when sharing
policy with T18; do not add a client dependency on the server crate.

## Remaining gate work

| Work | Required evidence / dependency |
| --- | --- |
| Row 10 live retry/catch-up exhaustion | Existing delayed-connect fixture starts the late client after server shutdown. Add bounded join failure or recovery while the server and other clients remain active, with continued committed progress. Existing evidence is partial. |
| Row 11 join budget | Add bounded byte-rate shaping and compressed baseline size / connect-to-ready timing. Demonstrate <=16 MiB and <=30 s at 1 MiB/s, 100 ms RTT, 2% loss. Do not call full-world transfer regional or count join failure as the normal-join pass. |
| Row 2 full-envelope separation | Exercise >100 m separation and region-to-region return traversal. Current structures differ by 18 m on each horizontal axis; the small walk lane does not cover the full envelope. |
| Rows 12–14 G4 | Eight players clustered and separated; 256 active bodies, 64 near one observer, 4096 sleeping persistent bodies, 10 edits/s, a 4 m blast every 10 s, and a 64-brick connected collapse. Record collider/cell complexity, transport overhead, interest separation and all validation budgets. |
| G4 duration and hardware | 30 s warmup, two measured minutes, then a 30-minute reduced-telemetry soak. Record reference hardware, CPU/GPU/memory/network targets and misses. Short localhost correctness tests are insufficient. |
| Dependencies | T21 / ENG-28 remains in progress. ENG-64 (oversized topology splits) and ENG-62 (full-workload graphical evidence) remain in progress and must be assessed for integrated gate coverage. T15, T17, T18, T20 and T22 are marked done in Loopira; this review does not reaccept them. |

The four residency refinements described as “own tickets” in the implementation
report have no matching dedicated issues in the project listing inspected for
this review. They are recommendations recorded here and on ENG-30, not newly
created issue IDs. T24 / ENG-31 and T25 / ENG-32 remain gated by T23.

## Verification performed

- Loopira: read project guide, ENG-30 and project issue status listing.
- Git/GitHub: `git status --short`, `git log -12 --oneline`, `git remote -v`,
  `git branch --show-current`, `git ls-remote origin refs/heads/main`,
  `git fetch origin main`, `git diff --stat HEAD origin/main`,
  `gh pr list --state merged --limit 15 --json number,title,mergedAt,mergeCommit,url`,
  `gh pr view 94 --json statusCheckRollup`.
- `cargo test -p spall_server --test residency_pass --test client_residency --test logical_baseline`:
  **8 passed, 0 failed**.
- `cargo test -p spall_sim --test logical_reload`: **3 passed, 0 failed**;
  mismatch-test coverage limitation described above.
- Source inspection of residency, reload, canonical hash, scenario acceptance,
  fixtures, and repository specifications; documentation whitespace check via
  `git diff --check`.

At inspection, PR #94 showed one successful `foundation` check and another
still in progress. This is a point-in-time observation, not a final CI verdict.
Full workspace fmt/clippy/tests, multi-process scenarios, crash suite, GPU
capture and performance/soak gates were **not rerun**. Earlier green claims
remain historical evidence only.

Changed files in this review: this document and linked review notices in
`docs/reports/G3.md`, `docs/reports/G3-residency-hash.md`, `docs/tasks.md`, and
`docs/validation.md`. No engine behavior changed. Existing untracked user work
was preserved. Next unblocked assignment: **T23 / ENG-30 — atomic failed-reload
handling**, then enforce residency scenario assertions; join-budget tooling is
the next independent gate slice.
