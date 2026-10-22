# ENG-200 — bounded weather presentation

Implemented client-side rain, storm, snow, high-wind and blizzard presentation
on the coordinator's integrated weather base. The client policy consumes the
authoritative `WeatherState`, camera, frame delta and immutable voxel `Volume`;
the renderer receives only bounded visual particles and presentation values.

`spall_client::weather::WeatherPresentation::update` returns a borrowed
`WeatherFrame` with `particles` and `WeatherLighting`. The center sky-column
sample supplies smoothed exposure (unknown or roofed is sheltered), which can
also drive audio. The renderer's `GameRenderer::set_weather_particles` accepts
both outputs and returns a `WeatherRenderError` for an over-cap or non-finite
particle input. It expands ribbons against the current camera, draws them with
the existing scene depth buffer, then applies a small HDR atmosphere overlay
for blizzard visibility, storm dimming and optional reduced lightning.

Particle distribution is deterministic from the authoritative seed and index,
while positions advance on clamped local frame time so they do not freeze
between coarse server snapshots. Teleports and world-epoch changes reset the
local phase. Counts are capped at 1,536, camera-local positions wrap within a
40 m horizontal area, and shelter queries inspect a fixed 5x5 grid with at
most 8 m of vertical samples per column (800 samples per update at quarter-
metre terrain resolution; up to 3,200 at sixteenth-metre detail resolution).
The grid is cached until the camera crosses a voxel cell or the immutable
volume changes. Unknown cells suppress particles in their sample column. Existing
scene depth prevents outdoor ribbons from drawing through roof and wall
geometry. F10/audio wiring is owned by the coordinator; no global controls or
authoritative weather state were changed here.

Checks run in the assigned worktree with an isolated Cargo target directory:

- `cargo test -p spall_client weather::tests --lib` — 5 passed.
- `cargo test -p spall_render --lib` — 69 passed.
- `cargo test -p spall_render weather::tests --lib` — 1 passed, validating both WGSL entry points with Naga.
- `cargo fmt --all --check` — passed.
- `git diff --check` — passed.

No GPU capture, 1 km playtest, or full `cargo xtask check` was run in this
worker slice; the coordinator owns the integrated window/audio build and live
playtest. That GPU review should confirm shader validation, storm/blizzard
visibility, sheltered views, and frame cost on the target machine. The vertical
exposure ray is limited to 8 m and direct-to-sky; sideways shelter and weather
visible through openings are approximated by the sample grid plus depth test.
