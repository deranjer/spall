> Historical partial validation. Completion evidence and current contracts are in [ENG-105](ENG-105.md).

# ENG-105 replication validation — 2026-09-29

Bounded follow-up to merged PR #167, not acceptance of ENG-105. The existing
implementation supplies server water, optional coarsening/worker execution,
full-domain presentation keyframes, gated springs, dam gates and admin reset.

## Correction

`spall_protocol/src/water.rs` now validates the public assembler input before
staging or allocating decoded fractions. Invalid headers discard partial state.
Nonfinal chunks must have the encoder's full chunk size; empty domains fail
encoding. No DTO field, wire tag or schema version changes. Four regression
tests cover malformed direct API input, short chunks, bounded decompression and
corruption followed by repair, and empty domains. Before the fix, three of the
new tests failed; the existing bounded decompression behavior already passed.

## Checks and measured results

- `cargo fmt -p spall_protocol`: completed.
- `cargo test -p spall_protocol`: 48 passed (7 water tests).
- `cargo clippy -p spall_protocol --all-targets -- -D warnings`: passed.
- `cargo test -p spall_server --test water_replication -- --nocapture`:
  2 passed in 7.04 s. Real QUIC client receives continuous keyframes, a dam cut
  replicates, authorized scene reset restores matching terrain, and reset is
  refused when disabled. This is functional evidence, not a bandwidth result.
- `cargo test -p spall_fluid boundary_ -- --nocapture`: 6 passed. The isolated
  cavity test confirms displacement reports capacity failure and retains the
  fluid arrays; it does not meet the always-succeed placement requirement.

## Acceptance gaps and next bounded work

The ticket remains in progress. Full-domain keyframes are implemented instead
of the specified per-brick snapshots/deltas. Late join receives the cached/new
frame after its baseline barrier. There is a 15 Hz cadence and a 3 MiB compressed
frame cap, but no practical byte-rate scheduler or measured bandwidth gate.
Poorly compressible maximum-size domains can exceed the compressed cap, and
the current server treats encoding failure as a run error.

The ticket's always-succeed placement decision is retained as a requirement.
Current displacement fails when a sealed pocket has no connected capacity;
the prior report incorrectly suggested all requested placement cases were met.
An incompressible fraction grid cannot fit the same volume into fewer cells
when every remaining cell is full. A conservative overflow/capacity policy
needs an explicit integration decision; do not delete water or route it through
solid walls to claim success.

Worker mode mutates its grid and publishes frames asynchronously; it does not
validate terrain revisions and publish on the owning simulation thread at a
tick boundary. Thus strict same-tick authority remains incomplete. Persistence,
crash recovery, dormancy, unknown-region exchange and several active regions
remain open. No GPU/hardware presentation, sustained timing, bandwidth, or
workspace-wide gate was run in this pass. Next unblocked assignment: ENG-105
increment 2 per-brick replication and byte-budget scheduling; strict worker
ordering and capacity policy are prerequisites for increment 1 acceptance.
