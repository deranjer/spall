# Dependency record

`Cargo.lock` is committed and was generated with Rust 1.96.1 on Windows
(`x86_64-pc-windows-msvc`). The project itself has no declared license yet.
The versions and license strings below were verified from the locked registry
manifests. Cargo may select compatible patch releases only by updating
`Cargo.lock`; this record names the releases actually locked now.

## T00 — build and process harness (verified 2026-09-06)

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T00 |
| --- | ---: | --- | --- | --- |
| clap | 4.6.6 | `derive`; default features | `MIT OR Apache-2.0` | sandbox and xtask CLI parsing |
| ctrlc | 3.5.2 | default features | `MIT/Apache-2.0` | xtask cancellation cleanup |
| pollster | 0.4.0 | default features | `Apache-2.0/MIT` | synchronous adapter/device startup |
| serde | 1.0.229 | `derive`; default features | `MIT OR Apache-2.0` | JSONL process records |
| serde_json | 1.0.151 | default features | `MIT OR Apache-2.0` | JSONL and smoke summaries |
| thiserror | 2.0.20 | default features | `MIT OR Apache-2.0` | typed host/harness errors |
| tracing | 0.1.44 | default features | `MIT` | sandbox process diagnostics |
| tracing-subscriber | 0.3.23 | `env-filter`; default features | `MIT` | `RUST_LOG` subscriber |
| wgpu | 24.0.5 | default features | `MIT OR Apache-2.0` | clear-only native surface render |
| winit | 0.30.13 | default features | `Apache-2.0` | native window, resize, close event loop |

Only the client package imports wgpu/winit. `sandbox-client` requires the
`sandbox/client` feature, so building `sandbox-server` leaves the server
package's active graph GPU/window-free. The server currently records the
requested listen address but deliberately does not bind it: QUIC transport,
authentication, and multiplayer start in T09.

## T01 — IDs, schemas, canonical encoding (verified 2026-09-06)

Added for `spall_protocol`. Both are pure-Rust and build GPU/window/network-free.

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T01 |
| --- | ---: | --- | --- | --- |
| blake3 | 1.8.7 | default features (`std`, portable SIMD; no `rayon`/C fallbacks) | `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception` | canonical topology + content-manifest hashing |
| postcard | 1.1.3 | `use-std` (+ default `heapless`) | `MIT OR Apache-2.0` | versioned DTO wire body encode/decode |

Transitive crates newly locked by these: `arrayvec 0.7.8`, `constant_time_eq
0.4.2`, `cpufeatures 0.3.1`, `cobs 0.3.0`, `hash32 0.2.1`, `heapless 0.7.17`,
`byteorder 1.5.0`, `spin 0.9.9`, `critical-section 1.2.0`, `embedded-io`,
`stable_deref_trait 1.2.1` (all `MIT`/`MIT OR Apache-2.0` family). `blake3`
pulls a `cc` build of its SIMD assembly; the reference host already has the
MSVC C++ tools recorded below.

`spall_protocol` contains only DTOs and pure codec/hash logic — no transport,
async, or simulation. `spall_net` (T09) will add Quinn on top of these
records; `spall_store` (T16) reuses the canonical little-endian encoding.

## T02 — brick storage and revisioned edits (verified 2026-09-06)

`spall_voxel` adds no new external dependency. It reuses `blake3` (already
locked for T01) for its representation-independent brick content hash and
`thiserror` for typed access/edit errors; it depends only on `spall_core`
among workspace crates, matching the `docs/architecture.md` dependency graph.
An optional `oracle` feature exposes the crate's dense reference model to
later foundation tasks (T03+) without copying it.

## T03 — fixture worlds, rays, and integer brushes (verified 2026-09-06)

`spall_voxel` gains one new external dependency for its ray/transform math.

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T03 |
| --- | ---: | --- | --- | --- |
| glam | 0.33.6 | `f64` (adds `DVec3` / `DQuat`); default `std` | `MIT OR Apache-2.0` | rigid volume↔world transforms and DDA ray math |

`glam` is the math library named in `README.md`; T03 is the first task that
needs real vector / quaternion rotation (world-space rays against a rigidly
posed volume). With the `f64` + default `std` features it is pure-Rust with
**no transitive dependencies**; no GPU, window, or async code enters
`spall_voxel`. Ray results are explicitly
*not* required to be bit-identical across machines — authoritative hit
validation stays server-side — so `f64` math here is fine. Integer brush
plans (`brush.rs`) use only `spall_core`'s fixed-point predicate and stay
fully deterministic.

## T04 — bounded job scheduling (verified 2026-09-06)

