# ENG-199 / WX2a — Authorized weather controls

Date: 2026-10-14. Branch: `codex/eng-196-weather-foundation`.

## Contract implemented

The protocol exposes `AdminCommand::SetWeather { kind: Option<WeatherKind> }`; the six supported kinds are Clear, Rain, Storm, Snow, HighWind and Blizzard. `None` returns to seeded automatic weather. The server applies this only when the existing `admin_commands` gate is enabled and responds with the selected mode and normalized precipitation/gust Q0.16 values. Forced snow and blizzard ignore the automatic seasonal snow threshold for testing.

`WeatherState` schema 2 appends `override_kind`, which is stored in the existing canonical checkpoint and journal state and survives recovery. Reset installs a fresh automatic state. Existing schema-1 checkpoint and journal bytes use an explicit `WeatherStateV1` migration to schema 2 with no override. Wire schema and negotiated protocol version are 6 so mixed protocol builds refuse before attempting the changed `AdminCommand`/weather record layouts.

Weather ingress compares epoch, revision and effective tick against the prior weather stream only; motion tick no longer rejects valid delayed weather frames. Every full replacement baseline advances the replica-local weather epoch fence, including when no weather frame has arrived. Network code invokes full baseline installation for initial join and the explicitly tagged reset transfer; per-brick repair uses its own patch path and does not touch the fence. The explicit reset transfer also fences out weather frames effective at or before the reset checkpoint tick, until the next state arrives. Automatic and forced storm lightning IDs and event ticks remain stable for the same event.

## Checks and results

- `cargo check -p spall_protocol -p spall_sim -p spall_server -p spall_client` — passed before the storage migration fallback was added.
- `cargo check -p spall_store -p spall_server` — passed after adding schema-1 recovery fallback.
- `cargo test -p spall_protocol weather::tests` — 5 passed, including forced snow levels, initial tick-zero validity, stable storm event, and explicit schema-1 DTO conversion.
- `cargo test -p spall_client delayed_weather_uses_its_own_stream_and_reset_fences_without_prior_weather` — 1 passed; a reset before any weather frame rejects epoch 1 and accepts epoch 2 despite effective ticks older than motion.
- `cargo test -p spall_sim weather_override_survives_ticks_and_reset_clears_it` — 1 passed.
- `cargo test -p spall_server weather_admin_gate_forces_snow_persists_through_ticks_and_restores_automatic` — 1 passed; disabled admin is rejected without mutation, enabled admin forces snow, and `None` restores automatic state.
- `cargo test -p spall_store --test durability weather_clock_checkpoint_and_journal_survive_restart_exactly` — passed with a forced snow override through checkpoint and journal recovery.
- `cargo test -p spall_store --test durability schema_one_weather_checkpoint_and_journal_migrate_to_automatic_state` — 1 passed for schema-1 checkpoint and journal recovery.
- Full workspace and live 1 km two-client test are coordinator-owned and were not run by this worker.

## Remaining risks and integration

The server keyframe cadence remains one second; a forced override is included in the next bounded keyframe, with no separate immediate-send path. The server unit scenario directly exercises the same authorization helper used by command dispatch; final live F10/two-client behavior remains for the coordinator's combined playtest. The protocol bump requires all connected clients and servers to use schema 6 together. GPU/audio performance and capture were not measured here.

Restart regression found during coordinator integration: a recovered journal cursor was passed to clean-shutdown durability wait, while a fresh `PersistPipeline` incorrectly initialized its acknowledged cursor to zero. Static recovered worlds with no new journal entries therefore waited forever on a prefix the store had already proven durable. Pipeline setup now seeds its acknowledged cursor from that recovery result; new worlds still start at zero. The network fixture now bounds its post-restart server join at 45 seconds so a future regression fails with a diagnostic instead of hanging indefinitely.

Additional checks for this correction:

- `cargo test -p spall_server --lib recovered_durable_prefix_needs_no_rewrite_before_shutdown` — 1 passed.
- `cargo test -p sandbox --features client --test progression_network -- --nocapture` — 1 passed; the recovered second server exited normally and the full authenticated crafting/restart scenario completed in 20.11 seconds.
- `cargo fmt --all -- --check` — passed after formatting the fixture change.

Next: coordinator integration and the combined live 1 km test.
