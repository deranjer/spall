# ENG-204: articulated animal preview

Date: 2026-10-14. Scope is a reusable sandbox visual bridge and bounded GPU
pose captures; no live creature population or G5 gate is claimed.

## Changes and review

`spall_asset` validates and samples portable assemblies. Sandbox
`animal_visual` expands exact runs once into bounded part-local geometry and
composes parent-relative poses into oriented cell boxes, preserving source
material/tint slots, inherited visibility, pivot and cell scale. It rejects
unknown references/mappings, invalid transforms, non-unit scale and expansions
above the caller budget or the fixed 250,000-cell cap. These are render cells,
not ECS entities, world voxels or physics bodies.

The preview worker reached its usage limit with source files written and CPU
tests passing. The coordinator recovered those files from its isolated worktree,
finished review and validation, and improved the assets after actual captures.
Fish fins became thin, tapered geometry with a forked tail and contrasting eyes;
crow plumage became charcoal; the deer gained width, a shorter neck and four
independent leg joints with diagonal phases. UUIDs, cell-size code and existing
symbolic sandbox materials remain unchanged. No manifest versions changed.

The fixed preview camera/floor use the union of oriented cell bounds across
poses. Initial per-frame framing masked idle translation and was corrected.
Captures retain parent-relative 6.25 cm cells instead of flattening animals into
quarter-metre terrain. The preview uses the production `GameRenderer`, explicit
render-only tint variants and an offscreen target; it is not an alternate renderer.

## Reproduction and evidence

```powershell
cargo run -p sandbox --example animal_fixtures -- --write
cargo run -p sandbox --example animal_fixtures -- --check
cargo test -p spall_asset
cargo test -p sandbox --lib
cargo test -p sandbox --test animal_assets --test animal_habitat_world
cargo test -p spall_fluid --test column_lazy_ocean
cargo run -p sandbox --features client --example animal-preview -- --bounded --out .local/animal-preview
cargo xtask check
```

The capture command completed on **NVIDIA GeForce RTX 4080 SUPER / Dx12**:
24 frames at 384 x 384, four poses for each animal's idle and movement clip;
three 1536 x 768 contact sheets and a JSON inventory under
`.local/animal-preview/`. `summary.json` records the actual adapter/backend.
Final pose counts are trout 136 cells/5 parts, crow 217/6 and deer 285/9.
Visual review found recognizable block silhouettes and tail, wing and leg
motion; this is a small stylized source-content proof, not final animal art.

CPU checks include 13 decoder/sampler tests, 6 visual bridge tests (including
an independently calculated rotated parent/child/pivot result and descendant
visibility), independently decoded material/movement/loop checks, live generated
gallery candidate validation after draining/cutting, and eager/lazy ocean parity.
The final full check result is recorded at closeout below.

## Limits and next task

GPU capture has no frame-time/population target and measures no navigation or
network cost. Vulkan, a distinct adapter and a world-population GPU gate are
unrun for this task. The captured water/land backdrop is a presentation floor,
not the generated-world habitat test (which is a separate CPU scenario).

Animals do not yet spawn, steer, collide, receive damage, replicate or recover
in the shipped world. In particular, animated source parts do not implicitly
become physics bodies or structural joints. ENG-205/A4 is the next unblocked
contract audit for authoritative creature lifecycle, collision/damage/voxel
ownership, versioned asset/clip state, durable recovery and interest streaming.
No local-only population or procedural regeneration of saved actors is accepted.

## Closeout

ENG-204 is accepted for its source preview scope. Formatting and strict
workspace/all-target/all-feature Clippy passed. The unrestricted
`cargo xtask check` run passed all unit/integration tests, then its `spall_net`
doctest build saw a new snow message against the older compiled protocol from
before concurrent snow edits. The single full command therefore exited 1.
`cargo test --workspace --all-features --doc` passed after refreshing the
dependency build; no snow source was changed for this animal task. The final
`cargo test -p sandbox --lib --test animal_assets --test animal_habitat_world`
also passed (23 library tests, 2 authored-asset tests, 1 live-world test).
No uninterrupted green `xtask check` on an immutable final tree is claimed.
Logs: `.local/animal-check-unrestricted.log`, `.local/animal-doc-check.log`,
`.local/animal-scoped-check.log`.

The first restricted full run timed out waiting for the dam-gate server's
fingerprint; `cargo test -p sandbox --all-features --test dam_gate_replication`
passed outside the restricted sandbox, and passed again in the unrestricted
full run. Initial preview Clippy diagnostics were corrected before the final
workspace lint passed. Full-run results are regression evidence, not acceptance
of later concurrent snow changes or any populated-world/performance gate.

The coordinator retained the completed source changes after the preview worker's
quota error. Independent worktrees were archived with recoverable snapshots.
Next unblocked implementation prerequisite remains ENG-205/A4.
