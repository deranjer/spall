# ENG-211: small interactive zoo

Accepted 2026-10-15 as a visual animation showroom requested by the user.
An aquarium, open-front aviary and paddock display all current species: trout,
crow and deer. The existing SPVX decoder, `AnimalVisual` bridge and production
`GameRenderer` preserve the original 0.0625 m cells, hierarchy, tint and clips.
The scene contains 638 animal cells (136 + 217 + 285). No runtime creature
actors or new simulation interfaces are introduced.

## Changed files

- `examples/sandbox/examples/animal_zoo.rs`: native winit viewer, scene, voxel
  signs/control overlay, free camera, exhibit shortcuts, pause and idle/action
  controls, bounded native smoke and offscreen evidence, scenario tests.
- `examples/sandbox/Cargo.toml`, `Cargo.lock`: example registration and existing
  workspace-pinned winit as a development dependency; no version change.
- `README.md`, `docs/animals.md`, `docs/tasks.md`, this report: launch instructions,
  scope and measured acceptance evidence.

## Exact checks and measured results

Run at the repository root:

```text
cargo test -p sandbox --features client --example animal-zoo
cargo clippy -p sandbox --features client --example animal-zoo -- -D warnings
rustfmt --edition 2024 --check examples/sandbox/examples/animal_zoo.rs
git diff --check
cargo run -p sandbox --features client --example animal-zoo -- --capture --out .local/animal-zoo
target/debug/examples/animal-zoo.exe --frames 90
```

All checks passed. Two scenario tests sample every frame over two seconds in
both idle and action modes: all exact source cells remain finite, use valid
materials and unit rotations, and their oriented corners stay inside the
exhibit envelope. Every specimen's action changes its pose; repeated idle
sampling is deterministic and the crow switches down to the perch.

Capture produced 12 PNGs and `summary.json`: overview and each close-up at
two action times and one idle time. Measured adapter: NVIDIA GeForce RTX 4080
SUPER, DX12. Overview and all three action close-ups, plus crow idle/perch,
were visually inspected for legibility, tint, framing and visibility.
Native smoke presented exactly 90 frames, reported `resized: true`, and exited
successfully. Its scripted sequence exercised each focus view, pause/resume,
idle/action and resize to 960 x 600 using the same state/render paths as the
interactive window. Native mouse capture and physical keyboard input have
not been independently automated. No performance threshold is claimed.

The existing incremental cache emitted a hard-link fallback warning; it copied
cache files and did not fail compilation. Offscreen outputs are local evidence,
not checked-in binary assets.

## Limits and next task

This is a free-camera showroom. Camera movement and display geometry do not
collide; aquarium water is a transparent display surface. Animals play authored
clips in place. Habitat navigation, terrain collision, corpse dynamics,
population, damage, authority, networking and persistence remain unimplemented
by this ticket. This display does not bypass the ENG-209 contact-response limits.
Next unblocked animal task: **ENG-210**, articulated movement and collision-response
rig feasibility, before authoritative population integration.
