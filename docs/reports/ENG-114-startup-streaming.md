# ENG-114 / T17: authoritative startup and larger regional workloads

This continuation profiles the remaining startup work after ENG-130 regional
baselines. The full seed-1 showcase world still has 196,608 terrain bricks,
2,366,450 water-domain cells and eight cave mouths. Server geometry, ownership,
global structural analysis at edit time, revisions and canonical hashes are exact.
No generator version, terrain/water workload, durable schema or budget changed.

## Diagnosis and changes

`tools/measure-regional-startup.ps1 -ProfileStartup` now records generation,
authoritative simulation setup, baseline capture/catalogue/encoding, and client
wait/receive/install phases. Timings are diagnostic sidecars, never saved or
replicated state. Setup profiler spans are drained before tick measurements.

The initial catalogue was already inexpensive after warming: approximately
47 ms of a 79 ms snapshot, followed by 305 ms encoding. The dominant blue-screen
work was collision construction (11.79 s), global support graph warming
(15.12 s) and serial brick hash warming (1.23 s). The global startup graph was
discarded; later edits rebuild and validate it through the normal analysis path.

Startup now warms exact local labels, counts and hashes with at most eight scoped
workers, one queued 64-brick batch per worker. Only the owning caller validates
revisions and publishes labels. Warming takes 5.01 s in the final observation.
The regression compares cold/warm global graphs, dependency records and old COW
snapshots after edits and eviction, with one, two and eight workers.

Generation packing retained all raw source snapshots to preserve Windows heap
allocation ordering. Freeing those arrays consumed 9.62 s in a detailed probe.
After every packed replacement is installed, bounded workers now release source
batches and join before generation returns. Final release took 6.64 s. Packing
itself stays on the owner. Determinism, golden digests, water and cave tests pass.

A proposed all-solid collision fast path passed parity tests but did not show a
material improvement (11.79 to 11.62 s, a single noisy observation). It was
removed. Collision startup remains the largest authoritative setup cost.

## Measured startup

Windows release build, 4096 cells / 1024 m edge, one headless regional client,
300 server ticks, seed 1. These are single local observations, not statistical
benchmarks or GPU presentation measurements.

| Metric | Before | Final |
| --- | ---: | ---: |
| Launcher start to client launch | 17,543 ms | 15,867 ms |
| Client connect to acknowledged world readiness | 29,692 ms | 18,612 ms |
| Authoritative setup | 28,792 ms | 17,647 ms |
| Collision/world setup | 11,793 ms | 11,995 ms |
| Structural warming plus hashes | 16,346 ms | 5,008 ms |
| Baseline capture, including catalogue | 79 ms | 82 ms |
| Catalogue hash iteration | 47 ms | 50 ms |
| Baseline encoding | 305 ms | 327 ms |
| Client receive/verify | 221 ms | 250 ms |
| Client install | 118 us | 139 us |

Observed readiness fell 37.3%; launch-to-readiness fell about 27%. Transfer stays
4,932,870 bytes, with 5,292 resident terrain bricks and 191,316 distant digests.
Final sampled process peaks: client 146,898,944 bytes (140.1 MiB), server
6,471,225,344 bytes (6.03 GiB). All before/final probes exited zero and agreed
within each run on hash
`60ac1dfc89dfb9eb76c8f4024365d4e8dd957c0ccc9e046cc48bed828ecd3199`.

Evidence directories: `.local/runs/eng114-startup-phases-before/`,
`eng114-startup-warming-after/`, `eng114-startup-collision-after/` (removed
candidate), and `eng114-startup-release-after/`. The collision-candidate probe
overlapped a CPU harness test briefly and is only diagnostic. Detailed generation
times in the final probe: columns 0.485 s, fill 7.283 s, packing/release 7.542 s
(compact 0.826 s, insertion 0.054 s, release 6.640 s), water/spawns 0.030 s.

## Larger streaming validation

New separate fixtures `eng114-regional-1024m-stress` and
`eng114-regional-1024m-impaired` retain the full authoritative world, two clients,
7,200 ticks, eight distant radius-8 foundation cuts and resets at ticks 4,500 and
6,000. They request regional replicas and add outbound/return player movement,
requiring actual reloads, evictions and evicted-transaction gaps. The impaired
client uses production CUBIC, an 8 MiB/s ceiling, 200 ms RTT, 20 ms jitter and
5% loss. Its original 16 MiB / 30 s join bounds are unchanged. These fixtures do
not represent eight players, whole-foundation collapse or soak acceptance.

An initial run used an obsolete xtask binary and requested full baselines; it was
stopped and excluded. The first correctly built regional run exposed a real
`ResidentEvictedConflict` during bulk source patch result-hash validation. Such
patches can restore bricks beyond `before`; the candidate geometry was hashed
with live, superseded digests. Digest removal is now staged with candidate geometry
before hash validation and committed atomically. Regression coverage exercises
inline/bulk patches, rejected-hash isolation and duplicate replay. The failed run
is retained at `.local/runs/eng114-regional-stress-valid/`.