`spall_jobs` adds **no new external dependency**. It depends only on
`spall_core` (coordinate / id / revision types reused in job tokens) and
`thiserror` (already locked) for its `SubmitReason` / `CounterExhausted`
errors. `Cargo.lock` gains only the `spall_jobs` package node.

The scheduler is executor-agnostic and deterministic: it never spawns a
thread. `ThreadJobPool` is a thin wrapper that runs jobs on `std::thread`
workers — standard library only, no `rayon` yet. `README.md` names a Rayon
pool for jobs; T04 deliberately keeps the mechanism (bounded queues,
priority order, token re-validation, generation invalidation, clean
shutdown) independent of the worker backend, so a Rayon adapter can be
added later without reworking the policy. An optional `testkit` feature
exposes an in-memory `WorldView` double (`spall_jobs::testkit::MapWorld`)
for T05 / T07 result-validation tests. No GPU, window, network, async, or
filesystem code enters `spall_jobs`.

## T07 — support graph and split plans (verified 2026-09-07)

`spall_structure` adds **no new external dependency**. It depends on
`spall_voxel` (brick snapshots, `Volume`, `EditOutcome`), `spall_jobs`
(`JobToken` / `WorldView` / `Staleness` / `Generation` / `TopologyEpoch` for
result re-validation), and `thiserror` (already locked). `Cargo.lock` gains
only the `spall_structure` package node.

