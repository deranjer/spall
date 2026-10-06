# ENG-126 large-world portable integration

Implemented 2026-10-06. The rebuilt `dist/spall-portable` launches the selected
1024 m seed-1 world without cargo or the source checkout. This accepts the
functional portable integration for a larger memory class; it does **not**
accept the engine memory, join-time, physics or fluid performance gates.

## Changes and contract decisions

- `spall_worldgen/generate.rs`, sandbox `worldgen_scene.rs`, and
  `spall_protocol/water.rs`: bounded fine water capacity is 64 Mi voxel cells.
  The original seed-1 preflight box remains 47,421,308 cells. Actual domains
  retain the existing outward coarsening alignment. Nothing is cropped.
- `spall_sim/water.rs`: canonical water schema 2 permits the extended domain;
  states within the old 32 Mi bound still emit schema 1. Older binaries reject
  schema 2. Exact fractions, trapped amounts, ledgers and recovery ordering
  are unchanged. Coarse-solid seed overlaps now displace conservatively or
  enter the trapped ledger instead of silently losing authored water.
- Protocol `segment.rs` / `records.rs`, net `conn.rs`, server `baseline.rs` /
  `serve.rs`, client `net.rs`: wire the existing segmented codec into actual
  joins and resets. New sentinel capability negotiates world version 3 with a
  256 MiB cumulative compressed ceiling. Legacy version 1/2 keep 64 MiB;
  legacy whole-blob decompression keeps 256 MiB. Default decoded segments are
  4 MiB, parts <=1 MiB, declared decoded totals <=16 GiB. Server encoding
  expands one segment of immutable snapshots at a time, not the whole world.
- The client checks explicit staging admission before segment allocation,
  validates parts, counts, bytes, hash chain and cursor, then swaps atomically.
  Default budget remains 4 GiB; the portable 1024 m launch explicitly uses
  8 GiB. `ClientNetConfig` callers now supply that budget. Replica replacement
  estimates count real dense/uniform representation instead of charging every
  uniform brick as dense. Existing geometry counts against reset admission.
- One queued version-3 baseline has its own <=256 MiB slot in the outbound
  FIFO. One writer-owned transfer may coexist with one queued transfer.
  Ordinary reliable backlog retains its 8 MiB / 2048-record bounds, ordering
  and disconnect policy. A second queued streaming baseline is refused.
- The client owns a bounded 64-record control pump throughout joins, bulk
  decoding, cache warming and replacements. It keeps reading heartbeats even
  when `BaselineEnd` arrives before the last bulk bytes. Canceling that pump
  closes the connection. Initial joins have a session deadline.
- Editor `main.rs`, `ui.rs`, `worldgen_panel.rs`, xtask `play.rs`, and
  `package-portable.ps1`: capacity refusals, elapsed loading status, periodic
  transfer progress and failures remain visible. A window opening no longer
  means the world is ready. A ready marker is retained once observed. The
  1024 m choice explains its memory requirement. Seed/size/season wiring,
  authority, reset UI, sun controls and existing HUD remain intact.

The generator version and terrain golden digest are unchanged. The existing
coarsening selector stops at factor 8; the largest domain can exceed the
50,000 fluid-cell target. Solver tolerances, dt and workload were not weakened.

## Measured evidence

| Run | Result |
| --- | --- |
| Original packaged 1024 m launch | Rejected 47,421,308 fine cells against 4,000,000 before bind |
| Version-2 segmented wire experiment | Rejected at the unchanged 64 MiB cumulative compressed limit |
| New real-QUIC 1024 m seed-1 join | Client exit 0, 196,608 bricks, 94,280,212 compressed bytes |
| Join timing | 92,981 ms receive/verify/install counter; ready confirmed at 95,042 ms from connect |
| Catch-up | 1 applied transaction, 0 rejected; 34 water keyframes, 0 water keyframe errors |
| Headless peak working set | Server 15,141,384,192 bytes (14.10 GiB); client 4,449,615,872 bytes (4.14 GiB) |
| Portable actual GPU window | Exit 0, 196,608 bricks installed about 98.2 s after client start; winter launch |
| Stationary window telemetry | Approximately 54–60 FPS, 1,051 terrain chunks, 1,028,464 triangles |

