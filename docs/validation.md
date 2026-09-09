# Validation and operating contract

These commands and fixtures are **planned interfaces**. T00 implements
`cargo xtask check`, the bounded GPU-free portion of `cargo xtask smoke`, and
the offline clear-window capability smoke (`cargo xtask smoke --graphical`).
T09 adds `cargo xtask net-check`: an in-process QUIC transport harness (one
server, N authenticated headless clients, an optional opaque UDP loss proxy,
every channel exercised, bounded teardown) that writes `summary.json`,
`net.jsonl`, and `metrics.json`.
T05 implements `cargo xtask capture`: offscreen greedy-meshed renders of the
acceptance shapes (cube, tunnel, checkerboard, negative coordinates, adjacent
bricks, rotated hollow volume). T12 extends each capture to shaded, albedo,
normal, linear-depth, shadow-cascade, and roughness PNGs using four sun-shadow
cascades, a linear-HDR opaque pass, and fixed-exposure tone mapping. T13 adds
`cargo xtask capture --scene colored-room`: open/closed colored-room captures
with shaded and indirect-only PNGs, a fixed 128-cubed lighting cache, separate
upload/trace/denoise timings, and deterministic closed/open and thin-wall
probes.

T14 adds incremental cache updates. `LightingUpdate` is a plain
(non-simulation) description of what changed — world-space AABBs cleared to air
plus solid regions refilled in order; `LightingVolume::apply_update` applies one
and records a cell dirty only when its material actually changes, so a moving
body vacates old cells and fills new ones with no separate clear step erasing
overlapping geometry. `capture_lighting_sequence` renders a base
`IndirectOnly` frame, then applies a list of updates, re-uploading only the
dirty cells (`take_dirty`) and re-tracing only the dirty cell-AABB grown by a
halo (`set_trace_region`); the denoise still runs full. Its `SequenceReport`
records per step: `dirty_cells`, `retraced_cells`, `gpu_trace_millis` /
`gpu_denoise_millis`, and the measured `band_luminance`. The
`rapid_destruction` fixture (colored emitter scene, one represented 0.5 m
occluder column removed) is exercised by
`rapid_destruction_reexposes_the_band_with_a_bounded_retrace` in
`spall_render/tests/capture_gpu.rs`: on the reference adapter the shadowed band
brightens ~22.8 -> ~26.3 sRGB luminance in the next frame while the re-trace
touches ~1.3 % of the cache (~1.43 ms -> ~0.11 ms).

`LightingUpdate::moving_box` builds the update for a box body moving between two
world AABBs: both boxes are dirty, the caller's static regions are re-asserted,
then the body is filled at its new box — so a moving body never erases geometry
that overlaps its old bounds. The `moving_body_overlap` fixture puts a
cache-only occluder in an emitter -> receiver path, then moves it out and back;
`moving_body_leaves_no_ghost_and_keeps_swept_geometry` checks the shadowed band
recovers when the body leaves (22.8 -> 28.5), the shadow re-forms when it
returns (back to 22.8, bit-identical to the base frame), and each move
re-traces only ~3 % of the cache.

