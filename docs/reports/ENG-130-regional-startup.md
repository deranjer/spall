# ENG-130: regional terrain startup

Generated-world editor Run in game and `xtask play --worldgen` now request regional
terrain geometry by default. The initial box has radius 10 bricks (80 m on each axis
from the spawn brick). Distant bricks retain exact revision, canonical content hash,
solid count, and modified-air information. The client fetches nearby missing geometry
as the player moves, using the existing revision-validated repair path. Bodies stay
complete and server authority, durable formats, and logical topology hashes remain intact.

## Measured startup

Windows release build, seed 1 showcase, 4096 cells / 1024 m edge. These are local
headless observations, not performance targets or GPU presentation timings.

| Metric | Regional | Full geometry control |
| --- | ---: | ---: |
| Server generation/startup before client launch | 17,671 ms | 18,443 ms |
| Client connect to installed/acknowledged baseline | 31,017 ms | 48,583 ms |
| Initial compressed baseline | 4,932,870 bytes | 94,280,212 bytes |
| Initial terrain geometry | 5,292 bricks | 196,608 bricks |
| Initial distant terrain digests | 191,316 bricks | 0 |
| Sampled client peak working set | 147,111,936 bytes (140.3 MiB) | 2,327,334,912 bytes (2.17 GiB) |
| Sampled server peak working set | 6,471,208,960 bytes (6.03 GiB) | 6,471,237,632 bytes (6.03 GiB) |

The regional probe exited successfully and client/server final topology hashes agreed:
`60ac1dfc89dfb9eb76c8f4024365d4e8dd957c0ccc9e046cc48bed828ecd3199`.
Evidence: `.local/runs/eng130-regional-4096/`, with summaries, JSONL logs,
process stdout/stderr, and `startup.measurement.json`. Peak working sets are sampled
process peaks; they do not include GPU allocations or all machine memory.

The final full control also exited successfully with client/server hash equality:
`50ec528c8c502c55d8c7912bdc01d49e2454a76dd4523a3c8d7366c8fe2fa0b1`.
Evidence: `.local/runs/eng130-full-4096-final/`. It used 3000 server ticks versus
300 for the regional observation; the longer run includes one autonomous topology
transaction and 44 bodies, explaining why final hashes differ across the runs.
Within each run the client agrees exactly with its server. The initial terrain
catalogue still contains the same 196,608 bricks. Timings are single observations
on this host, not a statistically controlled throughput benchmark. Transfer fell
94.8%, sampled client peak fell 93.7%, and observed readiness fell 36.2%.

The first full control used 300 server ticks and ended before its background baseline
capture completed. The second used 9000 ticks: the client successfully installed
94,280,212 bytes / 196,608 geometry bricks in 51,063 ms, peaking at 2,327,547,904
bytes (2.17 GiB), but the server exceeded the probe's 240-second supervisor bound.
That run is a startup observation, not a passing convergence/lifecycle probe. The
script now exposes bounded `-ServerTicks` (default 3000) to permit a complete control
without extending the supervisor deadline. Evidence for both failed control lifetimes
is retained under `.local/runs/eng130-full-4096*`.

## Validation

Passed:

```text
cargo fmt --all -- --check
cargo test -p spall_protocol -p spall_net -p spall_sim -p spall_client -p spall_server --lib --tests
cargo clippy -p spall_protocol -p spall_net -p spall_sim -p spall_client -p spall_server -p sandbox -p xtask --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
git diff --check
& tools/package-portable.ps1
& tools/measure-regional-startup.ps1 -WorldSizeCells 4096 -ServerTicks 300 -OutputDirectory .local/runs/eng130-regional-4096
& tools/measure-regional-startup.ps1 -WorldSizeCells 4096 -FullBaseline -OutputDirectory .local/runs/eng130-full-4096-final
```

The CPU command passed all enabled tests, including exact topology/replay, persistence,
late join under destruction, delayed/lossy transport, and residency regressions. Existing
ignored hardware/large-workload tests remain unrun. The named lint allowances cover
pre-existing test initialization and `window.rs::finish_frame` warnings; a strict
unqualified workspace lint/check gate is not claimed.

New scenario coverage: five server regional tests and two client staging tests, plus
protocol schema/count/radius isolation and capsule-sweep collision coverage. The real
QUIC test starts with two resident and six digest terrain bricks, walks beyond the
initial geometry, reloads and evicts, applies a distant cut, returns, and installs a
world reset. It finishes at the server's exact hash with zero rejected transactions,
zero rejected actions and zero baseline-transfer failures. A first assertion requiring
an applied transaction was corrected: a post-edit authoritative repair may satisfy the
held transaction directly. The test now verifies the committed server edit, evicted
revision gap, applied repair and exact convergence. An offline test independently
verifies distant-edit convergence before reset.

Real packaged GPU smoke (exit 0):

```text
dist/spall-portable/xtask.exe play --portable --worldgen showcase --seed 1 --worldgen-size 1024 --ticks 18000 --uncapped --shots "walk;move:20,1,0;walk;reset;wait:10;walk" --shots-dir G:\Programming\voxel_engine\.local\runs\eng130-gpu\shots --output G:\Programming\voxel_engine\.local\runs\eng130-gpu
```

Five PNGs were captured, and the final post-reset image was visually inspected: terrain
and trees are presented. The initial baseline installed about 2.9 seconds after
client startup for this 256 m world. This is presentation/reset smoke evidence, not a
1024 m GPU, unrestricted traversal, or rendering-throughput acceptance gate. The launcher
terminated only its owned server when the scripted client closed, as normal for play.
Portable binaries were refreshed in `dist/spall-portable`; SHA256 comparisons of all
four packaged executables against `target/release` matched. An earlier package attempt
encountered our running probe's executable lock; it succeeded after probe cleanup.

## Implementation and remaining work

Protocol world version 4 / segment schema 3 carries regional digests. Older formats
retain geometry. Installation atomically replaces geometry and digest namespaces;
reset and retry preserve each client's capability and current authoritative position.
Regional baselines are not shared across players. New geometry arrives through existing
bounded reloads (at most four requests per pass, nearest first). Default client limits
are 16,384 resident terrain bricks and 1 GiB dense-cell bytes; the existing 8 GiB staging
ceiling is an admission bound, not preallocation. Prediction holds at unknown capsule
sweep geometry instead of treating it as air.

Changed files: `spall_protocol/{baseline,segment,records}.rs`, `spall_net/conn.rs`,
`spall_server/{baseline,serve}.rs`, `spall_client/{net,replica,residency,predict}.rs`,
`spall_sim/{replication,world}.rs`, the sandbox client CLI, xtask generated-world launch,
portable packaging instructions, regional integration tests, the bounded startup probe,
README, and architecture/protocol/validation/task documentation. ENG-129's editor
Uncapped changes remain included in this checkout and package.

The server still generates and owns the entire world. Snapshot/catalogue construction
and cold canonical hashes still visit every logical brick, leaving significant client
startup delay even after geometry transfer shrinks. Server-evicted geometry may still
be read from backing during capture. Distant edits can cause geometry repairs outside
interest, and global water/ecology replication is unchanged. Free-camera movement does
not drive terrain interest. No new eight-player, regional impaired-link, 1024 m GPU,
G3/G5 destruction, or long-session plateau gates were run here.

Next unblocked task: ENG-114/T17 continuation, profile and reduce remaining authoritative
generation, snapshot/hash/catalogue costs and validate larger streaming workloads.
