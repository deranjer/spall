# ENG-66 — rapier3d version check for row 15 (T23 / G3)

Follow-up to T23 / G3 row 15 (first reported by PR #99, root-caused in PR #106
/ `docs/reports/G3.md` increment 20): a freshly-created authoritative player
capsule spawned within roughly the first few metres of a bounded volume's
`x = 0` origin, near a floor plus a second, wide/thin *elevated* structure
("beam"), does not respond to horizontal input for hundreds of ticks —
`grounded` stays `true`, wish-velocity is computed correctly every tick, but
`rapier3d::control::KinematicCharacterController::move_shape`'s returned
translation is `[0, 0, 0]` indefinitely. Root-caused to `rapier3d` itself, not
this codebase; mitigated (not fixed) by spawning at `x = 7 m` instead of
`x = 1 m` (`fixtures::SEPARATED_REGION_FAR_SPAWNS`). ENG-66 was scoped to check
whether a newer `rapier3d` release has since fixed it.

**Outcome: no fix available. `rapier3d 0.35.3` — the version already pinned in
the workspace `Cargo.toml` — is still the latest released version on
crates.io as of this check (2026-09-17). No code or dependency changes were
made. The regression test stays `#[ignore]`d, and
`fixtures::SEPARATED_REGION_FAR_SPAWNS`'s `x = 7 m` mitigation stays in
place.**

## What was checked

**crates.io.** `https://crates.io/api/v1/crates/rapier3d` lists, most recent
first:

| Version | Release date |
| --- | --- |
| 0.35.3 | 2026-08-28 |
| 0.35.2 | 2026-08-15 |
| 0.35.1 | 2026-08-08 |
| 0.35.0 | 2026-08-08 |
| 0.35.0-glamx0.2 | 2026-08-08 |
| 0.35.0-beta.0 | 2026-08-02 |
| 0.34.0 | 2026-07-04 |
| 0.33.0 | 2026-06-05 |

`0.35.3` — the workspace's current pin (`Cargo.toml` line ~34) — is the
newest entry. There is no `0.35.4`, no `0.36.0`, and no newer prerelease.

**`dimforge/rapier` CHANGELOG.md** (`master` branch,
`https://raw.githubusercontent.com/dimforge/rapier/master/CHANGELOG.md`):
top entry is `## v0.35.3 (28 August 2026)`, matching crates.io exactly — the
changelog has nothing past the version already pinned here.

