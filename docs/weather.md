# Weather, snow cover and winter dormancy — ENG-195

Planning baseline: 2026-10-14. Implementation authorized later the same day.
**ENG-195 in progress; WX0–WX2 integrated and tested, WX3/WX4 now authorized.** Follow-up to the T25
game-system handoff.

## Existing foundation

- `spall_ecology::living::Season` and `LivingState` already supply a saved season
  clock. Reuse it; do not introduce a separate client season or offline wall time.
- `living.rs::advance` skips biomass, wood proposals and seeding during winter,
  but chronological age and seed due times include winter. Deferred updates use
  the season at the endpoint, so boundary accounting needs an audit and tests.
- `cover.rs::regrowth`, `CoverMemory` and cleared-record expiry use elapsed
  ecological time without winter dormancy. Untouched cover is stateless seasonal
  appearance; destroyed-cover memory is a bounded saved exception.
- Worldgen v4 supplies biome, moisture, temperature and regional vegetation.
  ENG-189/190 remain tracked in progress/in review: confirm integrated interfaces
  before modifying them. This plan does not mark those tickets complete.
- ENG-194 supplies biome ambience and catalogued rain/thunder recordings, but
  explicitly excludes weather playback. Preserve concurrent sound work. Rejected
  strong-wind/snowstorm candidates and SnowFall music remain excluded.
- Existing solid worldgen snow is separate from the seasonal snow layer.

## Player experience

| Weather | First presentation |
|---|---|
| Rain | Light through heavy camera-local streaks, wind slant, bounded splashes, wetter surfaces and reduced distant contrast |
| Storm | Heavy rain, gusts, darkened sky/cloud response, lightning illumination and timed thunder hooks |
| Snow | Drifting flakes, wind slant and soft haze; later feeds ground accumulation |
| High wind | Gusting particle/foliage response, usable without precipitation |
| Blizzard | Strong wind plus dense snowfall, near-ground blowing snow and smoothly reduced visibility |

Blend continuous precipitation/wind intensity. Blizzard combines wind and snow,
rather than introducing a separate competing simulation. Begin with one shared
weather front over the bounded world, modulated by local climate and exposure.
Cold regions may snow outside winter; mild winter regions need not gain snow.
Winter globally pauses plant growth initially, independent of visible weather.
Regional independent fronts are deferred.

Use a bounded 3D precipitation volume around the camera, depth-tested and masked
by shelter. Indoors suppress nearby particles/lens effects while rain remains
visible outside windows. Optional droplets/whiteout and reduced lightning flashes
must be tunable; retain readable controls, with no mandatory camera shake.

## Authority and compatibility

Server owns weather transitions, time, snow accumulation and validated footprints.
Clients interpolate presentation and generate cosmetic particles. Pure CPU policy
uses seed, authoritative time and game-authored settings. `spall_sim` applies
validated results at tick boundaries; protocol/store own explicit DTOs and bytes;
render/client own effects; sandbox owns presets and audio selection. Start with
modules in existing crates, not an empty framework or speculative dependencies.
Headless serving stays free of GPU/audio runtime dependencies.

WX0 specifies fields/units before implementation: world epoch/revision, effective
tick, transition duration, weather seed/continuation, precipitation kind/intensity,
wind vector/gust parameters, temperature modifier and bounded lightning IDs.
Use finite quantized ranges, stable IDs and explicit schema negotiation. Do not
reserve wire tags in this plan. Late join/repair receives current authoritative
state; stale epochs/revisions and malformed/oversized records are rejected.

Checkpoint weather/clock continuation consistently with the world journal cursor.
Old saves get a documented deterministic default/upgrade, never regenerated
terrain or revived plants. Reset clears effects, snow caches, footprints and
pending thunder. Offline wall time does not advance weather or growth.

Audio consumes a normalized presentation frame (precipitation/wind, shelter and
lightning position/tick/ID). The sound owner selects approved assets and blends
with biome ambience. Distance-delayed thunder uses bounded scheduling, TTL and
event deduplication; late join must not replay expired strikes. Missing wind/snow
recordings do not block visuals. No new downloads or replacement of sound work.