The corrected clean run at `.local/runs/eng114-regional-stress-fixed/` failed.
Both clients initially installed the regional catalogue. Client 0 completed 1,753
reloads and 3,774 evictions and travelled 116.7 m from its start, but stopped at
observed tick 2,502 with no reset installed. Client 1 reached tick 7,200, rejected
26 later transactions and diverged. The server committed 27 transactions and six
scripted requests, versus eight required. Single-brick commit p95 was 2,340.55 ms
on six samples; the fixture does not configure latency acceptance targets. These
numbers are failures and diagnostics, not proof of commit-latency acceptance.

The impaired run at `.local/runs/eng114-regional-impaired/` also failed. Client 0
completed 1,191 reloads / 2,846 evictions, then stopped at observed tick 2,277.
It applied one transaction with zero rejections, but no reset. Client 1 installed
4,932,314 bytes and confirmed readiness in 181,775 ms: below the unchanged 16 MiB
byte ceiling, above the unchanged 30,000 ms time ceiling. It installed no follow-on
transactions or resets before server shutdown and did not converge. The server
committed 25 transactions / four scripted requests; single-brick commit p95 was
2,223.54 ms on four samples. Peak process memory was 6,471,831,552 bytes. No
evicted-transaction gap assertion passed in either run. A client process reporting
`passed` only means its local progress condition passed; the harness correctly
fails requested actions, resets and final hash agreement.

These runs validate that full-size traversal does fetch/evict real geometry, and
that the source-patch panic is fixed, but do **not** validate the complete larger
streaming workload. Reliable control-read failures and transaction rejection
reasons now have structured warning logs for the next diagnostic pass. The precise
cause of the early disconnect remains unresolved; inferred causes must not be
reported as measured. The impaired transfer/queue pacing problem remains even
with the smaller regional baseline. Do not increase budgets or remove scripted
cuts/resets to relabel these scenarios as passes.

A final clean diagnostic repeat at `.local/runs/eng114-regional-stress-diagnostic/`
also failed: client 0 stopped at observed tick 2,328 (1,675 reloads / 3,359
evictions, initial readiness 17,750 ms); client 1 reached 7,200 ticks but rejected
26 transactions with structured reason `result hash mismatch for volume VolumeId(1)`.
It installed no resets and did not match the server. No control-read error was
logged, so the early exit must not be attributed to a measured decode failure.
Peer-Bye reasons now also log at info level; use `spall_client=info` next time.

## Checks

- `cargo test -p spall_voxel -p spall_structure -p spall_worldgen -p spall_sim -p spall_server -p spall_client --lib`: 369 passed, seven ignored before the additional patch regression.
- `cargo test -p spall_worldgen --tests`: four unit and 15 integration tests passed; one large capacity test ignored. Determinism and golden generation version digest passed.
- `cargo test -p xtask --bin xtask`: 36 passed, one ignored.
- Final client patch regression exercises inline and bulk paths.
- `cargo test -p spall_client --lib --test oversized_split`: 95 unit tests and two split integration tests passed.
- `cargo test -p spall_client --test regional_staging -p spall_server --test regional_baseline`: two staging tests and five regional tests passed, including real QUIC movement/distant-edit/reset convergence on the small fixture.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo clippy -p spall_structure -p spall_worldgen -p spall_sim -p spall_server -p spall_client -p sandbox -p xtask --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments`: passed. The two allowances cover existing code; this is not an unqualified workspace lint gate.

ENG-114 remains in progress. Remaining isolated startup costs are collision
construction, exact local labels/hashes, terrain fill and raw source release.
Global structural analysis and integrated commit latency, impaired reset behavior,
full G3/G4 workloads, largest-world GPU behavior and long-session memory still
require their own evidence. Next unblocked assignment remains ENG-114 / T17:
diagnose the full-size reliable-read disconnect, later transaction rejection and
impaired baseline/catch-up pacing, then repeat these unchanged fixtures. Collision
construction and integrated global analysis remain separate performance costs.
No relaxed acceptance limit is reported as a pass. GPU and soak gates were unrun
in this continuation.

Changed files: `spall_structure/src/label.rs`, `spall_sim/src/{schedule,sim}.rs`,
`spall_worldgen/src/{generate,lib}.rs`, `spall_server/src/{baseline,serve}.rs`,
`spall_client/src/{net,replica}.rs` (under `crates/`),
`examples/sandbox/src/worldgen_scene.rs`, `tools/measure-regional-startup.ps1`,
`tools/xtask/src/session.rs`, the two new regional scenario fixtures, and
`docs/{tasks,validation}.md` plus this report. The portable package is rebuilt
from final source; its four executable hashes are checked against release outputs.
