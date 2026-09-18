# ENG-69 interactive-input acceptance record

Status: audit correction verified by focused automated checks. This report
distinguishes the merged PR #109 record from checks performed by this audit.

## Scope

ENG-69 is the T19 follow-up that connects a live window's keyboard and mouse
input to the existing QUIC player-prediction session. It owns the minimal
nearby-terrain debug view needed to make the controlled world visible. It does
not establish the real G2 renderer or draw other players' capsules.

The entry point is `cargo xtask play --release --scene g1 --ticks 18000`.
`sandbox-client --connect --interactive` uses WASD, Space, and mouse look;
Escape releases the cursor, and closing the window requests session shutdown.

## Historical evidence from PR #109

[PR #109](https://github.com/deranjer/spall/pull/109) (merge `ddb579a`) records
a hands-on user run of the command above and states that the investigated
frame-pacing jitter was gone after the pacing and prediction fixes. This is
historical, manually observed evidence attributed to that PR. It was not re-run
by this audit, and it must not be read as a new measurement or an automated
gate result.

The merge also contains focused automated coverage for prediction timing,
reconciliation, and the bounded character-query window. Those tests support
the implementation but do not synthesize physical keyboard or mouse input.

## Current audit checks

- `cargo fmt --all --check` — passed.
- `cargo test -p spall_client
  focus_loss_clears_window_keys_and_shared_actions_but_keeps_look` — passed.
- `cargo xtask scenario --name player-movement --loss-percent 0 --output
  .local/runs/eng-69-audit-player-movement` — passed: both clients and the
  server converged to
  `ac84d03d67117b1300cde753e77c7191e4a733d1929d696294895802eb8f2d16` after
  420 ticks; each client applied one transaction and observed 282 motion
  snapshots.

The scenario is headless scripted-movement regression coverage. It does not
measure interactive input, frame pacing, or rendering quality.

## Audit correction

This audit found that a focus change could omit the operating system's matching
key/button releases, leaving the local client with a held walk or jump and a
captured cursor. The live window now clears held actions and releases its cursor
on `WindowEvent::Focused(false)`. `LiveInput` has a unit test proving that this
focus-loss transition neutralizes both the window's held keys and the shared
movement/buttons while preserving the last look direction. On focus regain the
window accepts new keyboard events normally; it never revives the cleared state
without a new event.

## Remaining limitations

Coordinator integration verification, 2026-09-18: `cargo xtask check` passed
with both ticket fixes and the existing `9b1e190` transport-test correction.
The new focus-loss test ran successfully in that complete workspace suite.
The original ENG-69 implementation and this focused correction are ready for
merge; the correction is not yet published to `main`.

- No new hands-on desktop run was performed here; the audit did not acquire the
  desktop cursor.
- There is no automated physical-input or focus-transition scenario; the new
  unit test covers the input-state invariant only.
- The renderer remains the ENG-69 debug cube view. G2 lighting/material
  presentation and other-player capsules remain separate follow-up work.