## Winter dormancy

Track active growing-season duration separately from chronological age. Winter
awards zero biomass, wood-growth, seed-production or cleared-cover regrowth
credit. Spring resumes retained pre-winter progress without winter catch-up or
seed bursts. Split deferred intervals at season boundaries so fair-budget
scheduling does not change outcomes. Root/support destruction and eligibility
checks continue in winter; dormancy does not grant immortality.

Cover clearing records must not expire while growth is paused. Retain stateless
cover plus its existing bounded clearing memory; no individual grass entities.
WX1 chooses a saved active-growth clock/clearing-progress representation and
mirrors it in client frames. Preserve existing progress through an explicit
migration; historical winter credit cannot be inferred from insufficient old
data, so document the deterministic conversion and that limitation.

## Ground snow layer

Ground snow is a layer of fine voxels resting on terrain, up to 0.5 m deep. It is
authoritative server state but separate from terrain: it adds no solid cells,
collision/support, movement penalties or per-voxel bodies, and walking never edits
terrain or creates topology transactions. Existing solid snow stays destructible.
Eligible upward-facing static terrain, including built roofs, is covered under
slope, temperature and sky-exposure rules. Snow may hide grass without killing it.
How it is drawn is under "Snow layer rendering" below.

Prefer sparse region-tiled surface records keyed by stable volume/location and
geometry revision, with quantized depth, accumulation/melt progress and update
tick. Support stacked surfaces in one x/z column: roof and sheltered floor must
not share snow. Query committed geometry, not just generated height maps.
Unknown geometry defers exposure/accumulation. Begin with bounded near-vertical
exposure queries and disclose the sideways-wind/shelter limitation.

Cap tiles processed per update, with fair scheduling and retained progress.
Client interest/cache residency must not decide distant snow. WX3 chooses compact
cold-region state or analytical catch-up backed by sufficient saved weather
history for exact continuation. Never discard authoritative snow to hide a cap.
Measure costs before freezing tile size, resolution, update rate and memory caps.

Topology commits invalidate changed surfaces and dependent exposure columns before
cover publication. Roof removal exposes the floor for future snowfall; destroyed
surfaces lose their snow and marks. A split drops this snow on transferred
surfaces and clears the source, preventing floating snow. Moving-body accumulation
is deferred. Warm weather melts gradually; fresh snow fills footprints. No melt
or rainfall mass is injected into the water solver in this phase.

## Footprints

Derive footfalls from server-validated grounded movement and distance/stride,
alternating feet and supporting turns. Never accept arbitrary client stamp
positions; flying, jumping and teleporting cannot make tracks. Bind each imprint
to the actual contacted snow surface and revision, including layered roofs.

Replicate temporary imprints regionally so nearby players share tracks and late
join/repair gets the live set. Optional predicted marks reconcile to authoritative
IDs. Specify snow-depth threshold, TTL, per-tile/global caps and deterministic
oldest-first eviction. Temporary tracks may expire; snow depth may not silently
disappear. Snowfall/melt fills or fades marks; edits/splits remove stale marks.
Persist snow depth, but clear temporary footprints on restart. They are visual
history, not durable gameplay evidence. Moving-body/vehicle tracks are deferred.

## Implementation slices and acceptance

ENG-195 tracks the overall scope. ENG-196 (WX0) and ENG-197 (WX1) are integrated
into the local test branch. ENG-198 owns the combined F10/audio/1 km playtest;
ENG-199 supplies authorized overrides and ENG-200 supplies bounded screen effects.
ENG-206 owns WX3 snow cover; ENG-207 owns the precipitation angle correction
and snow presentation. WX4 follows the frozen snow surface contract. Combined checks and launch
evidence are recorded in `docs/reports/ENG-198-integrated-weather-playtest.md`.

