# ENG-196 / WX0 — Weather foundation

Date: 2026-10-14. Branch: `codex/eng-196-weather-foundation`.

## Contract implemented

`spall_protocol::WeatherState` is the versioned CPU state and `WeatherSnapshot` is its bounded reliable control record (tag 24, wire schema 5). Precipitation and gust are Q0.16, wind components are Q1.15, temperature is centi-degrees Celsius. The deterministic policy hashes the world seed and 30-minute profile bucket; each profile advertises a 5-minute transition. It emits clear, rain, storm, snow, high wind and blizzard. All state is checked before accepting it.

Weather reads the existing ecology clock after `advance_vegetation` at the simulation tick boundary. The snapshot includes the existing `start_season` code and `season_ms`; no ecology growth fields or clock semantics changed. A world without vegetation uses a fixed server tick clock (`floor(tick * 1000 / 60)` milliseconds) with Spring and 900,000 ms seasons. The chosen clock source is stored in state and does not change during a world. No wall or offline time is applied.

Climate policy uses Spring 8 C, Summer 22 C, Autumn 10 C, Winter -8 C; rain/storm add 1 C and snow/blizzard subtract 1 C. Snow and blizzards require <=2 C. For each 30-minute seeded bucket the roll weights are clear 30%, rain 30%, storm 10%, snow-or-rain 15%, high wind 8%, and blizzard-or-storm 7%. Precipitation levels are clear 0, rain 0.4 or 0.3, storm 0.9 or 0.6, snow 0.3 and blizzard 0.8 in normalized Q0.16 units; gust levels are 0.1, 0.15, 0.2, 0.5, 0.64, 0.84 as encoded by the policy.

Checkpoint state lives in an additive `checkpoint_weather` SQLite table with a checksum, preserving the existing world save schema and older database layout. A new appended journal variant records weather transitions. Old saves without the table recover with a documented deterministic seed/tick upgrade. Recovery validates checkpoint/journal DTOs. The server sends a small current keyframe every 60 ticks; the client rejects older ticks and epochs, clears weather on atomic baseline replacement, and requires a newer epoch afterward. World reset advances the weather epoch. Topology revisions/order are not modified.

## Files changed

- Protocol/policy and transport: `crates/spall_protocol/src/weather.rs`, `records.rs`, `lib.rs`, `codec.rs`; `crates/spall_net/src/message.rs`.
- Authoritative tick and clock: `crates/spall_sim/src/sim.rs`, `vegetation.rs`.
- Journal/checkpoint/recovery: `crates/spall_store/src/{dto,db,recover,schema}.rs`, store test fixtures; `crates/spall_server/src/{persist,serve,persist_pipeline}.rs`.
- Client ingress: `crates/spall_client/src/{net,replica}.rs`.
- Live replication assertion: `crates/spall_server/tests/column_water_replication.rs`.

## Checks and results

- `cargo check -p spall_protocol -p spall_net -p spall_sim -p spall_store -p spall_server -p spall_client` — passed.
- `cargo test -p spall_protocol -p spall_sim -p spall_store` — passed before the final clock-field alignment; then the focused post-alignment tests below passed.
- `cargo test -p spall_protocol weather::tests` — 4 passed, including all six profile families, deterministic repeat, wire round-trip and malformed-input rejection.
- `cargo test -p spall_sim weather_clock_tests` — 1 passed; verifies fallback clock and reset epoch.
- `cargo test -p spall_store --test durability weather_clock_checkpoint_and_journal_survive_restart_exactly` — passed.
- `cargo test -p spall_server --test column_water_replication client_receives_column_water_late_joiner_converges_and_reset_reseeds -- --exact --nocapture` — passed; a real QUIC early client, late joiner and reset observed weather snapshots.
- `cargo xtask check` — formatting, clippy and workspace compilation passed. Workspace tests began; it remained on `sandbox/tests/progression_network.rs::authenticated_crafting_retries_disconnects_isolates_players_and_survives_restart` beyond six minutes with no test result. The server reached durable tick 1200; I stopped the non-progressing runner. Full workspace test completion is unconfirmed.
- No GPU capture, audio playback or performance target was measured; those are outside WX0.

## Risks and next work

The temperature calendar is a deliberately simple global climate offset; biome/latitude/exposure modulation and presentation belong to later weather slices. Transition duration is replicated policy metadata; blending is not rendered in WX0. The existing server sends one-second refreshes, so a new client can wait up to one refresh interval after its baseline. Wind-driven forces, snow surfaces, particles, sound and lightning illumination remain out of scope.

No measured CPU/GPU performance claim is made. Next: coordinator integration and review with ENG-197 / WX1; after acceptance, the next weather slice is WX2.
