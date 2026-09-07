# Validation and operating contract

These commands and fixtures are **planned interfaces**. T00 implements
`cargo xtask check`, the bounded GPU-free portion of `cargo xtask smoke`, and
the offline clear-window capability smoke (`cargo xtask smoke --graphical`).
T09 adds `cargo xtask net-check`: an in-process QUIC transport harness (one
server, N authenticated headless clients, an optional opaque UDP loss proxy,
every channel exercised, bounded teardown) that writes `summary.json`,
`net.jsonl`, and `metrics.json`. `session`, `scenario`, `capture`, `bench`, and
`crash-test` still return an explicit unavailable-capability result until their
listed tasks are delivered. The `sandbox-server` host still records but does not
bind `--listen`; wiring `spall_net` into the host and the documented
connected-client command is T10. All numerical limits are provisional
acceptance targets. None is a measured result.

## Agent operation without an editor

Use Cargo aliases so `cargo xtask` runs the xtask package. The orchestrator launches built binaries directly after one build, avoiding multiple concurrent Cargo builds and target-directory lock contention.

```powershell
# Build, lint, and CPU checks. Implemented first in T00.
cargo xtask check
cargo xtask smoke --ticks 60

# T09: QUIC transport + fault harness, in-process, no GPU. Real loopback QUIC
# through an opaque UDP loss proxy; reliable records must survive the loss.
cargo xtask net-check --clients 2 --loss-percent 2 --output .local/runs/net

# Dedicated server, bounded automation run, no window/GPU dependency.
cargo run -p sandbox --bin sandbox-server -- --world .local/worlds/dev --seed 42 --listen 127.0.0.1:5000 --ticks 3600 --log-json .local/runs/server.jsonl

# Direct render window and input, no menus.
cargo run -p sandbox --bin sandbox-client -- --connect 127.0.0.1:5000 --server-fingerprint .local/session/server.fingerprint --join-token-file .local/session/join.token

# One server and two clients; collects outputs and terminates its own processes.
cargo xtask session --clients 2 --scenario fixtures/scenarios/tower-cut.toml --ticks 1800 --output .local/runs/tower-cut

# Separate-process bots exercise the actual transport without a GPU.
cargo xtask scenario --name destruction-network --clients 2 --headless-clients --ticks 3600 --output .local/runs/network

# Packet impairment means encrypted UDP packets through the test proxy.
cargo xtask scenario --name late-join-collapse --clients 3 --headless-clients --rtt-ms 100 --jitter-ms 20 --loss-percent 2 --output .local/runs/join

# Offscreen GPU rendering still requires a supported GPU/driver.
cargo xtask capture --scene colored-room --camera fixtures/cameras/colored-room.toml --size 1920x1080 --frames 120 --output .local/runs/lighting

# Release build, named fixture, fixed workload; emits machine-readable metrics.
cargo xtask bench --suite engine-slice --clients 8 --warmup-seconds 30 --duration-seconds 120 --output .local/runs/bench

# Defined fault points around journal/checkpoint publication.
cargo xtask crash-test --suite persistence --output .local/runs/crash
```

Interactive controls: mouse look, WASD, jump, primary tool, alternate placement, debug free-camera toggle, Escape to exit. Controls are configuration data. Scenarios can run every action without synthesizing keyboard/mouse events.

No runtime command shell or arbitrary code execution over the game socket. Agent control uses local scenario files or a loopback authenticated development command channel with a fixed command schema: load scene, move player, use tool, step ticks, capture, inspect hashes, save, quit. Disable the development channel in Internet server builds/configurations.

Every tool supports `--help`, explicit output directory, bounded tick/time runs, structured errors, and exit codes: 0 pass, 1 failed check/scenario, 2 invalid arguments/configuration, 3 missing environment capability. Missing GPU is reported separately from a passed graphics test. Crashes/timeouts are failures, not successful early completion.

xtask generates per-run session credentials and fingerprints, waits for structured readiness rather than a guessed sleep, tracks child PIDs, and cleans up only its own processes. Hide helper/server consoles on Windows; show a render window only when a graphical client is requested. Logs must not contain tokens. Temporary worlds and credentials go under ignored `.local/`; fixture inputs are tracked. A failed run preserves diagnostic artifacts.

## Required evidence

Each run writes `summary.json`, per-process JSONL logs, canonical topology hashes, resolved configuration/seed, scenario revision, compiler/dependency lock hash, OS, CPU, GPU/driver/backend if used, and build profile. Graphical runs also save requested PNGs and a frame sequence for motion evaluation. Record warmup duration and exclude warmup from timing percentiles.

Measure client frame time, main-thread time, GPU pass timings, server tick/physics time, edit acknowledgement and commit latency, support search work, collider build time, queue depth/age/bytes, allocations/resident memory, GPU cache memory, topology/hash failures, transport bytes, reliable backlog, and join duration. Unsupported GPU timestamps must be marked unavailable; do not fabricate a GPU time from CPU submission time.