### Frozen clock boundary for independent WX0/WX1 work

The existing `LivingState.time_ms`, `credit_ms`, `start_season` and `season_ms`
remain the authoritative chronological ecology clock. Neither worker changes
their units, season ordering or `VegetationClock` journal semantics. Weather
consumes a read-only snapshot of that clock after ecology advances at a tick
boundary; no wall-clock/offline progression. Worlds without vegetation require
an explicit server-owned fallback clock, persisted by WX0, with one chosen
clock source per world. WX1 owns private active-growth accounting and compatible
vegetation state/frame migration; WX0 must not depend on its new internal fields.

This resolves the clock portion of WX0 before parallel implementation: WX1
depends on this frozen contract rather than the remaining weather protocol work.
The WX0 worker owns weather policy/protocol/sim/server/store/client ingress;
WX1 owns ecology living/cover and vegetation-specific consumers. Shared-file
changes require coordinator agreement. Neither worker edits audio or shared
planning files. Coordinator owns review, integration, status and work logs.

### Frozen WX3/WX4 surface contract (2026-10-15)

ENG-206 owns authoritative policy, persistence and snapshot ingress; ENG-207
owns the native renderer and client surface conversion. ENG-208 owns validated
movement-driven footfall policy and combined integration. Workers use isolated
worktrees; the coordinator owns common docs, window/menu wiring and review.

Snow uses 16×16 quarter-metre-cell tiles (4 m), with separate solid-cell Y
layers and disjoint 256-bit exposed-face cohorts. The server supplies the masks,
actual terrain brick revisions, stable cohort IDs and quantized depths. Clients
derive exact +Y face geometry only where the matching committed geometry is
known. They do not infer authority from local residency or sky exposure.
Surviving faces retain their snow across unrelated edits; newly exposed faces
start at zero, and removed/occluded faces lose their snow and tracks.

Depth is Q16 metres, capped at 0.5 m, with additional Q32 fractional progress
retained by the owner and save. Cold Snow/Blizzard accumulates at 2 mm/s times
precipitation intensity when temperature is at most 0°C. Positive temperature
melts at 1 mm/s times `clamp(temperature / 20°C, 0, 1)`. Rates are deliberately
visible within a short test session. The first slice uses WX0's uniform
authoritative temperature; regional climate modulation remains deferred.
Exact catch-up composes ordered clamped-add interval transforms, preserving
both saturation and snow/melt ordering. Separate accumulated snow/melt totals
are insufficient. History can be compacted only after all affected cohorts have
consumed it; state is never discarded to disguise a processing cap.

Wire protocol 7 adds version-1 `SnowSurfaceFrame` records: full replacements
for one tile/Y layer, including empty tombstones and current transient marks.
Epoch and snapshot sequence fence resets and stale frames. Up to 256 disjoint
cohorts and 64 marks fit one control record; regional delivery bounds transport,
while the authoritative fair scheduler advances independently of viewers.

Footfall candidates use server capsule travel, a 0.65 m stride, alternating
9 cm lateral offsets and a six-tick minimum step interval. Airborne movement,
teleports, movement-epoch changes and observation gaps clear stride credit.
The owner validates the actual contact cell, exposed-face bit, revision and
at least 2 mm of snow before admitting a mark. Marks have a five-minute TTL,
64-per-layer and 4096-global oldest-first caps, and fade as snowfall refills
them or snow melts. They are shared live history and clear on restart.

These are implementation contracts, not acceptance evidence. Runtime, recovery,
network and GPU results are recorded in the ENG-206/207/208 reports as delivered.

