# ENG-198 — Combined nature audio and weather playtest

User request: integrate nature sounds and weather work, control it from F10,
then launch a 1 km generated world for a combined test. Parent scope includes
ENG-199 authoritative overrides and ENG-200 visual effects. No publication requested.

## Integration and controls

Existing ENG-194 biome ambience is present in the test branch. ENG-196 weather
foundation and ENG-197 winter dormancy were integrated from their reviewed worker
commits; final combined acceptance is recorded below after verification.

F10 provides server-authorized Automatic/Clear/Rain/Storm/Snow/High wind/Blizzard
selection. Local presentation controls toggle effects and reduced lightning.
Nature and weather audio have separate mute/volume controls. Next biome sound
previews the approved catalogue; Follow biome restores position-driven selection.
Snow/Scree remain silent where no natural recording was accepted.

Rain uses approved light/heavy rain, storms add a bounded one-shot thunder voice,
and high wind/blizzard reuse the approved pine wind recording. The latter is a
forest recording, not newly sourced snowfield ambience. No rejected recordings
or SnowFall music were added. Audio decoding/device work remains off the render
thread. Thunder IDs are deduplicated and expired events are suppressed; without
a replicated strike position, the first playback uses a short fixed atmospheric
delay rather than claiming distance-based propagation.

Winter pauses active plant and cleared-cover growth. The launch starts in winter;
forcing a visual weather profile does not change the ecology season. Seasonal
ground snow coating and footprints are WX3/WX4 and are not implemented in this
playtest. Existing solid worldgen snow is unchanged.

## Verification and live run

Do not infer a passed hardware/performance gate from compilation or a screenshot.
The final launch uses Natural seed 1,
4096 quarter-metre cells (1024 m edge), release binaries and enabled local admin
commands through `xtask play`. Record actual launch paths and PIDs after start.

Intermediate verification:

- `cargo check -p sandbox --features client` passed.
- `cargo test -p spall_ecology --lib`: 36 passed.
- `cargo test -p spall_client weather::tests --lib`: 6 passed after the capture
  review correction below.
- `cargo test -p spall_client input_tests::weather_capture_script_uses_typed_admin_choices --lib`: passed.
- Optimized server/client/xtask build passed. The launch harness also built the
  server without the client feature and the client with it.
- Workspace checks first exposed two test-only lint issues (Option comparison
  and boolean comparison); both were corrected. Strict lint/compile then passed.
  The test run reached `progression_network` and stalled at its final restarted
  server join. Owned test process 41020 was stopped after diagnostic collection;
  this run is not accepted as a full workspace pass. A bounded reproduction and
  final disposition are recorded below when complete.
- Sandbox audio mapping, all 19 approved decoders, short-loop boundary and
  weather asset selection tests passed in that workspace run.

The first bounded actual GPU run used Natural seed 1, 4096 cells and winter.
The client installed the 262,144-brick world catalogue and all seven scripted
weather commands (six profiles plus automatic) were accepted. Nine real captures
are in `.local/runs/eng198-capture/shots`; launcher/server/client diagnostics are
alongside them. F10 was visually reviewed and shader presentation succeeded.
Audio output streams opened without an unavailable-device warning; subjective
listening remains the user's playtest.

Capture review found particles correlated along a diagonal because low-bit XOR
did not decorrelate the random coordinate samples. Independent hashed x/z samples
correct this, and a quadrant-population invariant now passes. Final corrected
visual captures and the persistent playable handoff are pending below. The `play`
capture-tour step ends automation without closing the window, leaving the final
world under player control after screenshots.

The captured window was maximized at 2560 x 1369 despite the requested initial
size. No isolated GPU incremental budget, bandwidth budget, soak, two-client
impaired-link performance gate or complete shelter/destruction visual matrix is
claimed by these captures.

## Corrected live handoff

Final world: Natural, seed 1, 4096 cells (1024 m), initial Winter, optimized
binaries. Run directory: `.local/runs/eng198-live`. Launcher PID 6936, server
PID 44280, interactive client PID 41344. The requested session is intentionally
left running; closing its window lets xtask stop its server. The run is bounded
to 216,000 server ticks (about one hour). Earlier owned capture/launcher processes
were cleaned up, not unrelated processes.

The final script captured rain, storm, snow, wind, blizzard, clear and F10, then
`play` ended automation without closing the game. F10 is open with the pointer
released. Server acknowledgements confirm the six forced profiles. The corrected
GPU images were reviewed: rain/snow are spatially distributed, blizzard changes
visibility, and the controls fit with vertical scrolling. The menu and `play`
script paths now release mouse capture just like the actual F10 key.

Structured capture records report exposed-camera particle counts: Rain 766,
Storm 1536, Snow 463, HighWind 116, Blizzard 1221, Clear 11 then 19 during blend.
All are below the 1536-particle cap. The final images are
`.local/runs/eng198-live/shots/shot-00.png` through `shot-06.png`.
The listener selected the approved `dry-grass-wind.ogg` biome bed at the spawn.
No missing-file, audio-output, shader-validation or rejected-weather errors were
found in the final launch logs at handoff. Audio audition remains manual.

## Restart correction and final checks

The restart hang was an integration regression: the new persistence worker
started its acknowledged cursor at zero although recovery had already proven
an earlier weather journal prefix durable. Static recovered worlds could wait
forever at clean shutdown without producing any new records. The worker now
starts from that recovered durable prefix; new worlds still start at zero.
The exact authenticated progression/restart scenario now passes in 20.11 s,
and a specific no-rewrite recovered-prefix test passes. The fixture has a bounded
join diagnostic so a future stall reports failure.

`cargo fmt --all -- --check` and final
`cargo clippy -p spall_client --all-targets --all-features -- -D warnings` pass,
including the final pointer handoff changes. The combined full-workspace result
is recorded below once that run finishes. Concurrent animal foundation commits
were integrated by another session while checks ran; this report accepts only
the weather/audio integration and does not claim animal-system validation.
