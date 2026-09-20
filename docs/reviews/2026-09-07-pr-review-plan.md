# Spall PR review, repairs, and integration plan

Reviewed 2026-09-07. Scope: T00 re-check and every open PR in `deranjer/spall`, mapped to the Voxel Engine project in Loopira. All seven PR branches have received the reviewed fixes below. No PR has been merged. All current GitHub foundation checks passed for every published head above (final check completed 2026-09-07 19:21 UTC). The combined local check passed too. No unresolved review blocker remains for these task scopes.

## PR disposition

| PR | Ticket / task | Original head | Published repaired head | Changes made |
| --- | --- | --- | --- | --- |
| [#1](https://github.com/deranjer/spall/pull/1) | ENG-7 / T00 | 11568c5 | 69840c3 | Resolve relative Cargo target paths correctly; document implemented launch/check commands. |
| [#2](https://github.com/deranjer/spall/pull/2) | ENG-8 / T01 | 029f67d | 83d9a96 | Validate deserialized brushes/manifests, reject overflowing cell runs, enforce handshake limits/rates, support full-size bulk codecs, provide manifest-aware topology decoding. |
| [#3](https://github.com/deranjer/spall/pull/3) | ENG-9 / T02 | f82c00e | 65b7eb6 | Preserve edit atomicity at revision exhaustion, use canonical edit ordering, advance allocation after loading higher revisions. |
| [#4](https://github.com/deranjer/spall/pull/4) | ENG-10 / T03 | f5204c5 | 72270cd | Handle extreme ray ranges and fixed-point brush centres; adapt the new access error and canonical revision fixture digest. |
| [#5](https://github.com/deranjer/spall/pull/5) | ENG-11 / T04 | ee98219 | 3dd4619 | Bound queued/running/completed retained job costs and completed result counts; resume dispatch after draining results; reject byte-budget overflow. |
| [#6](https://github.com/deranjer/spall/pull/6) | ENG-14 / T07 | f8ecb90 | 03bd00a | Track absent/failed boundary dependencies in structural job tokens; regress absent/failed/resident-air transitions. |
| [#7](https://github.com/deranjer/spall/pull/7) | ENG-16 / T09 | 33c358f | 9a93c46 | Repair authentication, session/stream/bulk bounds, LAN binding, liveness/proxy ownership, metrics, and real process/fault acceptance. |

All updates preserve the original branch history. Earlier fixes were merged into dependent PR branches in dependency order, with each repair applied to its originating PR. The user's current T09 checkout and untracked `.codex/` configuration were preserved; remote PR updates do not automatically update that checkout.

## Findings and completed action plan

1. **T00 harness path resolution.** Launching the compiled xtask from `examples/sandbox` with `CARGO_TARGET_DIR=target` built successfully but searched for the server under the wrong directory. The same reproduction passes after resolving paths against Cargo's workspace working directory. The previous specification-only README statement now identifies the actual implemented commands.
2. **T01 untrusted values.** Derived Deserialize bypassed the checked sphere-radius and material-manifest constructors. The manifest's binary search could then operate on unsorted input. Deserialization now enters through validation. CellRun is explicitly contiguous +X with fixed Y/Z and a checked final i64 coordinate. A new bounded bulk codec permits the entire 1 MiB payload plus metadata; control remains 64 KiB. Registry-aware `decode_topology` rejects unknown materials before world application; generic decoding deliberately remains structural. Both handshake records validate their limits, and snapshot rates must match as well as server tick rates.
3. **T02 revision invariants.** Exhausting revisions could remove a resident brick before returning an error. Allocation is fully preflighted. Loaded brick revisions now advance the volume high-water mark. Edit outcomes and allocation follow documented `(z,y,x)` ordering. The cross-brick tower's pinned digest was updated because it includes revisions; the geometry is unchanged.
4. **T03 extreme coordinates.** Huge finite/infinite range conversion could overflow the ray step budget. Saturating budget arithmetic fixes it. Saturating cell-centre arithmetic could incorrectly include cells in zero-radius brushes at i64 extrema. Candidate centres now remain widened through the inclusion predicate; tests cover both extremes.
5. **T04 memory backpressure.** Admission previously charged only queued inputs, even when a job still retained them while running. Completed outputs could accumulate when installation stopped. Declared conservative job cost is charged once across all retained states, completed slots are bounded, and installation wakes newly eligible work. Near-u64::MAX admission is checked without overflow. These are declared-cost budgets, not a claim to measure every allocation a closure makes.
6. **T07 stale support decisions.** All-resident absent boundaries and failed-load boundaries were missing from tokens. Their exact observed states now invalidate results when they change. The initial suspicion about resident-air omission was corrected during review: resident-air revisions were already captured; an explicit air-to-solid regression now protects that behavior.
7. **T09 transport repairs.** Join tokens now use the OS CSPRNG. Sequence-window arithmetic cannot wrap; reliable sequence assignment and stream writes share a lock. Both endpoints install accepted limits, validate server replies, and bound establishment/authentication time. Client bind addresses permit LAN routing and match the target address family. Live server connection slots are bounded, released on drop, and reused with increasing generations. Received input datagrams must carry the assigned session. Bulk collection enforces payload/assembly/count limits, one transfer ID, ordered indices and part hashes, including full 1 MiB payloads. Delayed proxy traffic lives in one owned queue capped at 1024 packets and 2 MiB. Liveness exits on owner/connection shutdown, and a stalled heartbeat write closes the connection. Harness tasks are owned across errors/timeouts. Application-byte metrics now count observed bytes instead of message counts.
8. **Acceptance gaps filled.** Decoded records now exercise deterministic application faults separately from encrypted packet faults. A supervised test launches five OS processes: one server, two clients, and two UDP proxies. Both clients authenticate and exchange control, bulk and datagram records through packet loss/delay. Every child is deadline-bounded and killed/reaped on failure. The ignored `process_role` item is the child entry point invoked by this parent test, not an omitted acceptance scenario.

## Delegation and review

Three GPT-5.6 Terra workers reviewed independent areas in dedicated worktrees: core/storage, geometry/jobs/structure, and transport. They reported findings before authorized edits and saved focused commits. All three eventually hit the account usage limit. The coordinator inspected their patches, completed partial changes locally, added missing regressions, repaired integration failures, and ran the final combined validation.

Original worker/coordinator repair commits remain available locally for audit: `0e42443`, `703640b`, `d2ab869`, `aa5fbd0`, `dbb03df`, `4fc97cf`, `c62800e`, `6baef4c`, `2db9b10`, `c80b4c6`. The published PR table gives the branch heads after dependency integration and replay of those repairs.

## Verification evidence

- **T00 exact original implementation:** `cargo xtask check` passed. Fresh `cargo xtask smoke --ticks 60 --graphical` passed on Windows, RTX 4080 SUPER, Vulkan; three frames were presented and scripted resize was observed. Failure injection and delayed-readiness timeout returned failure summaries and left no owned child running. Relative-target reproduction failed before the fix and passed afterward. Earlier native-close evidence remains under `.local/reviews/t00/window-close.jsonl`.
- **Integrated geometry/structure:** `cargo xtask check` passed: formatting, Clippy with warnings denied, all features, 161 tests.
- **Transport branch:** `cargo xtask check` passed: 112 tests, with the child-process entry point separately marked ignored and exercised by its parent.
- **All seven together:** local integration commit `f8cbdcb` passed `cargo xtask check`: **192 tests, zero failures**, one child entry point marked ignored. This includes the actual five-process acceptance test.
- **Measured packet impairment:** `cargo xtask net-check --clients 2 --records 20 --datagrams 30 --loss-percent 5 --output .../net-final` passed. Two clients, 40/40 reliable replies, six bulk parts, 60 datagram replies, two duplicate drops. This run observed two proxy packet drops; 5% is the configured stochastic loss probability, not a claimed exact realized ratio. Captured application-byte counters were 5,342 sent / 8,610 received and Quinn transport counters 21,958 / 21,300 for that run.

Local artifacts under `G:/Programming/voxel_engine/.local/reviews/`:

- `t00-fresh/graphical/`, `t00-fresh/relative-target/`, `t00-fresh/relative-target-fixed/`
- `structure-check.log`, `network-check.log`, `combined-check.log`
- `net-final/summary.json`, `net-final/metrics.json`, `net-final/net.jsonl`
- `net-published-head/` repeats the run at exact published head 9a93c46 with updated reporting: 40/40 reliable replies, six bulk parts, 48 lossy datagram replies, two proxy drops. Datagram delivery varies with packet timing.

The native Vulkan validation layer was unavailable; this review does not claim validation-layer coverage. GPU feasibility/performance gates beyond the basic T00 window were not run. No G1 destruction or replication feasibility result is implied by transport success. Sandbox-host replication is T10, collision feasibility is T06, and bounded decompression remains with storage/T16 because T09 does not decompress payloads.

## Remaining integration steps

1. **Completed:** GitHub checks are green for the exact repaired heads above. Re-check if any branch changes after this review.
2. Merge through the dependency chain: #1, #2, #3, then #4 → #5 → #6; #7 can follow #3 independently. These are stacked PRs, so check/retarget each base to `main` once its prerequisite has actually landed. Do not accidentally merge only into an obsolete task branch.
3. When the geometry and transport branches meet, retain both workspace members and dependency entries. The combined check resolved `Cargo.toml`, `Cargo.lock`, and `docs/dependencies.md` this way without updating external versions. Local branch `review/integrated-all-prs` at `f8cbdcb` contains the tested resolution. Revalidate the eventual merged tree if it differs.
4. All PRs remain open; this review did not merge them. Once the prerequisite chain is integrated, the next independent implementation tasks are T05 (visible voxel baseline) and T06 (editable collision feasibility). T08 follows T06/T07, and T10 follows T08/T09. Full-world destruction and authoritative multiplayer remain required.

## Loopira final disposition

ENG-7, ENG-8, ENG-9, ENG-10, ENG-11, ENG-14 and ENG-16 are marked done after coordinator review, local integration and green GitHub checks. Each ticket explicitly states that its PR remains open/unmerged. The project work log records the exact commits, evidence and next task dependencies.