| Slice | Dependencies | Required evidence |
|---|---|---|
| WX0: state, clock and compatibility | Audit current T25/season interfaces | Freeze climate thresholds/durations and DTO/upgrade rules; deterministic transitions; exact restart continuation; two-client baseline/reset |
| WX1: winter dormancy | Frozen WX0 clock contract above; integrated ENG-189/190 interfaces | Winter biomass/wood/seeds/cleared cover frozen; summer-winter-spring and multi-season deferrals; no catch-up; destruction; save/load/client agreement |
| WX2: screen effects and sound hooks | WX0; integrated ENG-194 API for playback | All five profiles/transitions; roof/window/depth exposure; wind alone; teleport/resize/reset; reduced flashes; bounded particles and audio scheduling |
| WX3: snow cover | WX0, WX2 | Cold accumulation/warm melt; slopes/stacked roofs; roof edits/splits; unknown regions/distant catch-up; late join/repair; exact saved-depth continuation |
| WX4: footprints and integration | WX3 | Shared two-walker tracks; no airborne/fly/teleport stamps; stride limits; stale revisions; TTL/caps/refill/melt; region churn/reset/restart; storm plus destruction |

Small CPU fixtures cover policy, clocks, codecs and revisions. Each runtime slice
runs relevant scenarios and `cargo xtask check`. Use bounded existing xtask
scenario/capture interfaces after fixtures exist; no new command is claimed here.
Real GPU captures and matched weather-off/on traversal must include open ground,
forest, shelter, destruction and a 1024 m world. Two-client impaired-link/late-join
runs retain existing topology and streaming limits. Report unavailable gates unrun.

Provisional incremental targets, subject to measured review: owner weather/snow
work p95 <= 1 ms/tick; effects <= 2 ms GPU at 1080p on a documented reference
machine; resident snow/track state <= 32 MiB/client; steady weather/snow/track
payload <= 8 KiB/s/client excluding baselines. Record total frame/tick costs,
p99/max, CPU/GPU time, allocations, tile/particle/stamp counts, deferred work,
baseline bytes and repair latency. These are targets, not measured results.

## Deferred and next work

Wind forces on players/debris, lightning damage/fire, rain flooding, thermal
survival, slippery terrain, deep collidable snow and moving-body snow are outside
the first pass. Full-world destruction and multiplayer remain mandatory.

WX0 settles climate offsets/snowline, weather frequency/duration, clock ownership
and compatibility. WX3 settles surface storage and distant catch-up from measured
costs. WX4 settles visual track density/TTL. Next scoped work: **ENG-195 / WX0**
contract and integration audit (now integrated). ENG-198 / WX2 combined validation
is accepted with the limitations recorded in its report. The user authorized
WX3 snow cover and then WX4 footprints on 2026-10-14, and requested a correction
to rain/snow appearing too sideways. Normal precipitation will use one shared
fall/drift velocity for particle motion and ribbon orientation; blizzard and
wind-only cues retain stronger wind response. Surface storage and revision
contracts are frozen before dependent footprint/render implementation begins.

WX3/WX4 implementation and generated-world visual integration were reviewed on
2026-10-15. F10 controls snow visibility independently of precipitation, and
two real clients share server-validated tracks. Exact final checks, 1024 m
captures, live launch and remaining profiling/streaming limits are in
reports/ENG-207-snow-playtest.md. ENG-207's precipitation correction is accepted;
ENG-206/208 remain open for the documented combined acceptance follow-ups under
ENG-195. No matched performance or impaired-link snow gate is claimed.

## Snow tiers, blizzard and season selection

Snow is one `WeatherKind` at three `precipitation_q16` tiers (`spall_protocol::SnowTier`):
light 0.25, medium 0.5, heavy 0.85. An automatic Snow roll picks a tier from the seed;
`AdminCommand::SetWeather { kind, intensity }` forces one (`intensity` applies to Snow only,
default medium) and the tier survives ticks because it lives in the state's
precipitation. Blizzard is precipitation 0.95. The F10 weather label shows the tier, and
`--shots` accepts `weather:snow-light|snow-medium|snow-heavy` and `season:<name>`.

