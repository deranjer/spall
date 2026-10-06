# ENG-127 camera correction

User reported the sky following mouse movement. The fullscreen triangle passes
clip-space NDC into the sky fragment shader (+Y up), but the sky ray used
negative NDC Y. This mirrored sky features vertically relative to geometry and
made pitch changes move them incorrectly. Use positive NDC Y to match the
geometry camera projection in both the shared game and editor sky pipeline.
Also remove the redundant identical clear-color branches in the sky render pass,
which failed strict clippy; the clear color is unchanged.

Changed files in this correction: `crates/spall_render/src/shaders/skybox.wgsl`,
`crates/spall_render/src/game_renderer.rs`,
`crates/spall_render/tests/game_renderer_gpu.rs`, `docs/tasks.md`,
`docs/validation.md`, and this report. Existing skybox/day-night/portable work
in the working tree was preserved.

## Measured regression evidence

The new GPU test enlarges the sun disc to 4 degrees for the 320 x 240 capture.
At yaw/pitch offsets (0.16, 0.2) and (-0.16, -0.2) radians, it independently
projects the fixed celestial direction with `Camera::project` and requires
warm sun pixels at the resulting framebuffer position. Moving the camera by
(25, -8, 40) metres must leave all sky pixels byte-identical.

Before the shader fix, the test failed at the first projected sun pixel
(173, 147), which instead contained sky RGB [120, 188, 223]. After the fix,
both directions and exact translation invariance passed. The existing sun/cloud
variation test also passed. Captures: `.local/runs/eng-94-parity/skybox-daylight.png`,
`skybox-rotation-up.png`, and `skybox-rotation-down.png` in that directory.

## Checks

- `cargo test -p spall_render --test game_renderer_gpu skybox_sun_tracks_world_direction_under_camera_rotation -- --ignored --nocapture`: failed before fix as above.
- `cargo fmt -p spall_render`: completed.
- `cargo test -p spall_render --test game_renderer_gpu skybox -- --ignored --nocapture`: 2 passed after fix.
- `cargo test -p spall_render`: passed; other GPU tests remain ignored by default.
- `cargo check -p spall_render -p spall_client -p sandbox --features client`: passed.
- `cargo fmt -p spall_render -- --check`: passed.
- `cargo clippy -p spall_render --all-targets -- -D warnings`: initially found identical clear branches; passed after simplifying them.
- `& ./tools/package-portable.ps1`: release builds and portable packaging passed.

These are correctness checks, not a frame-time target or full-workspace gate.
Manual interactive mouse-look review remains unrun. Portable executables in
`dist/spall-portable` were refreshed. Next scoped action: ENG-127 interactive
acceptance review of this correction; no additional implementation ticket started.