Temporal reprojection, moving-camera light-trail evidence, and
edit-commit-to-lighting wall-clock latency remain later T14 increments;
cross-GPU and p95 frame cost remain T15.
Each run also writes `summary.json`; it exits 3 when no GPU adapter is available.
The `summary.json` is schema `version: 4`. CPU and GPU costs are reported
separately and must not be conflated: `gpu_render_millis` is a real device
measurement from render-pass timestamp queries and is `null` (with
`gpu_timing_available: false`) on adapters that do not support them — it is
never a CPU-derived figure. T12 also reports `gpu_shadow_millis`,
`gpu_opaque_millis`, and `gpu_tone_map_millis`; T13 adds
`gpu_indirect_trace_millis`, `gpu_indirect_denoise_millis`, and
`cpu_lighting_upload_millis`. `cpu_capture_millis` (whole render → readback →
PNG-encode loop), `cpu_readback_millis`, and `cpu_encode_millis` are CPU
wall-clock and include the synchronous readback map wait and PNG compression.
The pre-`version: 2` `gpu_millis` field measured that CPU loop, not the GPU, and
must not be read as a GPU timing.
T06 adds one offline measurement binary,
`cargo run --release -p spall_physics --bin collision-bench`, which runs the
voxel-collision feasibility scenarios and writes `collision-feasibility.json`;
it is not wired into `cargo xtask bench`.
T08 adds one offline authoritative-edit binary,
`cargo run -p spall_sim --features scenario --bin sim-scenario`, which drives the
in-process `Simulation` through the terrain-split, rotated-moving-body cut,
conflict-convergence, and no-second-impulse scenarios and writes `summary.json`.
It runs the authoritative edit/split/collider-swap path only.
T10 wires `spall_net` into the hosts and adds the multi-process replication
harness: `sandbox-server --serve` binds `--listen` and runs the authoritative
`spall_sim::Simulation` behind QUIC; `sandbox-client --connect` is a headless
replica that applies committed `TopologyTransaction`s and scripts cuts. `cargo
xtask session --scenario <file.json>` and `cargo xtask scenario --name <builtin>`
(built-ins in `fixtures/scenarios/`) launch one server and N client **OS
processes** over real QUIC — optionally behind per-client UDP proxies with
`--loss-percent` — run the scenario's scripted cuts, and pass only if the server
and every client agree on the final canonical topology hash. They write
`summary.json` plus per-process `*.summary.json` / `*.jsonl`.
T16 adds durable persistence: `crates/spall_store` (the versioned SQLite save
schema and single WAL writer) and `spall_server::persist` (the `SimWorld` ⇄
save-record conversion and recovery). `sandbox-server --serve --save` recovers
from `<world>/world.db` on start, journals every committed transaction, and
checkpoints on `--checkpoint-interval-ticks` and clean shutdown. `cargo xtask
crash-test --suite persistence` runs the in-process crash-point / disk-fault
matrix through a real `Simulation` (bridge scene → column cut → beam detaches),
including a genuine SQLite engine write failure, and writes `summary.json` with
the measured bytes/write rate. Recovery after an **abrupt, unclean process
kill** at the journal / checkpoint publication boundaries is a separate
child-process harness: `cargo test -p spall_store --test abrupt_crash` (it kills
real child processes and reopens from a fresh process). `summary.json`'s
`unrun_here` field names what the in-process suite deliberately does not cover.
Durable writes go through `spall_server::persist_pipeline::PersistPipeline`: a
single off-thread `Writer` fed a **bounded** queue of immutable
snapshots/records, so a disk stall never stalls physics; a full backlog or a
failed write stops the run rather than continuing an unsavable world. The
integrator journals periodic 20 Hz body pose batches on a sequence contiguous
with the topology transactions, so a crash rewinds motion only to the latest
durable pose batch. `cargo test -p spall_server --test persist_pipeline`
reports retained-snapshot memory (max queue depth) and flush / checkpoint
latency.
T17 adds live late join: `sandbox-client --connect --late-join` pulls a
dependency-complete `BaselineWorld` over a bulk transfer instead of installing
the fixed scene, drains the server's bounded catch-up queue, and reaches the
authoritative topology hash with no edit replay. A scenario file may list
`late_join_clients` (and `late_join_connect_delay_ms`); those clients connect
after the tick loop is running and are excluded from `--min-clients`. Built-in
`late-join-collapse` runs one server + two early cutters + one late-join
replica. Mid-session brick `RepairRequest`s are answered with a one-brick
authoritative baseline patch (exact revision parity), and a reconnected session
generation invalidates the prior one server-side.
T19 adds headless player movement: `sandbox-server --serve --scene walk` runs the
flat `walk_arena` and gives each connecting client an authoritative capsule;
`sandbox-client --move FROM:TO:MX,MY,MZ:BUTTONS` (repeatable) scripts a movement
path, predicts the capsule locally with the shared `step_character` kernel, sends
`InputFrame` datagrams, and reconciles against the server's player snapshots. A
scenario file may set `scene` and list `player_paths` (per-client legs) plus a
`movement` acceptance block (`max_correction_m`, `min_distance_m`,
`min_ground_contact_ratio`, `expect_no_hover`). Built-in `player-movement` runs
one `walk` server + two scripted movers plus one floor cut; each mover's
`ClientSummary.movement` must show bounded corrections, sustained ground contact,
the scripted travel distance, no hover after the cut, and a clean held-input
release. It passes headless and with `--loss-percent 2`. The deep
prediction/reconciliation acceptance is CPU tests: `cargo test -p spall_physics
character::`, `-p spall_sim --test player_movement`, and `-p spall_client --test
prediction` (predictor vs. a live `spall_sim::Simulation` through an injected
100 ms link — convergence, floor-removal-no-hover, lost-button-release).
Interactive window input and full moving-body crush outcomes remain unrun /
follow-up.
`bench` still returns an explicit unavailable-capability result until its listed
task is delivered. All numerical limits are provisional acceptance targets. None
is a measured result.