Client particles (`spall_client/src/weather.rs`): snow uses a 24 m box of 8-12 cm flakes
falling 1.0-1.5 m/s with a per-flake sway, on a world-anchored lattice wrapped around
the camera. Count scales with the smoothed precipitation (about 2k / 4k / 7k) and the
blizzard is 1.5x heavy (about 10.5k) of fast near-horizontal streaks, 8-14 m/s along the
wind, pulsing with gusts. The cap is `spall_render::MAX_WEATHER_PARTICLES` (12,288).
Shelter still comes from the 5x5 sky-exposure grid. Heavy snow lowers
`WeatherLighting.visibility` to about 0.7; a blizzard pulses it between about 0.3 and
0.15. The renderer turns visibility into a whiteout through the scene's distance fog
(`weather_fog`: the end shrinks toward 22 m, the start toward the camera, and the colour
goes grey-white), plus a stronger full-screen overlay (alpha cap 0.36).

`AdminCommand::SetSeason { season }` (0 spring .. 3 winter, F10 buttons) re-anchors the
vegetation `start_season` and the weather `start_season` together, keeping elapsed time,
so the chosen season is current at once, snow eligibility and temperature follow, and the
season keeps advancing from it. The vegetation state is journalled and the next weather
and vegetation keyframes (within a second) carry the change to clients.

## Snow layer rendering

(`spall_render/src/snow_mesh.rs` meshes, `spall_client/src/snow.rs` feeds it.) Each
exposed +Y terrain face with snow is a 4x4 grid of 6.25 cm columns. A column's height
is `floor(depth / 6.25 cm + u)` whole steps, where `u` in [0, 1) is a deterministic
hash of the column's world position (a 3x3-column block hash plus a smaller
per-column hash, so equal heights form soft lumps). The mean surface therefore
follows the authoritative depth exactly, and depth below one step shows as a sparse
dusting that fills in as snow arrives, with no flat translucent sheet and no jump
when a step is crossed.

- Opaque, depth-writing geometry drawn through the lit terrain pipeline with
  procedural snow materials (`SNOW_MATERIAL_BASE`, a packed-snow variant), so it is
  shadowed and fogged like terrain. Colour varies a few percent per column.
- Tops are merged greedily and side faces are emitted wherever a column stands higher
  than its neighbour or the snow meets open air; where a solid cell stands beside the
  support plane the side is omitted. Never one cube per column.
- Levels of detail per 4 m tile: tiles within 14 m (hysteresis to 18 m) use 6.25 cm
  columns and footprints; farther tiles use one 0.25 m column per cell at the
  authoritative depth. One GPU mesh per tile and layer.
- The existing exposure logic stays authoritative: the client revalidates each face
  against the terrain replica (solid below, air above) and never treats unknown
  geometry as air.

**No pop.** Tile meshes are replaced whole, nearest first, within a 4 ms slice per
update, and the old mesh stays drawn until its replacement lands. Two causes of the
reported disappearing sheet were found and fixed:

- *Measured:* the client's snow-frame cache (4096 frames) was smaller than the
  working set of a heavy-snow view (about 5,500 frames at 96 m) and evicted the
  oldest frame first. The server resends frames in a slow sweep that starts at the
  player, so the oldest frames were the nearest ones: every sweep, all snow within
  about 50 m vanished until the sweep came back (a capture log showed the nearest
  frame jumping from 0 m to 45-57 m, in step with the missing snow). The cache now
  holds 6144 frames and, when full, drops the frames farthest from the player
  (`ReplicaWorld::set_snow_focus`).
- *By inspection:* the old converter dropped every face whose brick revision
  differed from the frame's, so a terrain edit near the player hid the snow until
  the server resent the tile. A revision change alone no longer hides snow (the
  sampled geometry decides), and a cell whose geometry is momentarily unknown keeps
  what it showed.

## Carved footprints

An authoritative footprint carves the near columns inside a sole about 38 x 16 cm
(a tapered capsule from heel to ball, a little over life size so it reads on the
6.25 cm grid), oriented by the heading, alternating feet. Deep snow is removed down to one packed step (the
floor shows in a bluer packed-snow colour) and a one-step rim heaps up just outside
the sole; in thin snow the print sits half a step down. Strength fades the dent as
new snow refills it. TTL, per-layer and global caps, fade by snowfall and melt
removal are unchanged and server-side; carving is presentation of those marks only.

