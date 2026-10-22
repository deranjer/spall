# ENG-207 precipitation and snow presentation

## Change

Precipitation now uses one world-space velocity for both particle movement and the visible ribbon axis. Ordinary rain and snow keep a clear downward component with modest wind drift; blizzards drift more strongly while still falling, and high-wind-only cues remain horizontal. Ordinary snow uses short, near-square flecks; blizzards retain visibly stretched flakes. Weather phase advances locally in seconds at frame cadence and remains monotonic, so 1 Hz authoritative snapshots do not freeze the precipitation between updates.

The existing deterministic four-quadrant population fix is preserved. Clear-weather baseline gust no longer creates horizontal precipitation cues.

## Validation

- `cargo fmt --all` — passed.
- `cargo test -p spall_client weather::tests --lib` — passed, 8 tests.
- Tests cover cardinal and diagonal wind, downward fall versus drift, blizzard slant while falling, wind-only cue orientation, clear-weather no-precipitation, and deterministic quadrant population.

## Remaining scope

The coordinator owns window toggles, live GPU capture, and the integrated 1 km playtest. The renderer and client presentation API are implemented in this worktree.


## WX3 presentation

The client converts authoritative per-tile snow frames into merged 4 m tile rectangles, rechecking each masked face against the current terrain brick revision, a filled support cell, and a known-empty cell above. Unknown, roofed, stale, and zero-depth faces are suppressed. Conversion is cached by frame snapshot sequence, volume state stamp, and an 8 m camera bucket, with a 72 m camera-local radius and the renderer's 8,192-patch / 1,024-footprint caps. Diagnostics report confirmed/rendered coverage, maximum depth, stale/hidden faces, and cap-limited cohorts.

The renderer draws snow after opaque terrain with scene depth testing and writes depth for vegetation/transparent occlusion. A softly bounded ambient/sun estimate shades its cool vertex colors against the active environment. Footprints are boot-shaped and clipped to their authoritative quarter-metre contact cell. Weather effects toggling does not alter these ground-snow patches; the host owns a separate ground-snow presentation toggle.

Additional checks after the snow implementation:

- `cargo test -p spall_client snow --lib` — passed, 4 tests covering flat/hole merging, current revision/exposure, roof and unknown suppression, stale brick revision, zero-depth visibility, contact-cell footprints, and cached diagnostics.
- `cargo test -p spall_render snow --lib` — passed, 4 tests covering bounded patch/depth input, contact-cell clipping, environment-aware shading, and WGSL validation.
- `cargo fmt --all` — passed.
- GPU capture and 1 km playtest remain with the coordinator.

## Integrated visual evidence

The coordinator's release capture reviewed the Rain, Snow, and Blizzard profiles at `.local/runs/eng207-angle/shots/shot-00.png` through `shot-02.png`. Rain and ordinary snow now read as falling with only a modest wind slant; ordinary snow uses compact flakes, while blizzard particles remain stretched. Each capture completed and the client exited normally; the only reported warning was the expected connection close during shutdown. A GPU capture of the newly added ground coating pass is still pending the coordinator's live integration run.
