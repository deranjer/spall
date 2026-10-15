# ENG-207 precipitation fall-angle correction

## Change

Precipitation now uses one world-space velocity for both particle movement and the visible ribbon axis. Ordinary rain and snow keep a clear downward component with modest wind drift; blizzards drift more strongly while still falling, and high-wind-only cues remain horizontal. Ordinary snow uses short, near-square flecks; blizzards retain visibly stretched flakes. Weather phase advances locally in seconds at frame cadence and remains monotonic, so 1 Hz authoritative snapshots do not freeze the precipitation between updates.

The existing deterministic four-quadrant population fix is preserved. Clear-weather baseline gust no longer creates horizontal precipitation cues.

## Validation

- `cargo fmt --all` — passed.
- `cargo test -p spall_client weather::tests --lib` — passed, 8 tests.
- Tests cover cardinal and diagonal wind, downward fall versus drift, blizzard slant while falling, wind-only cue orientation, clear-weather no-precipitation, and deterministic quadrant population.

## Remaining scope

Snow ground coating and footprints are not implemented here. They depend on the coordinator freezing the authoritative WX3 surface snapshot contract. GPU capture and integrated 1 km playtest remain with the coordinator.
