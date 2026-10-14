# Weather, snow cover and winter dormancy — ENG-195

Planning baseline: 2026-10-14. Implementation authorized later the same day.
**ENG-195 in progress; no accepted weather gates.** Follow-up to the T25
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
- Existing solid worldgen snow is separate from the proposed seasonal coating.

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

## Thin ground snow

First snow is a shallow visual coating: no new solid voxels, collision/support,
movement penalties or per-voxel bodies. Existing solid snow stays destructible.
Coat eligible upward-facing static terrain, including built roofs, with slope,
temperature and sky-exposure rules. Snow may hide grass without killing it.

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
surfaces lose coating and marks. A split drops this visual coating on transferred
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

ENG-195 tracks the overall scope. ENG-196 (WX0) and ENG-197 (WX1) are assigned
in separate worktrees to Luna Medium workers. Later slices remain unassigned.

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
contract and integration audit, once implementation is requested.
