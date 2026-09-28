# ENG-66 — row 15 spawn-overlap correction (T23 / G3)

## Correction (2026-09-23): not a Rapier defect

The original isolated run used an invalid spawn at `[1, 1, 1]`. The floor top
is `y = 1.0 m`; the raised beam's underside is `y = 2.5 m`; the default
standing capsule is `1.8 m` tall, so it intersects the beam by `0.3 m`. The
first sweep reports zero-time contacts from this initial overlap, and horizontal
movement remains blocked. This is expected for an overlapping spawn and does
not establish a Rapier character-controller defect.

Keeping `x = 1 m` and moving only `z` to `1.6 m` clears the beam footprint.
The new non-ignored physics regression moves more than 1 m over 200 ticks; the
separated-region integration test confirms all four corrected spawn slots walk
and remain grounded. Both separated-region spawn tables now use the clear lane.
No Rapier version change is needed; `0.35.3` remains the latest release checked
against the official [Rapier changelog](https://github.com/dimforge/rapier/blob/master/CHANGELOG.md).
The remaining content below records the earlier investigation, whose
conclusion that this was an upstream defect is superseded by this correction.

## Original report (2026-09-17; conclusion superseded)

The original ENG-66 pass checked whether a newer `rapier3d` release had fixed
the freeze reported in PR #99 / G3 increment 20. It concluded the bug was
upstream and that moving from `x = 1 m` to `x = 7 m` mitigated it. That
conclusion was based on a capsule spawned intersecting the beam, as described
in the correction above.

**Version-check outcome at that time:** `rapier3d 0.35.3` was the latest
released version on crates.io (2026-09-17). No dependency change was made.

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

## Historical regression evidence (before the correction below)

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

This output was the evidence behind the now-superseded upstream-defect theory.

## Current regression evidence

`cargo test -p spall_physics --lib row15` passes with a valid near-origin
spawn at `[1, 1, 1.6]`, clear of the beam footprint. The separated-region
integration test verifies that every normal and full-envelope spawn slot walks
more than 1 m over 30 ticks and remains grounded. The old `[1, 1, 1]` state
intersects the beam by 0.3 m vertically; the reported freeze is therefore
expected for an invalid overlapping spawn.

`cargo xtask scenario --name t23-g3-full-envelope --timeout-ms 300000` also
completed its traversal (slot 0 measured 118.89 m); each client reported the
same final hash, and replay, cold-restart, and reconnect hash checks passed.
The overall scenario remains **failed**: `body_settled` was false because the
detached-body sleep predicate was false (measured final speed was `7.12e-6`
m/s after 2,045 stable ticks). No gate or threshold was changed.

## Historical recommendation, superseded by the correction above

- No dependency change is required for this issue. The former `x = 7 m`
  workaround rationale and ignored-failure-test recommendation are superseded
  by the overlap diagnosis above.

## Checked-but-not-applicable

This section records the dependency-only pass from 2026-09-17; the later
correction above changed source, spawn fixtures, and tests without changing
`Cargo.toml` or `Cargo.lock`.