## Wind

One function, `spall_protocol::wind_at(weather_seed, clock_s)`, is the wind for
everything that shows it: particle velocity and streak slant, blizzard whiteout,
and the wind-bed audio gain. `clock_s` is the authoritative weather clock
(`clock_ms / 1000`; clients add the local time since the last 1 Hz snapshot), so
every client sees the same heading and strength at the same moment. The profile's
`wind_x_q15`/`wind_z_q15`/`gust_q16` fields are no longer used for presentation.
The server sim does not use wind.

- Heading: a new random heading anywhere in 360 degrees every 20-90 s (55 s grid,
  35 s jitter), reached by a smoothstep turn lasting 5-15 s, plus about 9 degrees of
  wobble.
- Strength 0..1: 20 s slots that are a lull (about 20%, never two in a row, level
  0-0.03), moderate (40%, 0.25-0.5) or strong (40%, 0.75-0.9), blended over 6 s at
  slot edges, with 4-7 s surges of up to +-0.12 riding on the stronger slots.
- Speed per kind (`Wind::speed_mps`): blizzard 17.5 m/s x strength^1.3 (a lull is
  under 1 m/s, snow falls straight down); snow tiers 3 m/s x strength^1.8; rain 3.5,
  storm 8; wind-only weather 2 + 10 x strength.
- Blizzard whiteout thins to about 0.6 visibility in a lull and closes to 0.15 in
  a strong gust; the blizzard and wind-only audio beds run at 30% gain in a lull and
  full gain in a gust.
- Each blizzard streak still swirls on its own slow circle (local time).

## Underwater

When the camera eye is below the replicated water surface (`eye_below_surface`),
weather particles, whiteout fog, storm dimming and lightning flashes fade out over
about a second (`WeatherPresentation::set_submerged`), the underwater shader look
takes over, and weather audio drops to 7% gain. Surfacing fades everything back.

## Ambience under weather

Weather ducks biome ambience per layer (`ambience_mix`): other layers by up to 65%,
water tracks (`BiomeAudioConfig::water_tracks`: stream, ocean, beach) by at most a
quarter of that, and water layers take mix slots before other layers. Weather beds
play on their own `AmbienceAudio` worker, so they never evict ambience layers.

## Winter Crossing profile (ENG-269)

`sandbox-server --worldgen <preset> --scenario-winter` selects a scenario
profile (requires `--worldgen`; replaces `--season` and `--animals`). Without
the flag nothing changes.

- **Weather.** `WeatherProfile::WinterCrossing` (`spall_protocol`) runs 5-minute
  fronts (`WINTER_CROSSING_FRONT_MS`) at a fixed -8 C base whatever the clock
  season: 18 % blizzard, 67 % snow (heavy 60 %, medium 30 %, light 10 %), 10 %
  clear, 5 % high wind; no rain or storms. At the default 20-minute day that is
  a blizzard about every 1.7 days, 6 in-game hours per front. The server picks
  it at startup (`CustomWorld::with_weather_profile`,
  `SimulationConfig::weather_profile`); it is not persisted.
- **Held winter.** The season clock starts in winter with `HELD_SEASON_MS`
  (about 31 years) per season, so it never advances.
- **Animals.** `sandbox::winter_crossing::ANIMAL_TARGETS` (deer 120, crows 60,
  trout 12, bears 4, snow leopards 6, wolves 12 = three packs), born around a
  25-point grid across a 1 km map. Animals still need an ephemeral world (no
  `--save`).
- **Wolves.** `WOLF_BITE_DAMAGE` is 12 (nine bites kill); one hunt is capped at
  `WOLF_HUNT_BITES` = 3 (see animals.md). This applies to all worlds.

Numbers: `docs/reports/ENG-269-winter-tuning.md`.
