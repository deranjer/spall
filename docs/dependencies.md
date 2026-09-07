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