At a gate, retain raw metrics alongside a concise report in `docs/reports/Gx.md`. Report workload counts actually achieved: players, bodies, bricks, occupied cells, cut volume, and pending work. Otherwise a faster result may simply be a smaller workload.

## Correctness fixtures

| Fixture | Required assertion |
| --- | --- |
| coordinates | Euclidean mapping and DDA at negative/exact boundaries; no overflow on invalid extreme inputs |
| brick-reference | Random edit sequences match a dense oracle; old immutable snapshots stay unchanged |
| mesh-seams | Face coverage matches reference across all neighbors; missing-neighbor arrival invalidates halos |
| bridge-cross-brick | Remove the final support crossing four or more bricks; exactly the unsupported span detaches |
| support-offscreen | Support beyond loaded/render range is resolved correctly after eviction and reload |
| bottom-plane-removal | All in-bounds anchor cells remain destructible; removing the last ones releases remaining solids |
| rotating-body-cut | A moving hollow voxel body splits without moving geometry at the split instant or duplicating mass |
| conflicting-edits | Concurrent plans commit in valid order; stale results retry and eventually progress |
| collision-commit | Queries and movement reflect the same committed topology, including placement and deletion |
| destruction-network | Two clients agree with server geometry after cuts, duplicates, loss, reorder, and repair |
| late-join-collapse | A third client obtains a consistent current world while objects split and move |
| interest-crossing | A multi-region body remains one entity; entering clients receive required dependencies |
| save-air | Completely mined bricks remain empty after checkpoint, eviction, restart, and regeneration |
| crash-transfer | Crash before/after source-removal/child-create commit cannot recover partial ownership |
| malformed-input | Invalid lengths, huge coordinates, compressed bombs, invalid IDs/NaNs, excessive rates reject within bounds |
| cantilever-strength | Material capacity changes failure outcome; damage/bonds survive restart and replication |
| sleep-wake | Settled persistent rubble wakes before nearby interaction and remains destructible |
| scene-switch | Old asynchronous results never enter a new world/session at reused coordinates |

Use property tests for coordinates, storage/edit conservation, and codec bounds. Use tiny reference algorithms for topology/mesh coverage. Physics tests use position/energy tolerances where appropriate; exact hashes test topology, not floating-point trajectories. A rendered screenshot is not proof of collision or replication correctness.

## Gate workload and targets

Choose and record a reference Windows gaming PC and dedicated-server CPU in T00. Provisional envelope: desktop six-core-class CPU, 32 GiB system RAM, discrete GPU with 8 GiB VRAM. This is a test planning envelope, not a published minimum specification or purchase recommendation. Repeat backend smoke tests on D3D12 and Vulkan when available. Linux headless CI is desirable from the start; Windows is the initial client target.

### G0 — reproducibility

Clean checkout builds with the pinned toolchain. CPU checks need no display adapter. Bounded smoke runs are reliable in CI, including cancellation. Instructions identify actual Windows build prerequisites discovered in T00. Tooling cannot claim later commands work before their tasks land.

### G1 — real destruction and two clients

64 x 32 x 64 m world, 25 cm terrain cells. Include a 12 m hollow tower/bridge spanning brick boundaries, an excavatable slope, and a moving hollow test volume. Run 60 seconds at 60 server ticks/s; after initial cuts, drive 10 tool requests/s total across two clients. Include one cut affecting a 4 m diameter sphere and a 64-brick connected body stress case.

Required: exact ownership/conservation, zero unrepaired topology mismatch at quiescence, all accepted work eventually completes within the stated budget, and no permanent hidden support. Normal single-brick edit server commit target: p95 <=100 ms without network delay. Ordinary structure split target: <=500 ms; designated large-collapse stress target <=2 s before consistent activation. These are gates to measure, not guarantees from the chosen algorithms.

For overload tests, requests beyond admission capacity may return busy. The report must distinguish requested, rejected, queued, and committed counts. Rejecting every expensive action does not satisfy the gate: all named mandatory edits must complete.

### G2 — graphics quality and cost

1920 x 1080, fixed exposure, recorded sun/camera/material settings. Test daylight terrain, a colored enclosed room, an emissive source behind an occluder, thin walls, moving debris, and rapid destruction. Capture both a settled frame and at least 120 consecutive moving frames.

Required visual behavior: correct silhouettes/materials, soft/contact shadows, visible diffuse indirect illumination, emissive lighting, dark enclosed areas, and timely dynamic updates. Flag light leakage, ghosting, noise, and shadow instability for review. Numeric image diffs can catch regressions on one known GPU; cross-GPU pixels need tolerances and human review.

Provisional target: client p95 frame <=16.7 ms at 1080p after warmup, with GPU p95 <=12 ms and CPU frame work p95 <=4 ms. CPU/GPU overlap; these are not additive proof of the total. Test lighting-only and combined collapse scenes. If indirect lighting cannot fit, record a revised quality/performance decision instead of quietly removing it.

