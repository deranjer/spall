# Animal foundations: dated history (to 2026-10-19)

Moved out of `docs/animals.md` on 2026-10-20 when animals became plain server
entities (see [animal lifecycle](../animal-lifecycle.md)). The exact-ownership,
articulated-collision and corpse-aggregate work described below was removed in
that change and remains available in git history.

## Implemented evidence (2026-10-14)

ENG-201/202/203 are reviewed and accepted for their bounded foundation scopes.
The independent decoder reads all three authored animals; sampled joint motion
and loop endpoints pass CPU tests. The generated gallery seed-1/256-cell test
finds aquatic, flying and land candidates using live terrain/column water, then
rejects the fish after draining a column and the deer after removing support.
Dormant-ocean reads match eager water without waking or allocating regions.

ENG-204 adds sandbox `animal_visual`, preserving per-part geometry, source
materials/tints, joint transforms, parent visibility, pivot and 6.25 cm scale.
Unit scale is required; expansion is capped before allocation. Its bounded
production-renderer example writes 24 poses and three contact sheets:

```powershell
cargo run -p sandbox --features client --example animal-preview -- --bounded --out .local/animal-preview
```

Visual review on RTX 4080 SUPER / D3D12 prompted thinner fish fins and eyes,
charcoal crow plumage and a wider deer with four independent leg hinges.
Camera and floor are fixed per animal so root motion is not cancelled by the
preview. This is source animation/rendering evidence, not running creatures
in a world. Exact commands, results and limits: `reports/ENG-204-animal-preview.md`.

ENG-205/A4 is accepted for its lifecycle audit and prerequisite map, with the
reviewed [lifecycle proposal](animal-lifecycle.md) and
[audit evidence](reports/ENG-205-contract-audit.md). Runtime interfaces remain
proposed until the physical representation gates are reviewed.
Current body-spawn and committed-edit APIs do not provide an animated actor
lifecycle.
`InitialGameWorldSetup` runs only at fresh-world creation, and the existing
`SimWorld::spawn_body` path bypasses committed topology creation (the server
uses it before client baselines). It cannot safely stand in for animal births,
deaths or spawning during play.
Creature collision/hits, editable ownership after damage/death,
replicated asset/clip state, durable recovery and streaming/reset identity all
need an explicit shared contract. The preview must not become a local-only
population shortcut.

The user authorized a Luna worker for ENG-205 after the preview closeout.
One Luna 6 Medium worker audits the cross-cutting lifecycle contract in a
dedicated `animal-lifecycle` worktree, starting from `71540f8`. It owns a new
`docs/animal-lifecycle.md` proposal and ticket-specific evidence; the coordinator
retains shared-doc integration and Loopira status/log ownership. The audit
must map existing APIs and freeze identity, source-part/cell ownership,
tick-boundary ordering, collision/hits, births/deaths, durable state and
baseline/interest/reset semantics before dependent runtime implementation.
Implementation slices may fan out only after that shared contract is reviewed.

## Lifecycle review and collision prerequisite (2026-10-15)

ENG-205 review requires one assembly owner with distinct part-local source
volumes, preserving overlapping source coordinates and stable part/cell
provenance. Moving part transforms must not rewrite source cells. Death needs
an atomic ownership transfer and a separately reviewed editable multi-volume
corpse representation; the existing one-volume dynamic body path is insufficient.

ENG-209 was implemented in an isolated `animal-collision` worktree at the
accepted animal baseline `7208ffc`. A Luna 6 Medium worker measures a candidate:
one Rapier kinematic root carrying exact per-part voxel or merged-cuboid
colliders. The bounded CPU gate covers current-pose queries, source-cell hit
identity, stale update isolation, geometry edits, real contacts, and the actual
trout/crow/deer geometries. It introduces no runtime creature lifecycle API.
No representation or population capacity is accepted at dispatch. Dynamic
corpse ownership, locomotion, multiplayer and recovery remain separate work.

ENG-205 review is integrated as `bf56e76` (following `33d73df`). It fixes
part-local ownership, exact frozen corpse poses, admission limits including
retained corpses, 60 Hz simulation/animation versus 20 Hz motion snapshots,
ping-pong phase recovery, and monotonic identity/reset rules. This closes the
audit, not live creature support. Focused regression command
`cargo test -p spall_asset -p sandbox --lib --test animal_assets --test animal_habitat_world`
passed: 13 asset tests, 23 sandbox library tests and 3 integration tests.
ENG-209 closeout follows below; editable physical corpse assembly remains a
separate gate before runtime lifecycle work.

## Exact animated collision prototype accepted (2026-10-15)

ENG-209 is integrated as `c46b7c6` and accepted for its bounded experimental
geometry/query scope. The existing Rapier world holds one kinematic root with
exact per-part colliders and separate source-volume identity. Tokened pose and
topology updates reject stale/invalid work before mutation. A post-step collider
pose-cache bug found by the full animal workload is fixed and covered by an
independent regression. No production creature lifecycle interface is accepted.

The final release matrix passes all six trout/crow/deer and collider combinations:
38,520 source-cell oracle checks and 66 guaranteed misses, with zero mismatches.
The integrated focused suite passes 118 tests, with one manual timing test
ignored. Strict physics and probe Clippy, formatting and whitespace checks pass.
Exact commands, contact diagnostics and limits:
[adapter report](reports/ENG-209-articulated-collision.md),
[workload report](reports/ENG-209-animal-workload.md), and its committed JSON.

Geometry/query success does not resolve movement physics. Local limb animation
produces much weaker dynamic-probe impulses than the same root-driven path.
Prescribed kinematic root motion crosses fixed terrain under the current
collision defaults; the separate character query sweep blocks that wall.
Contact manifolds do not supply validated source-cell damage provenance.

ENG-210 subsequently implements the bounded per-source-part kinematic rig,
continuous occupied-cell transition queries, same-world solver substeps and
qualified source-contact witnesses. The coordinator integration checks and
limits are recorded in [ENG-210 integration](reports/ENG-210-integration.md).
The original one-step counterexamples remain retained as diagnostic evidence;
the new substep candidate does not make the default one-step solver continuous.

ENG-212's bounded editable dynamic aggregate and ENG-213's full-world locality
are integrated; [combined evidence](reports/ENG-212-213-integration.md) records
checks and actual 1024 m measurements. Next: ENG-214 explicit corpse connectivity
and source-cell split ownership.
Live spawning, navigation, multiplayer/recovery, far-origin precision and
population capacity remain unproven. The requested 1 km weather/animal launch
is held for fully simulated animals, rather than the visual zoo.
