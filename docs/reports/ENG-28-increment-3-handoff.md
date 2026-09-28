# ENG-28 / T21 increment 3 — implementation handoff

**Ticket:** ENG-28 — "T21 Contact damage and dormant debris" (`in_progress`).
**Owns:** server damage rules and region sleep policy.
**Read first:** `AGENTS.md`, `docs/architecture.md` (§"Large collapses and load
policy", §"Collision and character physics", the two `T21 outcome` paragraphs),
`docs/protocol.md`, `docs/validation.md` (§"Correctness fixtures" →
`contact-damage` / `sleep-wake`, and the two CPU-side proof notes).

---

## Where things stand

| Increment | PR | Branch | What it delivered |
| --- | --- | --- | --- |
| 1 — contact → **terrain** damage | #73 | `feat/eng-28-t21-contact-damage` | `PhysicsWorld::contact_impulses`; pure `spall_sim::contact_damage::ContactDamagePolicy`; `Simulation::apply_contact_damage` (opt-in). Dynamic-vs-terrain contacts only. |
| 2 — region dormancy | #74 (stacked on #73) | `feat/eng-28-t21-inc2-dormancy` | `PhysicsWorld::deactivate_body` / `reactivate_body`; pure `spall_sim::dormancy::DormancyPolicy`; `Simulation::apply_dormancy` (opt-in); `Simulation::submit` wakes a dormant body an edit targets. |

Acceptance status after increment 2:

| T21 acceptance bullet | Covered by |
| --- | --- |
| repeated resting contacts do not continuously fracture floors | inc 1 |
| a falling body can damage terrain | inc 1 |
| sleeping rubble can be excavated and wake | inc 2 |
| fragment counts / pending jobs bounded, explicit admission | inc 1 + inc 2 per-tick caps |

All four bullets have CPU-side evidence. **Increment 3 is hardening + the two
explicitly-deferred behaviours; it is not required to reach "accept", but T21
should not be marked `done` until at least 3a lands and a coordinator reviews
the whole T21 surface.**

Base your branch on `feat/eng-28-t21-inc2-dormancy` (or on `main` once #73 and
#74 have landed). Suggested branch: `feat/eng-28-t21-inc3-body-fracture`.

---

## Scope of increment 3

Three pieces, roughly in priority order. 3a is the substantive one.

### 3a — body-on-body contact fracture

**Current behaviour.** `Simulation::apply_contact_damage`
(`crates/spall_sim/src/sim.rs:262`) classifies each `ContactImpulse`:

```rust
let striker_idx = match (contact.dynamic[0], contact.dynamic[1]) {
    (true, false) => 0,
    (false, true) => 1,
    _ => continue,   // <-- (true, true) = debris-on-debris is dropped here
};
```

A `(false, false)` pair cannot occur (only terrain is `Fixed`). So the `_` arm
is exactly the debris-on-debris case, and it is currently ignored.

**Goal.** A hard enough impact between two dynamic bodies carves a bounded cut
into (at least) one of them, using the same threshold / cooldown / per-tick-cap
machinery as terrain damage.

**Work.**

1. **`spall_sim::contact_damage` — target a body, not just terrain.**
   - `ContactEvent` (`contact_damage.rs:92`) hard-assumes terrain: field
     `target_volume: VolumeId` + doc "terrain volume taking the damage", and
     `PlannedDamage.target` is always `EditTarget::Terrain`
     (`contact_damage.rs:116`). Generalise: `ContactEvent` gains an
     `EditTarget` (or a `target_kind` enum) alongside `target_volume`, and the
     policy emits `PlannedDamage { target, .. }` faithfully.
   - **Brush frame.** For `EditTarget::Terrain` the brush centre is a *global*
     cell (`brush_at` in `contact_damage.rs` rounds `point_m / cell_m`). For
     `EditTarget::Body` the brush must be in the **body-local** cell frame:
     `body.pose.xform(body.cell_size()).world_to_local_cell(DVec3::from(point_m))`
     (`spall_voxel::RigidXform::world_to_local_cell`). The policy is pure and
     must not touch `SimWorld`, so the caller
     (`Simulation::apply_contact_damage`) has to hand the policy the contact
     point **already resolved to the target volume's cell frame**, or pass the
     `RigidXform` in the `ContactEvent`. Prefer the former — keep the policy
     working in cell coordinates only. Rename `brush_at` inputs accordingly.
   - **Cooldown key.** Today the per-region key is
     `(event.target_volume.get(), brick_of(point_m, cell_m))` where the brick is
     computed from the **world** point. For a moving body the same physical spot
     maps to a different world brick every tick, so the cooldown never bites.
     Key on `(target_volume, brick-in-that-volume's-local-frame)` instead.

2. **`Simulation::apply_contact_damage` — build the `(true, true)` event.**
   - Resolve both `contact.bodies[i]` to entities via `world.body_by_phys`
     (already used for the striker). Skip if either side is dormant (a dormant
     body has no physics body, so this can't actually happen — but assert it).
   - **Decide which body takes the damage.** This is a coordinator decision —
     see "Open decisions". Recommended default: the body with the **lower
     speed** at the contact (the one being struck); if the speeds are within
     `still_speed_m_s` of each other, damage the **lower-mass** body. Optionally
     damage both when both clear the threshold, but bound it so one collision
     cannot spawn N cuts.
   - `resting_impulse_n_s` for the event = `mass_struck * g * dt` (same formula,
     use the struck body's mass from `body_state(phys).mass_kg`).
   - `striker_born_this_tick` from `report.committed[..].children` — reuse the
     existing `born_this_tick` set; check the **struck** body's entity too (a
     body split out this tick is at its split instant, not a real impact).

3. **Submit the cut** through the existing loop (`sim.rs:293`): server-authored
   `RequestId(SERVER_REQUEST_ID_BAND | self.next_damage_seq)`, actor
   `CONTACT_DAMAGE_ACTOR_ID`, `EditTarget::Body(struck_entity)`, optional
   `ExplosionImpulse`. The body-targeted edit path already exists (client
   body-recuts, e.g. the `g1-networked-destruction` body-targeted cut).
   **Note:** if the struck body were dormant, `submit` would need to wake it —
   but `apply_contact_damage` runs on live contacts only, so this is moot;
   still, route the submit through `Simulation::submit` (not
   `self.pipeline.submit_intent` directly) so the dormancy-wake guard covers it
   uniformly. Terrain cuts can stay on the direct path or move too — pick one
   and be consistent.

4. **Cascade bound.** One collision must not fracture a body every tick into a
   cascade. The existing guards mostly cover it: the per-region cooldown (now
   body-local), the world-wide `max_intents_per_tick`, and
   `striker_born_this_tick`. Add a test that a stack of settling debris does not
   produce an unbounded cut stream (assert total damage cuts over N ticks is
   `<= some small multiple of the body count`).

**Tests (add to `crates/spall_sim/tests/contact_damage.rs`):**
- `a_heavy_body_dropped_on_a_lighter_one_fractures_the_lighter_one` — two stone
  cubes, one dropped onto the other on a slab; assert the struck (lower) body
  loses cells, the dropper does not (or loses fewer), conservation holds
  (`total_solid_cells` before == after + destroyed), body count stays bounded.
- `settling_debris_stack_does_not_cascade` — drop ~4 cubes in a loose stack;
  run to rest; assert the number of admitted body-damage cuts is bounded and
  they stop once everything sleeps (mirror of
  `a_settled_body_stops_damaging_the_floor`).
- Pure-module unit tests for the new `EditTarget::Body` path: brush lands in the
  body-local frame; body-local cooldown key bites for a moving body.

**Docs:** update the `T21 outcome — contact damage` paragraph in
`docs/architecture.md` ("Increment 1 damages terrain only" → "increments 1 and 3
damage terrain and detached bodies"); update the `contact-damage` fixture row in
`docs/validation.md` and the increment list in `docs/tasks.md` (§T21).

### 3b — an adjacent terrain edit wakes a dormant neighbour

`docs/architecture.md`: *"Sleeping bodies retain geometry, pose, identity, and
future destructibility; nearby edits wake affected neighbors."* Increment 2
wakes a dormant body only when an edit **targets that body**
(`Simulation::submit`, `sim.rs:192`). A **terrain** cut directly under a dormant
body should also wake it — the ground it rests on just changed.

**Work.**
- `Simulation::apply_dormancy` currently takes only `&mut DormancyPolicy`
  (`sim.rs:321`). Give it `report: &TickReport` (same shape as
  `apply_contact_damage`). #74 is not merged, so changing this signature is
  free — call it out in the PR.
- From `report.committed`, build a world-space box per terrain transaction with
  `crate::player::transaction_world_box(&committed.topology, terrain_volume_id,
  cell_m)` (already used by `advance_players`, `sim.rs`). Turn each box into an
  `ActiveRegion` (centre + half-diagonal radius) **and/or** set `hard_wake` on
  any dormant `BodyDormancyInput` whose `world_bounding_sphere` is within
  `wake_margin_m` of that box.
- Prefer `hard_wake` (immediate, bypasses `min_dormant_ticks`) — a support
  change under settled rubble should not wait out the hysteresis window.
- The `BodyDormancyInput.hard_wake` field already exists and is already honoured
  by `DormancyPolicy::plan`; increment 2 just always passes `false`.

**Tests (`crates/spall_sim/tests/dormancy.rs`):**
- `a_terrain_cut_under_dormant_rubble_wakes_it` — settle a cube to dormant on a
  thick slab, then cut the slab directly beneath it; assert the body
  reactivates the same tick (or the tick the cut commits) and then re-settles /
  falls as the geometry dictates.
- `a_terrain_cut_far_from_dormant_rubble_leaves_it_dormant` — negative control.

### 3c — server wiring (optional, coordinator's call)

`apply_contact_damage` and `apply_dormancy` are both **opt-in** and unused by
`spall_server::serve`, matching how the increments were delivered. Wiring them
into the serve loop is real integration work with a real hazard:

- The `body-rest-on-structure` G1 gate runs the server with
  `--await-body-settle` and asserts, from **physics**, `max_penetration_m()`,
  per-body sleep, and origin stability for `>= body_settle_min_stable_ticks`.
  A body that goes **dormant** is not in the physics world, so those reads would
  need to fall back to the frozen `Body` record, or dormancy must be disabled
  under `--await-body-settle`.
- Contact damage during a gate run would change the committed hash the gate
  replays against.

If you wire it: put both behind explicit `ServeConfig` flags (default **off**
for the existing gate scenarios), add a dedicated `sleep-wake` xtask scenario
(networked: a client settles rubble, disconnects/moves away, reconnects/returns,
and the rubble is shown to have deactivated then reactivated and still be
destructible), and record evidence in a `docs/reports/` note. This is arguably
its own follow-up ticket rather than part of increment 3.

---

## Invariants you must not break

1. **No mutation in a physics callback.** Contacts are read *after*
   `PhysicsWorld::step`; damage becomes an `EditIntent` that stages off-tick and
   commits on a later tick. Never edit a volume from inside contact handling.
2. **Dormancy is invisible to authoritative state.** Deactivating / reactivating
   a body must not change `world_hash`, `total_solid_cells`, or the checkpoint
   set. `dormancy_is_invisible_to_the_authoritative_state` guards this — keep it
   green. (Body-on-body *fracture* does change state, on purpose, via a normal
   committed transaction — that's fine; it's an edit like any other.)
3. **Conservation.** Every damage cut is a normal commit, so the existing
   `terrain_solid + child_solid + destroyed == before` ledger applies. Add
   conservation asserts to the new body-fracture tests.
4. **Bounded admission.** Fragment counts and pending jobs stay bounded: keep
   the per-region cooldown, `max_intents_per_tick`, and the dormancy per-tick
   caps. Excess is dropped and counted, never queued unboundedly.
5. **Determinism.** `ContactDamagePolicy::plan` and `DormancyPolicy::plan` are
   pure and order-independent. Any new input (struck-body selection, terrain-box
   regions) must be sorted / canonicalised before it can affect a cap decision.
6. **Server request-id band.** Server-authored damage cuts use
   `RequestId(SERVER_REQUEST_ID_BAND | seq)` (`1 << 62`). Do not collide with
   client request ids; do not renumber the band.
7. **Reserved player / actor bands** unchanged (`PLAYER_ENTITY_BASE = 1 << 48`,
   `CONTACT_DAMAGE_ACTOR_ID = 1 << 40`).

---

## Open decisions for the coordinator

1. **Which body absorbs a body-on-body hit?** Options: (a) lower-speed body;
   (b) lower-mass body; (c) both, capped. Recommendation: lower-speed, tie-break
   to lower-mass, single cut per collision per tick. Needs sign-off before
   implementation because it is a gameplay approximation, like the T22 strength
   equations.
2. **Should 3b use `hard_wake` or proximity-with-hysteresis?** Recommendation:
   `hard_wake` — a support change is not a "maybe".
3. **Is 3c in scope for this ticket or a new one?** Recommendation: split it
   out; it needs its own networked fixture and gate-interaction review.
4. **Threshold tuning.** `ContactDamageConfig` / `DormancyConfig` defaults were
   tuned on 1–4-cube fixtures. The G4 workload (256 active bodies, 4096 sleeping)
   will want a retune; that is `ENG-30` (T23) gate work, not increment 3, but
   note any value that looks fragile.

---

## Suggested order of work

1. Rebase onto the latest of #73/#74 (or `main` if merged). Confirm
   `cargo test -p spall_physics -p spall_sim` is green.
2. **3b first** — it is small, self-contained, and de-risks the
   `apply_dormancy` signature change while #74 is still open.
3. **3a** — generalise `ContactEvent` / `PlannedDamage` to carry an
   `EditTarget`; add the body-local brush frame + cooldown key; wire the
   `(true, true)` arm in `apply_contact_damage`; tests.
4. Docs: `architecture.md`, `validation.md`, `tasks.md`. Update the ENG-28
   ticket body (add an "Increment 3" section) and add a work-log entry with
   changed files, exact checks, measured results, remaining risks, next task.
5. **3c** only if the coordinator says it belongs here.

---

## Test checklist (before the PR)

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test -p spall_physics -p spall_sim -p spall_server --all-features`
- `cargo test --workspace --all-features` — the only expected failure is
  `spall_net::process_harness::separate_process_transport`, a pre-existing
  flaky two-process QUIC test unrelated to this work (passes on rerun; passes on
  clean `main`). Confirm nothing else regresses.
- New: body-on-body fracture (fracture + conservation + no cascade), terrain-cut
  wakes dormant neighbour (+ negative control), pure-module unit tests for the
  body-target path.

---

## Reference — key files & symbols

| Symbol | File | Role |
| --- | --- | --- |
| `PhysicsWorld::contact_impulses` | `crates/spall_physics/src/world.rs` | per-pair solved normal impulse (N·s) + world point + normal + `dynamic: [bool; 2]`, read after `step()` |
| `ContactImpulse` | `crates/spall_physics/src/world.rs` | the DTO above |
| `PhysicsWorld::{deactivate_body, reactivate_body, is_dormant, active_body_count}` | `crates/spall_physics/src/world.rs` | reversible dormancy at the adapter |
| `ContactDamagePolicy` / `ContactEvent` / `PlannedDamage` / `ContactDamageConfig` | `crates/spall_sim/src/contact_damage.rs` | pure terrain-damage filter — generalise to bodies here |
| `brush_at`, `brick_of`, `cell_index` | `crates/spall_sim/src/contact_damage.rs` | brush construction / cooldown keying — need body-local variants |
| `DormancyPolicy` / `BodyDormancyInput` (`hard_wake`) / `ActiveRegion` | `crates/spall_sim/src/dormancy.rs` | pure dormancy decision — `hard_wake` already honoured |
| `Simulation::apply_contact_damage` | `crates/spall_sim/src/sim.rs:234` | contact → event → submit; the `(true, true)` arm at `:262` is the body-on-body TODO |
| `Simulation::apply_dormancy` | `crates/spall_sim/src/sim.rs:321` | add `report: &TickReport`; feed terrain-edit boxes as regions / `hard_wake` |
| `Simulation::submit` | `crates/spall_sim/src/sim.rs:192` | already wakes a dormant body an edit targets |
| `SERVER_REQUEST_ID_BAND`, `CONTACT_DAMAGE_ACTOR_ID` | `crates/spall_sim/src/sim.rs:30,34` | server-authored damage ids |
| `Body::world_bounding_sphere` | `crates/spall_sim/src/body.rs` | world sphere from `collider_region` + pose |
| `BodyPose::xform` → `RigidXform::world_to_local_cell` | `crates/spall_sim/src/body.rs`, `crates/spall_voxel/src/transform.rs` | world metres → body-local cell coords |
| `transaction_world_box` | `crates/spall_sim/src/player.rs` | world box of a committed topology transaction (used for player invalidation; reuse for 3b) |
| `SimWorld::{deactivate_body, reactivate_body, dormant_body_count, body_is_dormant, body_by_phys}` | `crates/spall_sim/src/world.rs` | sim-side dormancy + phys→entity lookup |
| integration tests | `crates/spall_sim/tests/{contact_damage,dormancy}.rs` | extend these |
