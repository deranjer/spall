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

## T05 — visible voxel baseline (verified 2026-09-07)

Two new crates. `spall_mesh` (CPU surface generation) adds **no new external
dependency**: it depends on `spall_voxel`, `spall_jobs` (the `JobToken` /
`WorldView` / `Staleness` used to invalidate a mesh when a halo brick arrives),
`glam` (vertex/winding math), `blake3` (mesh digest) and `thiserror` — all
already locked. `Cargo.lock` gains only the `spall_mesh` node.

`spall_render` (wgpu resources, one opaque pipeline, offscreen capture) matches
the `docs/architecture.md` edge `spall_render -> spall_mesh, spall_core`. It
reuses `wgpu` / `glam` / `pollster` (already locked for T00's client) and adds:

| Direct dependency | Locked version | Enabled feature/configuration | Registry license string | Exercised by T05 |
| --- | ---: | --- | --- | --- |
| bytemuck | 1.25.2 | `derive`; default features | `MIT OR Apache-2.0 OR Zlib` | zero-copy vertex / uniform / palette upload |
| image | 0.25.10 | `default-features = false`, `png` only | `MIT OR Apache-2.0` | PNG encode/decode for capture output and tests |

`pollster` is promoted from a `spall_client`-local dependency to a workspace
dependency (same locked `0.4.0`) and now also gates `spall_render`'s headless
`request_adapter` / `request_device`. `wgpu 24.0.5`'s deprecated
`Mat4::look_to_rh` / `Mat4::perspective_rh` (glam) and `ImageCopy*` (wgpu) names
are avoided: the renderer uses `glam::camera::rh::{view::look_to_mat4,
proj::directx::perspective}` and `wgpu::TexelCopy{Texture,Buffer}Info` /
`TexelCopyBufferLayout`.

`spall_render` is only reachable through the `sandbox/client` feature (the new
`sandbox-capture` binary, built by `cargo xtask capture`). Building
`sandbox-server` still leaves the server package GPU/window-free. `tools/xtask`
gains no dependency: it shells out to the built `sandbox-capture` binary the
same way it launches `sandbox-server` / `sandbox-client`.

Transitive crates newly locked by `image` with only the `png` feature:
`png 0.18.1`, `fdeflate 0.3.7`, `byteorder-lite 0.1.0`, `moxcms 0.8.1`,
`pxfm 0.1.30` (`miniz_oxide` is already present via wgpu) — all
`MIT`/`MIT OR Apache-2.0`/`Zlib`.

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
