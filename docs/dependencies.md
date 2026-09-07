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
