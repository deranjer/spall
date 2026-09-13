# ENG-28 / T21 increment 4 (3c) — server wiring + sleep-wake evidence

**Ticket:** ENG-28 — "T21 Contact damage and dormant debris".
**Scope:** `docs/reports/ENG-28-increment-3-handoff.md` §3c — wire the two
opt-in passes (`Simulation::apply_contact_damage`, `Simulation::apply_dormancy`)
into `spall_server::serve`, behind explicit off-by-default `ServeConfig` flags,
plus a dedicated networked `sleep-wake` xtask scenario and this evidence note.
The handoff flagged 3c as "arguably its own follow-up ticket rather than part
of increment 3" — the coordinator decision recorded here is that it stays in
ENG-28.

## What changed

- `ServeConfig.contact_damage: Option<spall_sim::ContactDamageConfig>` and
  `.dormancy: Option<spall_sim::DormancyConfig>`, both `None` by default —
  every scene and every existing gate fixture is byte-identical to before this
  increment (neither pass is ever called). `ServeConfig::headless()` and every
  test-only `ServeConfig` literal set both to `None` explicitly.
- `spall_server::serve`'s tick loop: right after `sim.tick()` and before the
  `--await-body-settle` bookkeeping, `apply_contact_damage` / `apply_dormancy`
  run once each (only when configured), folding submitted/rejected cut counts
  and deactivation/reactivation counts into new `ServeSummary` fields
  (`contact_damage_cuts_submitted`, `contact_damage_cuts_rejected`,
  `dormancy_deactivations_total`, `dormancy_reactivations_total`;
  `ServeSummary::version` 4 -> 5).
- `sandbox-server --contact-damage` / `--dormancy`: each turns the pass on
  with the module's `DEFAULT` tuning (no new numeric CLI knobs — the existing
  `ContactDamageConfig::DEFAULT` / `DormancyConfig::DEFAULT` are what every
  CPU-side T21 test already validates).
- **Gate-interaction hazard, addressed by staying off.** The handoff flagged
  that a deactivated body leaves the live physics world, which
  `--await-body-settle`'s settle check (`max_penetration_m`, per-body sleep)
  reads directly, and that contact damage changes the committed hash a gate
  replays against. Neither pass is enabled on any existing scenario fixture —
  `dormancy` / `contact_damage` and `require_body_settled` are never set
  together in this repo. No gate scenario's behaviour or hash changed.
- New `Scene::SleepWake`: `spall_voxel::fixtures::sleep_wake_arena` is
  `walk_arena` (the T19 30 m walk lane) plus a small column-and-beam in the
  player lane (`x` 60..=61 / beam `x` 54..=70, both `z` 6..=7, `y` 4..=11) —
  15 m from the `x = 1 m` spawn row, well outside the default 4 m dormancy
  wake margin. `spall_sim::fixtures::sleep_wake_setup` reuses `walk_arena_setup`'s
  collider region and `WALK_ARENA_SPAWNS` unchanged — the added geometry sits
  entirely inside the existing bounds, so `Scene::Walk` is untouched.
- `fixtures/scenarios/sleep-wake.json` + `cargo xtask scenario --name
  sleep-wake`: one client, server runs `--dormancy`.
  1. Tick 10: the client cuts the column (`dev_unvalidated_actions`, so the
     cut lands regardless of the client's own position) — the beam detaches,
     falls ~1.5 m, and settles.
  2. With nobody nearby, the dormancy pass deactivates it once it has been
     asleep and still for `settle_ticks` (120 ticks, ~2 s).
  3. Ticks 400-550: the client walks `+X` down the lane (`WALK_SPEED_M_S =
     4.5`) from `x = 1 m` toward the beam, crossing into the 4 m wake margin —
     the dormancy pass reactivates it by proximity.
  4. Tick 650: a `target: "body"` cut retargets at the (now-reactivated)
     detached body and lands, proving it is still fully destructible.
  - `dormancy_assertions: { min_deactivations: 1, min_reactivations: 1 }` in
    the fixture requires the server's own report to show both transitions
    happened — not just that `--dormancy` was passed.
  - `xtask`: `Scenario.dormancy: bool` (passes `--dormancy`),
    `Scenario.dormancy_assertions`, `dormancy_requirements_met` (folded into
    the session pass/fail alongside `residency_requirements_met`).
  - The existing `body_cut_ok` check (any body-targeted cut in the script must
    land on some client) already covers "still destructible" — no new
    machinery needed there.
- Docs: `docs/architecture.md` (new "T21 outcome — server wiring, increment
  4 / 3c" paragraph), `docs/validation.md` (`sleep-wake` networked-proof
  paragraph after the CPU-side dormancy proof), `docs/tasks.md` (T21
  increment 4 entry).

## Measured

`cargo xtask scenario --name sleep-wake --timeout-ms 90000` (real OS processes
over QUIC):

| run | result | committed | body_cut | body_disp_m | deactivations | reactivations | hash |
| --- | --- | --- | --- | --- | --- | --- | --- |
| clean | passed | 2/2 | true | 1.95 | 1 | 1 | `50302b2e...` |
| `--loss-percent 2` | passed | 2/2 | true | 1.88 | 1 | 1 | `50302b2e...` (same) |
| clean + `replay_check` | passed | 2/2 | true | 1.88 | 1 | 1 | `50302b2e...`, replay 2/2 events match |

The agreed hash is identical clean, under 2% loss, and on exact replay from
the tick-0 baseline — dormancy never touches `world_hash` (only the two
topology transactions — the column cut and the body cut — are journalled and
replayed; deactivate/reactivate is not a topology event). `max_contact_penetration_m`
stayed at `~1.5e-4` m (no clipping), consistent with existing T21 CPU-side
evidence.

## Checks

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test -p spall_voxel -p spall_sim -p spall_server -p xtask --all-features`
  — new `xtask` unit tests `dormancy_assertions_require_both_a_deactivation_and_a_reactivation`
  / `dormancy_assertions_are_met_trivially_when_unconfigured`; every existing
  `ServeConfig` test literal updated for the two new fields.
- `cargo xtask scenario --name sleep-wake` — clean, `--loss-percent 2`, and
  with `replay_check` (see Measured above).

## T21 status after this increment

Every T21 acceptance bullet and every increment-3 follow-up now has evidence:

| Bullet | CPU-side | Networked |
| --- | --- | --- |
| repeated resting contacts do not fracture floors | inc 1 | — |
| a falling body can damage terrain | inc 1 | — |
| sleeping rubble can be excavated and wakes | inc 2/3 | inc 4 (this note) |
| fragment counts / pending jobs bounded | inc 1 + 2 | — |
| body-on-body fracture | inc 3 | — |
| adjacent-edit wakes a dormant neighbour | inc 3 | — |
| opt-in passes reachable from a real server | — | inc 4 (this note) |

`apply_contact_damage` / `apply_dormancy` remain **off by default** for every
scene — this increment makes them reachable through `ServeConfig` /
`sandbox-server` and proves the wiring end to end on a dedicated fixture; it
does not turn either pass on for any existing gate or game scene. Recommend
closing ENG-28.