### G3 — persistence, late join, and streaming

Use a 256 x 128 x 256 m bounded world with resident cache limits low enough to force eviction. Drive a collapse while a third client joins, then reconnect that client. Traverse away/back, save, and restart. Test crashes at every persistence transaction boundary plus truncated/corrupt data and disk-full injection.

Required: post-recovery topology equals the declared durable prefix, IDs remain unique, no partial ownership transactions, and no mined-terrain regrowth. Normal checkpoint remains asynchronous; measure its retained snapshot memory. Record expected crash loss from the unflushed suffix separately from corruption.

Join target: dependency-complete near-player baseline <=16 MiB compressed, ready within 30 seconds on an imposed 1 MiB/s transfer budget with 100 ms RTT and 2% packet loss. The general transfer bound is larger, but exceeding this workload target fails the normal-join gate. Retry/catch-up stress must terminate with either successful join or a bounded explicit failure while connected clients continue.

### G4 — eight-client engine slice

Run for two measured minutes after 30 seconds warmup; also run a 30-minute reduced-telemetry soak. Eight players in both clustered and separated arrangements, 256 active nontrivial voxel bodies server-wide, at least 64 nearby to one observer, and an accumulated population of 4,096 sleeping persistent bodies. Drive 10 ordinary edits/s total and one 4 m diameter blast every 10 seconds. Include one 64-brick connected collapse. Geometry fixtures must specify occupied cells and collider complexity, not only body count.

Targets on the recorded reference hardware:

| Resource | Provisional target |
| --- | --- |
| Server simulation | 60 Hz; total tick p95 <=12 ms, p99 <=16.7 ms; physics p95 <=6 ms |
| Client | G2 frame targets in the standard scene; separately report destruction spikes and p99 |
| Server resident memory | <=8 GiB including physics, jobs, persistence staging, and union of player interests |
| Client resident memory | <=4 GiB excluding separately reported driver allocations |
| GPU resources | <=4 GiB tracked renderer allocations; record driver-reported usage where possible |
| Steady network | <=256 KiB/s server egress per client; actual transport overhead reported |
| Baselines | Separate capped 1 MiB/s/client budget; no unbounded concurrent transfers |
| Reliable topology backlog | Returns to normal within 5 s after named blast; capped bytes/age at all times |
| Impaired connection | 100 ms RTT, +/-20 ms jitter, 2% transport packet loss; topology converges |
| Stress connection | 200 ms RTT, 5% loss; bounded degradation/recovery, no corruption or runaway memory |

A second burst test exceeds ordinary load to verify explicit admission/backpressure. It need not maintain the same throughput but must not corrupt state, delete solid matter, leak memory, or make every subsequent join impossible. Publish target misses and the bottleneck; never rewrite fixture values to conceal failure.

Material strength, contact damage, local player prediction, reliable repairs, full save/recovery, and destructible terrain/body parity are functional requirements of this gate. GPU and server gates may run on separate machines; localhost eight-client runs are correctness evidence, not a substitute for realistic network/performance measurement.

### G5 — larger world

Define actual radius, height, concurrent active regions, topology metadata size, and persistent debris envelope from G4 measurements. Demonstrate multiple physics origins with widely separated players and approach/merge tests. Measure generation, streaming/LOD seams, far graph traversal, and long-session storage growth. Do not publish an infinite-world claim or a maximum player count based on extrapolation alone.

## Completion and review

A task passes when its own acceptance evidence exists and its integration dependencies still work. A gate passes only when all its required behaviors pass on the documented workload. Skipped GPU tests, untested crash paths, placeholder transport, and unsolved giant-collapse limits must remain visible as unfinished work.

G1/G2 are architecture decision points. Strong review is needed for structural graph correctness, collision representation, replication atomicity, and temporal lighting. Smaller agents can implement frozen interfaces and small fixtures effectively; they should not decide these cross-system tradeoffs independently.

### T09 review regression coverage

`cargo test -p spall_net` includes `separate_process_transport`: one OS server process, two OS client processes, and two OS UDP proxy processes, with packet loss and forwarding delay. Each client checks reliable replies, bulk parts and motion datagrams. Every child is supervised under a 30-second whole-run deadline and killed/reaped on failure. The ignored `process_role` test is its child entry point, invoked by the parent; it is not an omitted scenario. `cargo xtask net-check` remains the faster in-process measurement command and is labelled accordingly.

The transport regressions also exercise constructor validation through postcard, 1 MiB bulk payloads, negotiated limits, QUIC establishment timeouts, decoded-message loss/reorder, duplicate/overflow sequences, bounded bulk part metadata, liveness-owner shutdown, and delayed-proxy cancellation. Application-byte metrics use connection counters (control/datagram/bulk frame bytes observed at the sampling point, excluding QUIC overhead and authentication), not message counts. Wire-byte metrics come from Quinn. Neither harness is a destruction/replication/G1 feasibility result.