The `docs/architecture.md` dependency graph is refined from
`spall_structure -> spall_voxel` to `spall_structure -> spall_voxel, spall_jobs`.
Rationale: a completed structural analysis is an off-tick job result and must
be discarded when its input brick revisions move, so it carries the same
`spall_jobs::JobToken` mechanism every other background result uses rather than
a parallel one. Both are `-> spall_core` foundation crates; no cycle is
introduced. An optional `oracle` feature exposes the crate's dense flood-fill
reference model (mirroring `spall_voxel`'s `oracle` feature). Dev-dependencies
enable `spall_jobs/testkit` and `spall_voxel/oracle` for the scenario tests. No
GPU, window, network, async, or filesystem code enters `spall_structure`.
## T09 — transport and fault harness (verified 2026-09-07)

New crate `crates/spall_net` (the QUIC transport adapter). It is the only place
Quinn, rustls, and Tokio appear. `tools/xtask` gains `spall_net` + a minimal
`tokio` runtime for the `cargo xtask net-check` harness command. Dependency
direction matches `docs/architecture.md`: `spall_net -> spall_protocol`
(`-> spall_core`); no dependency on `spall_voxel`/`spall_sim`.

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T09 |
| --- | ---: | --- | --- | --- |
| quinn | 0.11.11 | `default-features = false`, `runtime-tokio`, `rustls-ring`, `log` | `Apache-2.0 OR MIT` | QUIC endpoints, reliable streams, datagrams |
| rustls | 0.23.44 | `default-features = false`, `ring`, `std`, `tls12` | `Apache-2.0 OR ISC OR MIT` | TLS 1.3 configs + custom fingerprint-pinning verifier |
| rcgen | 0.13.2 | `default-features = false`, `ring` | `MIT OR Apache-2.0` | self-signed development server certificate |
| tokio | 1.53.1 | `spall_net`: `rt`,`rt-multi-thread`,`net`,`time`,`sync`,`macros`,`io-util`; `xtask`: `rt-multi-thread`,`time`,`macros` | `MIT` | async transport/IO runtime only (no simulation) |

Transitive crates newly locked: `quinn-proto 0.11.17`, `quinn-udp 0.5.15`,
`rustls-webpki 0.103.15`, `rustls-pki-types 1.15.1`, `ring 0.17.14`,
`untrusted 0.9.0`, `socket2 0.6.5`, `mio 1.2.3`, `lru-slab`, `rustc-hash`,
`getrandom 0.2.17` + `wasi`, `rand`/`rand_core`/`rand_pcg` (rcgen),
`yasna 0.5.2` + `time 0.3.55` (`deranged`, `num-conv`, `powerfmt`,
`time-core`), `chacha20 0.10.2`, `subtle`, `zeroize`, `tinyvec`,
`tokio-macros 2.7.2` — all `MIT` / `Apache-2.0` / `ISC` family (`ring` is
`ISC AND MIT AND OpenSSL`). `ring` ships prebuilt MSVC assembly; no extra
Windows toolchain beyond the VS 2022 C++ tools already recorded below.

`spall_net` is transport only: no `zstd` yet (compression / bounded
decompression of brick payloads arrives with `spall_store` in T16). The T09
malformed-input tests bound the declared-length and assembled-transfer paths.

## T06 — editable voxel collision feasibility (verified 2026-09-07)

`spall_physics` adds the physics solver named in `README.md`. It is the only
crate that depends on `rapier3d`; Rapier/parry types never leave the
`spall_physics::world` and `::collider` modules (callers address bodies by an
opaque `BodyId`). Dependency direction matches `docs/architecture.md`:
`spall_physics -> spall_voxel` (`-> spall_core`); no `spall_jobs` /
`spall_structure` / GPU / window / network edge.

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T06 |
| --- | ---: | --- | --- | --- |
| rapier3d | 0.35.3 | default (`dim3` + `f32` + `std`) | `Apache-2.0` | rigid-body solver, compound + voxel colliders, CCD, contact manifolds |

`rapier3d 0.35` moved its public math to **glam** (via the `glamx` wrapper):
`Vector` = `glam::Vec3`, `Rotation` = `glam::Quat`, `Pose` = `glamx::Pose3`,
`IVector` = `glam::IVec3`. `PhysicsPipeline::step` takes a `&mut BroadPhaseBvh`
(aliased `DefaultBroadPhase`). This is the released `0.35.3` surface, not the
`master`-branch docs.

Transitive crates newly locked (all `Apache-2.0` or `MIT OR Apache-2.0`
family; `wide` is `Zlib OR Apache-2.0 OR MIT`): `parry3d 0.30.2`,
`nalgebra 0.35.0`, `nalgebra-macros`, `simba 0.10.2`, `glamx 0.3.0`,
`glam 0.30.10` / `0.31.1` / `0.32.1` (older majors pulled by parry/rapier/glamx,
coexisting with the workspace's `glam 0.33.6`), `approx`, `wide`, `safe_arch`,
`matrixmultiply`, `rawpointer`, `libm`, `typenum`, `num-complex`,
`num-rational`, `num-bigint`, `num-integer`, `num-derive`, `num-traits`,
`ordered-float`, `spade`, `rstar`, `robust`, `heapless` (already present via
postcard), `hash32`, `hashbrown`, `foldhash`, `allocator-api2`, `ena`,
`downcast-rs`, `either`, `profiling-procmacros`. `nalgebra`/`simba` compile a
`build.rs`; no C toolchain beyond the MSVC tools already recorded is needed.

`rapier3d` features **available but not enabled**: `enhanced-determinism`
(libm-forced math for cross-platform reproducibility — physics tests use
position/energy tolerances instead), `parallel` (rayon), `simd8`,
`serde-serialize`. A dev-dependency enables `spall_voxel/oracle` for the mass
cross-check. The `collision-bench` binary and the `#[cfg(test)]` feasibility
scenarios are the only consumers; no server loop drives physics yet (T08).

## T08 — authoritative edit and body transfer (verified 2026-09-07)

`spall_sim` adds **no new external dependency**. It depends on `spall_structure`
(support graph, split membership, conservation ledger), `spall_physics`
(collider builds, `PhysicsWorld`, analytic mass), `spall_jobs`
(`Scheduler` / `JobToken` / `WorldView` for bounded staging and commit-time
re-validation), `spall_protocol` (`TopologyTransaction` / `MotionSnapshot` DTOs
and the canonical hash), `spall_voxel`, `spall_core`, `glam` (already locked in
T03 — used for the child-velocity cross product and pose math), and `thiserror`.
`Cargo.lock` gains only the `spall_sim` package node.

The `docs/architecture.md` dependency graph is refined from
`spall_sim -> spall_structure, spall_physics` to add `-> spall_jobs, spall_protocol`
(both `-> spall_core` foundation crates; no cycle). Rationale: staging is a
bounded off-tick job re-validated through the same `JobToken` mechanism every
other derived result uses, and `spall_sim` owns the authoritative-state → wire
record conversion (`docs/architecture.md`).

An optional `scenario` feature enables `serde` / `serde_json` (already locked)
for the offline `sim-scenario` binary; the library and its tests do not pull
them. Dev-dependencies enable `spall_structure/oracle` (dense BFS support
reference for the conservation cross-check) and `spall_voxel/oracle`. No GPU,
window, network, async, or filesystem code enters `spall_sim` (the scenario
binary writes a `summary.json` under `.local/`, like `collision-bench`).

Two additive, non-breaking methods were added to `spall_physics` for the T08
integration (no contract or signature change to existing items):
`OccupancyGrid::from_solid_mask` (build a grid from a caller-supplied solid
mask — used by the coarse-fracture fallback) and
`PhysicsWorld::set_body_pose` / `set_body_velocity` (spawn a split child at its
parent's transform and inherited velocity).

## T10 — replica transactions and motion (verified 2026-09-07)

**No new external dependency.** The T10 crates only add workspace-internal path
edges and reuse already-locked crates:

- `spall_sim` gains a `replication` module (no new deps).
- `spall_client` adds path deps `spall_protocol`, `spall_net`, `spall_voxel`
  and enables `tokio` (`rt`, `rt-multi-thread`, `net`, `time`, `sync`, `macros`)
  + `serde` / `serde_json` for the headless replication client and its JSON
  summary. `spall_sim` is a **dev-dependency** only (the phase-B acceptance test
  drives a real authoritative `Simulation`; it is not a runtime edge in the
  `docs/architecture.md` graph). A `[dev-dependencies]` cycle
  `spall_client -> spall_sim -> ... -> spall_client`? No: `spall_sim` does not
  depend on `spall_client`.
- `spall_server` adds path deps `spall_protocol`, `spall_net`, `spall_sim`,
  `spall_voxel` and the same `tokio` feature set, plus `serde` / `serde_json` /
  `tracing`. `spall_client` is a **dev-dependency** for the in-process
  server↔client session test.
- `examples/sandbox` adds a non-optional `spall_net` path dep so
  `sandbox-server --serve` / `sandbox-client --connect` can read per-run
  credential files.
- `tools/xtask` adds no dep: `cargo xtask session` / `scenario` reuse the
  already-present `spall_net` (`UdpProxy`), `tokio`, and `serde_json`.

`Cargo.lock` gains only the new package nodes; every external version is
unchanged from T06 (`rapier3d` stack) and T09 (`quinn` / `rustls` / `tokio`
stack).

## T16 — durable world checkpoint and journal (verified 2026-09-07)

New crate `crates/spall_store` — the save schema and durable SQLite I/O. It is
the only crate that depends on `rusqlite` / `zstd`; dependency direction matches
`docs/architecture.md` (`spall_store -> spall_protocol -> spall_core`), with no
edge to `spall_voxel` / `spall_sim` / GPU / window / async.

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T16 |
| --- | ---: | --- | --- | --- |
| rusqlite | 0.37.0 | `bundled` (vendored SQLite 3, `libsqlite3-sys 0.35`) | `MIT` | WAL writer, checkpoint/journal transactions, `PRAGMA user_version` guard |
| zstd | 0.13.3 | default (`zstd-safe 7`, `zstd-sys 2.1.0+zstd.1.5.7`) | `MIT` | compressed dense brick payloads, bounded decompression |

Transitive crates newly locked: `libsqlite3-sys 0.35.0` (`MIT`), `zstd-safe
7.3.0` (`BSD-3-Clause`), `zstd-sys 2.1.0+zstd.1.5.7` (`BSD-3-Clause`; vendored
zstd C is `BSD-3-Clause OR GPL-2.0`), `hashlink 0.10.0` (`MIT OR Apache-2.0`),
`fallible-iterator 0.3.0` / `fallible-streaming-iterator 0.1.9` (`MIT/Apache-2.0`),
`getrandom 0.4.3` (`MIT OR Apache-2.0`), plus build-time `pkg-config`, `vcpkg`,
`jobserver`. `libsqlite3-sys` and `zstd-sys` compile vendored C with the MSVC C
toolchain already recorded below; no extra Windows prerequisite.

`rusqlite` features **available but not enabled**: `serde_json`, `chrono`,
`load_extension`, `backup` (versioned DTO migration into a separate database is
a later task; `docs/protocol.md`). `spall_store` sets `synchronous=FULL` and
verifies both pragmas on open. `blake3` (already locked in T01) provides the
16-byte journal-payload integrity check. `postcard` (T01) encodes the save DTOs;
the journal keeps `spall_protocol` wire records verbatim so the wire schema
stays versioned independently of the save schema.

## Verified Windows prerequisites

- Rust toolchain: `rustc 1.96.1 (31fca3adb 2026-06-26)`, Cargo 1.96.1,
  `stable-x86_64-pc-windows-msvc`.
- MSVC C++ tools: Visual Studio 2022 Community with `VC.Tools.x86.x64`.
- Reference development host discovered for future gate records: AMD Ryzen 7
  9700X (8 cores / 16 threads), 63.6 GiB physical RAM, NVIDIA RTX 4080 SUPER
  driver 32.0.16.1088.

T00's GPU capability smoke records the adapter/backend from wgpu in its
client JSONL log. It does not claim a renderer-quality or GPU-performance
gate; those begin in T05 and T12 respectively.
