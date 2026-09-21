# Optional expendable-debris lifetime

The user authorized deletion of explicitly expendable debris on 2026-09-21.
This is a game rule for intentional material destruction. It is separate from
T23 dormancy tuning, external-body lifecycle and compaction. ENG-30 remains open
and T23 unaccepted. Existing acceptance fixtures and their persistent populations
keep this feature **off**. No performance improvement is claimed.

## Eligibility and ownership

`spall_sim::DebrisLifetimePolicy` starts with no approved bodies. The game calls
`approve(EntityId)` only for a known expendable fragment in the current world.
Small size, material, birth by a split, or a high body count grants no permission.
`protect(EntityId)` revokes permission. Newly split children never inherit it.
The engine has no inventory/container/ownership taxonomy yet: the caller must
exclude structures, valuable resources, containers and player-owned objects.

Configuration sets occupied volume in cubic metres, continuous dormant ticks,
player/body clearances and per-tick work/removal budgets. Occupied volume uses
actual solid cells and their cell size, not an origin, AABB or brick count.
Hard limits bound this first implementation to 256 approved ids, 256 solid cells
and eight resident bricks per fragment, and at most 32 candidate checks per tick.
Unknown/evicted geometry and larger representations are deferred, never partly deleted.

Only dormant, sleeping bodies with zero recorded velocities qualify. Waking,
geometry revision changes, a pending body edit, nearby players, or a skipped
policy observation resets the timer. A runtime dormancy generation also detects
wake/re-dormancy between observations. Simulation time counts; offline time does not.

At expiry, conservative bounding spheres must clear every other dynamic body,
including sleeping and dormant bodies. This intentionally protects piles and
supporting fragments, even when a precise contact check might allow removal.
Terrain contact alone is permitted. Nearby terrain edits wake approved dormant
fragments before expiry; ordinary dormancy support-removal behavior still applies.
Current player capsules have a separate clearance check.

## Publication and recovery

Hosts call `Simulation::apply_debris_lifetime` once after `tick` and
`apply_dormancy`, before publishing the mutable `TickReport` or its journal.
The pass prepares an isolated empty-volume candidate, validates revisions/hashes
and protocol limits, reserves ids on a cloned registry, then retires the body and
appends its transaction. Failed preparation leaves matter and journal unchanged.
No body is forced asleep to make it eligible.

Deletion uses existing `CellRun` operations writing air, the existing empty-body
retirement behavior, and the normal ordered transaction/journal stream. Clients,
catch-up, baselines and save recovery use the same format as ordinary destruction.
No wire or save schema change is required. Surviving cells plus the explicitly
destroyed cells balance the previous world; this is not conservation by transfer.
`TickReport.debris_retired` identifies retired entities and removed cells/volume.
Server summaries count `debris_retired_total` and `debris_destroyed_cells_total`.

Approvals and pending timers are intentionally **session-local** in this first
version. A newly created policy protects all bodies; a game must reauthorize its
known expendable ids after loading. The sandbox's explicit id list is reapplied
at startup with a fresh full grace period. Already committed destruction persists
through journal recovery and checkpoints. Restart never accelerates expiration,
but repeated restarts can delay it. Durable gameplay tags/deadlines are a later
world-schema decision, not an implicit field in a physics record.

## Sandbox opt-in

Alongside the normal `sandbox-server --serve` arguments:

```text
--dormancy --debris-dormant-seconds 300 --debris-max-volume-m3 0.125
--expendable-debris-entity <known-expendable-id>
```

The entity flag is repeatable. The example interval is a game choice, not a tuned
performance recommendation. The sandbox uses a 16 m player clearance, 0.5 m body
clearance, four mature-candidate checks and one removal per tick. The engine API
exposes these settings. No automatic classification is provided. Do not copy an
id list between unrelated worlds or use it to remove a fixture's persistent bodies.

## Validation

The dedicated simulation regressions cover explicit approval/protection, physical
size boundaries, continuous dormancy including between-observation wake cycles,
player and supporting-neighbour protection, edit precedence, work/removal budgets,
missing observations, invalid configuration and atomic failure on id exhaustion.
The persistence integration test checks live replica application, duplicate
delivery, journal-suffix recovery, a fresh baseline and a subsequent checkpoint.
See the implementation work log for exact executed checks and outstanding limits.

This policy cannot fix the current soak by itself: frequently reawakened rubble
never completes its dormant interval. No new soak, GPU measurement or native visual
acceptance is implied by these regression tests.