## Agent operation without an editor

Use Cargo aliases so `cargo xtask` runs the xtask package. The orchestrator launches built binaries directly after one build, avoiding multiple concurrent Cargo builds and target-directory lock contention.

```powershell
# Build, lint, and CPU checks. Implemented first in T00.
cargo xtask check
cargo xtask smoke --ticks 60

# T09: QUIC transport + fault harness, in-process, no GPU. Real loopback QUIC
# through an opaque UDP loss proxy; reliable records must survive the loss.
cargo xtask net-check --clients 2 --loss-percent 2 --output .local/runs/net

# T10: authoritative replication host. Binds --listen, runs spall_sim behind
# QUIC, writes its cert fingerprint and a run summary. --paced for 60 Hz real
# time so scripted clients can interact. (T00 bounded loop: drop --serve.)
cargo run -p sandbox --bin sandbox-server -- --serve --listen 127.0.0.1:0 --ticks 300 --paced \
  --join-token-file .local/session/join.token --fingerprint-out .local/session/server.fingerprint \
  --addr-out .local/session/server.addr --summary-json .local/runs/server.summary.json --log-json .local/runs/server.jsonl

# T10: headless replication client. Applies committed transactions to a replica
# and scripts cuts as `--cut TICK:X,Y,Z:RADIUS`.
cargo run -p sandbox --features client --bin sandbox-client -- --connect 127.0.0.1:5000 \
  --server-fingerprint .local/session/server.fingerprint --join-token-file .local/session/join.token \
  --cut 4:10,4,1:2 --summary-json .local/runs/client.summary.json

# One server and two client OS processes over real QUIC; collects outputs and
# terminates its own processes. Passes iff every replica agrees on the topology hash.
cargo xtask session --scenario fixtures/scenarios/tower-cut.json --clients 2 --output .local/runs/tower-cut

# Named built-in scenario. --loss-percent runs each client behind a UDP proxy
# that drops/delays/reorders encrypted packets.
cargo xtask scenario --name destruction-network --loss-percent 5 --output .local/runs/network

# T17: two early cutters plus one --late-join replica that connects mid-collapse,
# pulls a baseline over a bulk transfer, and catches up to the server hash.
cargo xtask scenario --name late-join-collapse --output .local/runs/late-join

# T11 CPU-side G1 gate fixture on the cross-brick bridge scene. The full
# graphical capture portion remains unavailable until T05/T12. Two real OS
# clients over QUIC: client 0 cuts the seam-straddling column to detach the
# beam then cuts the falling beam again (a body-targeted cut), both clients
# excavate opposite floor ends concurrently. Passes only if all ten scripted
# cuts commit, the server and both clients agree on one hash, the detached body
# spans >= 2 bricks (cross-brick ownership transfer), each live client sees the
# beam move >= 0.3 m, the body cut lands, and the committed topology-event
# stream replayed from the tick-0 baseline reproduces the hash. Add
# --loss-percent 2 for the impaired-transport run, which additionally requires
# the clients to have observed motion datagrams delivered out of order. See
# docs/reports/G1.md.
cargo xtask scenario --name g1-networked-destruction --loss-percent 0 --output .local/runs/g1

# T19: one `walk` server + two scripted player capsules that predict movement,
# send InputFrame datagrams, and reconcile against the server's player snapshots.
# Passes on bounded corrections, ground contact, travel distance, and no hover.
cargo xtask scenario --name player-movement --loss-percent 0 --output .local/runs/player-movement

# T12: stable acceptance cameras with six views and per-pass GPU timing.
# Needs a supported GPU/driver; exit 3 otherwise.
cargo xtask capture --output .local/runs/t12-1080p --width 1920 --height 1080 --strategy greedy

# T13: static one-bounce colored-room feasibility capture. Moving 120-frame
# evidence remains T14/T15. See docs/lighting-decision.md.
cargo xtask capture --scene colored-room --width 1920 --height 1080 --output .local/runs/t13-lighting

# Release build, named fixture, fixed workload; emits machine-readable metrics.
cargo xtask bench --suite engine-slice --clients 8 --warmup-seconds 30 --duration-seconds 120 --output .local/runs/bench

# Defined fault points around journal/checkpoint publication.
cargo xtask crash-test --suite persistence --output .local/runs/crash

# T18 CPU acceptance: shared cache policy plus the bounded 256 x 128 x 256 m
# streamed fixture (structural reload, save-air, body crossing, collision gate,
# traversal plateau). The full G3 multi-process gate remains T23.
cargo test -p spall_voxel -p spall_client -p spall_server --all-features
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
| player-movement | A scripted capsule walks and stays grounded; a 100 ms link keeps corrections bounded; removing a floor cannot leave the player hovering; a lost button release stops it within 250 ms |

Use property tests for coordinates, storage/edit conservation, and codec bounds. Use tiny reference algorithms for topology/mesh coverage. Physics tests use position/energy tolerances where appropriate; exact hashes test topology, not floating-point trajectories. A rendered screenshot is not proof of collision or replication correctness.

## Gate workload and targets

Choose and record a reference Windows gaming PC and dedicated-server CPU in T00. Provisional envelope: desktop six-core-class CPU, 32 GiB system RAM, discrete GPU with 8 GiB VRAM. This is a test planning envelope, not a published minimum specification or purchase recommendation. Repeat backend smoke tests on D3D12 and Vulkan when available. Linux headless CI is desirable from the start; Windows is the initial client target.

### G0 — reproducibility

Clean checkout builds with the pinned toolchain. CPU checks need no display adapter. Bounded smoke runs are reliable in CI, including cancellation. Instructions identify actual Windows build prerequisites discovered in T00. Tooling cannot claim later commands work before their tasks land.

### G1 — real destruction and two clients

64 x 32 x 64 m world, 25 cm terrain cells. Include a 12 m hollow tower/bridge spanning brick boundaries, an excavatable slope, and a moving hollow test volume. Run 60 seconds at 60 server ticks/s; after initial cuts, drive 10 tool requests/s total across two clients. Include one cut affecting a 4 m diameter sphere and a 64-brick connected body stress case.

Required: exact ownership/conservation, zero unrepaired topology mismatch at quiescence, all accepted work eventually completes within the stated budget, and no permanent hidden support. Normal single-brick edit server commit target: p95 <=100 ms without network delay. Ordinary structure split target: <=500 ms; designated large-collapse stress target <=2 s before consistent activation. These are gates to measure, not guarantees from the chosen algorithms.

The tracked `g1-networked-destruction` fixture is the CPU-side T11 evidence
surface. It runs on the cross-brick `cross_brick_bridge_scene` (a seam-
straddling column holds a beam, both spanning the `x = 32` brick boundary) and
requires: every scripted cut to commit (so an early quiescence fails the run);
final server/client hash agreement; the detached body's cells to have been
owned across a brick boundary (`>= 2` distinct bricks — cross-brick structural
support propagation and brick-boundary ownership transfer); at least
`minimum_body_displacement_m` of real detached-body motion on every live
client; the body-targeted cut landing against the detached body; and the
committed topology-event stream, replayed deterministically from the tick-0
baseline via the durable journal, reproducing the live canonical hash. The
`--loss-percent 2` variant additionally asserts the clients observed motion
datagrams delivered out of `snapshot_seq` order. It does **not** yet cover a
detached body coming to rest on remaining structure (the beam free-falls —
tracked separately), nor the full 64 x 32 x 64 m / 60-second /
10-requests-per-second workload, 64-brick collapse stress case, graphical
captures, or GPU timings; those remain explicit follow-up evidence until the
renderer and gate workload are available. Measured results:
`docs/reports/G1.md`.

For overload tests, requests beyond admission capacity may return busy. The report must distinguish requested, rejected, queued, and committed counts. Rejecting every expensive action does not satisfy the gate: all named mandatory edits must complete.

### G2 — graphics quality and cost

1920 x 1080, fixed exposure, recorded sun/camera/material settings. Test daylight terrain, a colored enclosed room, an emissive source behind an occluder, thin walls, moving debris, and rapid destruction. Capture both a settled frame and at least 120 consecutive moving frames.

Required visual behavior: correct silhouettes/materials, soft/contact shadows, visible diffuse indirect illumination, emissive lighting, dark enclosed areas, and timely dynamic updates. Flag light leakage, ghosting, noise, and shadow instability for review. Numeric image diffs can catch regressions on one known GPU; cross-GPU pixels need tolerances and human review.

Provisional target: client p95 frame <=16.7 ms at 1080p after warmup, with GPU p95 <=12 ms and CPU frame work p95 <=4 ms. CPU/GPU overlap; these are not additive proof of the total. Test lighting-only and combined collapse scenes. If indirect lighting cannot fit, record a revised quality/performance decision instead of quietly removing it.

### G3 — persistence, late join, and streaming

Use a 256 x 128 x 256 m bounded world with resident cache limits low enough to force eviction. Drive a collapse while a third client joins, then reconnect that client. Traverse away/back, save, and restart. Test crashes at every persistence transaction boundary plus truncated/corrupt data and disk-full injection.

Required: post-recovery topology equals the declared durable prefix, IDs remain unique, no partial ownership transactions, and no mined-terrain regrowth. Normal checkpoint remains asynchronous; measure its retained snapshot memory. Record expected crash loss from the unflushed suffix separately from corruption.

Join target: dependency-complete near-player baseline <=16 MiB compressed, ready within 30 seconds on an imposed 1 MiB/s transfer budget with 100 ms RTT and 2% packet loss. The general transfer bound is larger, but exceeding this workload target fails the normal-join gate. Retry/catch-up stress must terminate with either successful join or a bounded explicit failure while connected clients continue.

T18's CPU acceptance uses the bounded world dimensions above with deliberately
small cache ceilings. `spall_server/tests/residency.rs` verifies that a remote
anchor dependency is reloaded before its beam is classified and then releases
after the anchor cut; a modified-air brick is persisted before eviction and
reloads without regrowth; one rotated/moving body remains one identity across
partition references; a fast swept entry is blocked until all collision bricks
are ready; and a 20-brick traversal never exceeds its three-brick fixture
budget. `spall_client/tests/residency.rs` applies the same policy to replica
terrain while retaining complete body geometry. These are correctness and
bounded-accounting results, not the G3 memory, network, or join-duration gate;
those measurements remain for T23.

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