The headless client ended with topology hash
`50ec528c8c502c55d8c7912bdc01d49e2454a76dd4523a3c8d7366c8fe2fa0b1`,
matching the authoritative final hash of the preceding seed-1 3000-tick run.
The successful join probe's server was explicitly terminated by its 240 s
supervisor deadline after the client passed; it produced no final server
summary. Do not describe that supervisor outcome as a complete server run.
An earlier overnight clock-jump probe was inconclusive and is excluded.

Headless evidence: `.local/runs/eng126-large-join-pump/client.summary.json`,
process logs, and `.local/eng126-large-join-pump.ps1`.

Portable smoke command, from the packaged directory:

```text
xtask.exe play --portable --worldgen showcase --seed 1 --worldgen-size 4096 --season winter --ticks 12000 --output runs/eng126-1024m-gpu --shots "wait:115;walk;wait:2" --shots-dir runs/eng126-1024m-gpu/shots
```

The bounded supervisor `.local/eng126-portable-smoke.ps1` captured logs and
cleaned up only its own processes. xtask waited five seconds for the server
after the window closed, then used its owned-child cleanup. The capture was
visually inspected: terrain, trees, sky/clouds and game controls are present.

![1024 m portable game capture](../../dist/spall-portable/runs/eng126-1024m-gpu/shots/shot-00.png)

## Exact checks

All the following completed successfully unless explicitly qualified:

```text
cargo test -p spall_protocol                         # 53 tests
cargo test -p spall_sim --lib water::tests           # 9 tests
cargo test -p spall_client --lib                    # 86 tests
cargo test -p spall_server --lib                    # 75 tests
cargo test -p spall_server --test segmented_join --test late_join_session --test replication_session --test water_replication
                                                    # 8 real-network scenarios
cargo test -p spall_net --lib                       # 24 tests
cargo test -p spall_net --test transport malformed_length_and_oversized_transfer_stay_bounded
cargo test -p spall_server --release --test water_persistence -- --include-ignored
                                                    # 2 tests, extended domain + failed commit
cargo test -p spall_worldgen --release              # 19 ordinary tests; 3 hardware/measurement tests ignored
cargo test -p spall_worldgen --release --lib large_world_column_capacity -- --ignored --nocapture
cargo test -p spall_editor --lib                    # 23 tests
cargo test -p spall_editor --bin spall-editor        # 16 tests; large capacity test separately run
cargo test -p spall_editor --release --bin spall-editor worldgen_panel::tests -- --include-ignored
                                                    # 8 tests including 1024 m preview
cargo test -p xtask                                 # 34 tests; 1 external diagnostic ignored
cargo check -p sandbox --features client --all-targets
cargo fmt -p spall_protocol -p spall_net -p spall_sim -p spall_worldgen -p spall_server -p spall_client -p sandbox -p spall_editor -p xtask
cargo clippy -p spall_protocol -p spall_net -p spall_sim -p spall_worldgen -p spall_server -p spall_client -p spall_editor -p xtask --all-targets -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
& tools/package-portable.ps1
git diff --check
```

Strict clippy without the two allowances reports pre-existing day/night
`field_reassign_with_default` and window `finish_frame` argument-count findings.
Those unrelated implementations were preserved. The final command with those
two named lint allowances passes. No zero-test filtered invocation is counted
as evidence.

New regressions cover negotiated streaming joins, insufficient admission,
mixed-brick exact hashes, segment splitting, cumulative caps, early end with
delayed bulk beyond the watchdog interval, bounded queued baseline slots,
compact replacement admission, water schema limits and exact durable recovery.
Existing network destruction, repair, restart and admin-reset scenarios pass.

## Remaining risks and next assignment

Server peak memory exceeds the 8 GiB target; headless client peak exceeds the
4 GiB target. Recommend 32 GiB host RAM for this largest local launch. This is
a resource warning backed by measurement, not a new engine performance target.
Stationary FPS is a smoke observation, not a G5 percentile/stress acceptance.
Full 1024 m reset/destruction stress, impaired-network joins, multi-client peak
memory, fluid feasibility and manual mouse-look/admin review remain unrun.

The portable ENG-126 implementation is complete. ENG-113/114 and ENG-30/G3/G5/G6
acceptance remain separate. Next unblocked assignment: **ENG-114**, measure and
reduce large-world initialization/memory and exercise the full-size reset and
destruction scenarios while preserving the current authority and data bounds.