**`dimforge/rapier` GitHub Releases page**: empty ("There aren't any releases
here") — this project publishes its changelog in-repo rather than as GitHub
Releases, consistent with the crates.io/CHANGELOG.md agreement above being
the authoritative source.

**Relevant character-controller history already included in 0.35.3** (i.e.
already in effect when row 15 was root-caused, so these are not new fixes to
try — noted here for completeness): `v0.35.0`'s "The character controller's
snap-to-ground now triggers on any movement that isn't upwards... Purely
lateral movement along the ground no longer flickers the `grounded` flag when
snap-to-ground is enabled" and "The broad phase now filters candidate pairs by
collision groups, so colliders that can never interact don't reach the narrow
phase at all". Neither describes a translation-returned-as-zero freeze; both
predate and are already present in the pinned `0.35.3` under which row 15 was
isolated.

**Upstream issue search** (GitHub, `dimforge/rapier` and `dimforge/bevy_rapier`):
no open or closed issue matches this exact symptom (translation permanently
`[0,0,0]` near a bounded volume's origin next to an elevated thin structure).
The closest adjacent reports are different failure modes on the same
controller:
- [`dimforge/rapier#485`](https://github.com/dimforge/rapier/issues/485) —
  `move_shape` with a zero `desired_translation` skips the internal loop
  entirely, so the character stops being pushed by a moving body. Different
  trigger (a *zero* desired translation, not a nonzero one that gets
  discarded) and different symptom (fails to be pushed, not fails to move
  itself).
- [`dimforge/bevy_rapier#301`](https://github.com/dimforge/bevy_rapier/issues/301)
  and [`#489`](https://github.com/dimforge/bevy_rapier/issues/489) — the
  controller getting stuck against vertical walls at certain approach angles.
  Different geometry (a wall the character walks into, not a floor + elevated
  beam near an origin it hasn't touched) and the "stuck on walls" class of bug
  was already addressed once before (`v0.17.2`, `v0.19.0`'s
  `normal_nudge_factor`) — this is a further, apparently still-open variant of
  that class, but not row 15's reported geometry.
- [`dimforge/rapier#488`](https://github.com/dimforge/rapier/issues/488) — a
  character controller dipping into a moving vertical platform. Also
  unrelated (row 15's floor and beam are both static/fixed bodies).

None of these is close enough to file row 15 as "the same bug" against, and
none suggests an existing fix to pull forward. Filing a fresh upstream issue
with the isolated repro (`crates/spall_physics/src/character.rs`'s
`row15_elevated_beam_near_origin_freezes_horizontal_movement` doc comment
already contains the full ruled-out-causes writeup) remains the next concrete
step whenever someone picks that up; this ticket did not file it (out of
scope — ENG-66 was a version-check pass, not an upstream-engagement pass).

## Dependency compatibility (recorded, not exercised — no bump was attempted)

Confirmed `rapier3d` has exactly one consumer in the workspace
(`crates/spall_physics`, `rapier3d.workspace = true`) and no other crate
constrains it — `cargo tree -p spall_physics -i rapier3d` shows only that one
edge. `Cargo.lock` has it locked with `parry3d 0.30.2` and `nalgebra 0.35.0`,
the versions `rapier3d 0.35.3` itself requires. Since no newer release exists,
none of this needed to be tested against a bump.

## Regression evidence (current behaviour, unchanged)

```
cargo test -p spall_physics --lib row15 -- --ignored --nocapture
```

```
running 1 test

thread 'character::tests::row15_elevated_beam_near_origin_freezes_horizontal_movement' panicked at crates\spall_physics\src\character.rs:359:9:
expected this to fail today (row 15): capsule should have moved but stayed frozen at x=1 after 200 ticks of forward input
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test character::tests::row15_elevated_beam_near_origin_freezes_horizontal_movement ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 45 filtered out; finished in 0.80s
```

Still fails exactly as `docs/reports/G3.md` increment 20 recorded — the
defect is still present under `rapier3d 0.35.3`. No other checks
(`cargo fmt`, `cargo clippy`, `cargo test --workspace`) were run because no
dependency or code change was made; the full check suite from the ticket's
suggested workflow only applies to the "a fix was found" branch, which did
not occur here.

## Recommendation

- **No dependency bump.** There is nothing to bump to.
- **Keep `fixtures::SEPARATED_REGION_FAR_SPAWNS`'s `x = 7 m` mitigation
  exactly as-is.** No new evidence changes the `x >= ~6.5 m` empirical
  threshold from increment 20.
- **Keep the `#[ignore]`d regression test as the tripwire.** Re-run
  `cargo test -p spall_physics --lib row15 -- --ignored --nocapture` the next
  time a `rapier3d` bump is considered for any other reason (a future feature
  or another bugfix pass) — if it ever starts passing, that is the signal to
  drop `#[ignore]` and reconsider whether the spawn mitigation is still
  needed.
- **Filing upstream remains open, unstarted work**, not resolved by this
  ticket. The isolated repro in `character.rs`'s doc comment is
  ready to paste into a new `dimforge/rapier` issue whenever the coordinator
  wants to spend that effort; nothing found here changes that recommendation
  either way.
- **Re-check periodically.** `rapier3d` shipped three patch releases
  (`0.35.1`–`0.35.3`) in about three weeks around early-to-late August 2026,
  so another point release is plausible on a similar cadence; a repeat of
  this crates.io/CHANGELOG.md check costs a few minutes and needs no code
  changes unless something relevant actually appears.

## Checked-but-not-applicable

Per the ticket's step 3 ("if no newer release exists... do not change any
code or dependencies"), no edits were made to `Cargo.toml`, `Cargo.lock`, or
any source file. `crates/spall_physics/src/character.rs`'s
`row15_elevated_beam_near_origin_freezes_horizontal_movement` test keeps its
`#[ignore]` attribute unchanged.
