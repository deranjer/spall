# ENG-114 memory optimization and full-world stress

2026-10-06. User-authorized follow-up to ENG-126. The workload remains the
seed-1 showcase world, 4096 cells (1024 m) wide, all 196,608 terrain bricks
resident, with its original water, vegetation and exact collision policy.
The server <=8 GiB and client <=4 GiB targets are unchanged.

The subsequent [commit/transfer follow-up](ENG-114-commit-transfer.md) records
the next implementation: its clean run reaches 7.847 GiB server peak, with
lower staging/commit timings and an explicit experimental controller probe.
Measurements below are the earlier pass, preserved as comparison evidence.

## Implementation

Terrain collider initialization validates all plans before changing physics,
but retains only coordinates and representation choices. It reconstructs and
publishes one exact occupancy grid at a time from the same immutable terrain.
This removes the simultaneous staging of every solid brick's occupancy grid.

Brick restoration constructs an immutable payload directly. Low-diversity
nonuniform bricks use a full-width material palette and one byte per cell;
more than 256 distinct materials retain the original u16 array. Edits expand
to u16 before mutation and compact after the batch. Snapshots retain copy on
write. Hashing, saves, wire cells, generator versions and geometry are
unchanged. Memory accounting charges actual palette/array bytes; incoming
baseline and reload admission still reserve conservative full-width costs.

Each packed payload uses one allocation containing 32,768 byte indices and a
512-byte palette. Workers produce full-width immutable arrays; the owner packs
them after joining the workers, retaining source snapshots until packing ends.
This lifetime matters on Windows: freeing each large source between small
packed allocations severely fragmented the heap. A full-size candidate spent
486.75 s validating collision plans and never completed client installation
before its deadline (`eng114-1024m-packed-bulk`). It is a failed run.

The ignored 196,608-brick allocator diagnostic isolates this effect: parallel
packed construction took 78,312 ms and subsequent 1,000 grid-buffer allocations
took 1,377 ms. Dense worker construction followed by owner packing with source
lifetimes retained took 1,331 ms plus 699 ms; subsequent allocations took 6 ms.
These allocator measurements explain the change; they are not physics gates.
An attempted 4,096-brick packing batch took 60,496 ms and left subsequent
allocations at 1,991 ms (`.local/eng114-allocator-owner-batches.log`), so it
was rejected. Only the manual diagnostic exposes that alternative.

Reset installation compares every incoming cell before reusing an old immutable
payload. Incoming revisions and modified-air metadata remain authoritative;
hash-cache reuse requires matching metadata. The old replica survives until
the complete replacement validates. The largest-world client staging budget
remains 8 GiB.

The scenario harness now supports generated worlds and scripted admin resets.
It requires every replica to install both resets and match the final server
hash. A per-client set counts each scripted request's committed receipt once:
ecology transactions and duplicate acknowledgements cannot satisfy that check.
The memory supervisor samples only processes identified in that run's logs.

## Measured evidence

The pre-palette run `.local/runs/eng114-1024m-stress-reuse` passed the original
harness: eight requested/staged digs, zero rejections or unresolved actions,
31 total transactions including ecology, two resets installed by both clients,
44 final detached bodies and a maximum eight-brick detached body. All three
final hashes were
`a4dfd69be466860501872ebee18ca86c1e7a7963e208bcff444ca6bc461000bd`.
That run predates the stronger per-script receipt check and temporarily used
a 9 GiB reset admission allowance. The final configuration restores 8 GiB.

| Measurement | Earlier ENG-126 | Collider streaming + reuse, before palette |
| --- | ---: | ---: |
| Server peak working set | 14.10 GiB | 10.01 GiB through two resets |
| Client peak working set | 4.14 GiB | 4.175 GiB |
| Compressed initial baseline | 94,280,212 bytes | 94,280,212 bytes |
| Client 0 ready | about 95 s | 60.443 s |

The first full-size reset test failed explicitly at the 8 GiB staging boundary:
the uncompressed old replica plus conservative incoming staging needed
8,938,708,996 bytes. Evidence is preserved in
`.local/runs/eng114-1024m-stress-1`; it is not a successful reset result.

The packed small fixture passed two cuts, both resets on both replicas, one
unique scripted commit receipt per client, and final hash
`6ded20d0a3205906576fb88cc4bfb04c88a76cea2beffb894b3e2412f95d9119`.
Final evidence: `.local/runs/eng114-small-final`, exit 0, 26,444 ms including
build; reset responses were 119 and 118 ms, zero baseline failures. This is a
correctness fixture, not a large-world memory gate.

The final packed clean run (`.local/runs/eng114-1024m-batch-packed`) passed in
414,215 ms. Both clients confirmed all four assigned digs exactly once and
installed both resets. Eight actions were requested/staged, zero rejected or
unresolved, and all replicas applied 31 transactions without rejection. The
final hash matches the pre-palette run above: geometry and authoritative state
are unchanged. Client 0 was ready in 54,156 ms; reset responses took 45,401 and
45,885 ms. Compressed baseline size remains 94,280,212 bytes.

Measured peak working sets were 8,671,567,872 bytes (8.076 GiB) server and
2,357,305,344 / 2,356,572,160 bytes (2.195 / 2.195 GiB) clients. Both clients
meet the 4 GiB target; the server exceeds 8 GiB by 77.85 MiB during reset.
Initial server peak was about 6.03 GiB. This is an improvement, not a passed
server memory gate. The final configuration uses the original 8 GiB client
staging admission budget throughout.

The corrected impaired run (`.local/runs/eng114-1024m-batch-packed-impaired`)
failed explicitly in 359,788 ms. The shaped client (8 MiB/s, 200 ms RTT,
20 ms jitter, 5% encrypted-packet loss) never installed its initial baseline
before the 7,200-tick server run ended. The transparent client installed its
initial baseline but lost the connection during reset installation. Only four
of eight requests reached the server, one reset occurred, neither replica
installed a reset, and final hashes did not agree. Server peak was
8,742,285,312 bytes (8.142 GiB); this failed workload cannot establish a memory
gate pass. Its startup overlaps the allocator diagnostic and later regression
checks, so its timing is not an isolated performance comparison. The failure
establishes that this bounded fixture needs network/transfer profiling; it does
not establish whether insufficient run duration or a transport defect is the
root cause. Admission limits and workload were not relaxed to make it pass.

An earlier impaired probe was stopped after 567 s without either client
installing its baseline; its 3.32 GiB reading is not a successful memory result.
A separate clean probe crossed a roughly 74-minute host/tool execution gap and
missed its startup deadline; exclude it from performance comparisons.

## Checks

Passed:

```powershell
cargo test -p spall_voxel --lib
cargo test -p spall_store
cargo test -p spall_worldgen --release
cargo test -p spall_voxel -p spall_store -p spall_physics -p spall_structure -p spall_mesh -p spall_sim --lib
cargo test -p spall_sim --test logical_reload --test atomic_commit --test collider_origin
cargo test -p spall_server --lib --test segmented_join --test water_replication --test residency --test residency_pass --test client_residency
cargo test -p spall_server --release --test water_persistence -- --include-ignored
cargo test -p spall_client --lib
cargo test -p xtask
cargo test -p sandbox --release --test worldgen_scene
git diff --check
cargo fmt -p spall_voxel -p spall_worldgen -p spall_physics -p spall_structure -p spall_sim -p spall_client -p spall_server -p sandbox -p xtask -- --check
cargo clippy -p spall_voxel -p spall_worldgen -p spall_physics -p spall_structure -p spall_sim -p spall_client -p spall_server -p sandbox -p xtask --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
```

The two clippy allowances cover existing unrelated code. Ignored hardware and
capacity measurements are separate from ordinary test success. New coverage
includes palette boundaries, material ID 65535, canonical parity, COW edits,
metadata-sensitive restoration, reset completion and false workload success.
Final verification outputs are preserved in `.local/eng114-final-*.log`.
The portable folder was rebuilt with `& tools/package-portable.ps1`; all four
copied executables were checked against release-build SHA256 hashes. Build
output is in `.local/eng114-portable-package.log`. GPU presentation and driver
memory are not included in these headless measurements.

Files changed for this pass: voxel `brick.rs`, `volume.rs`, `accounting.rs` and
`random_parity.rs`; worldgen `generate.rs`; physics `occupancy.rs`; structure
`label.rs`; simulation `world.rs` and `backing.rs`; server `baseline.rs`,
`residency.rs` and `residency_pass.rs`; client `replica.rs`, `segmented.rs`,
`net.rs`, `lib.rs` and `residency.rs`; sandbox `sandbox-client.rs`; xtask
`session.rs`; the three `eng114-*` scenario fixtures; memory/package scripts;
and protocol, worldgen, validation, task and report documentation. Other dirty
files belong to existing work and are not attributed to this pass.

Reproduce the bounded probes from a freshly built debug xtask:

```powershell
cargo build -p xtask
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-reset-small -OutputDirectory .local/runs/eng114-small-new -TimeoutMs180000
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-stress -OutputDirectory .local/runs/eng114-large-new -TimeoutMs900000
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-impaired -OutputDirectory .local/runs/eng114-impaired-new -TimeoutMs900000
```

## Open gates and next assignment

The packed clean run's single-brick commit p95 was 27,071.08 ms against the
<=100 ms target. The impaired fixture's 256 MiB / 240 s join limits are
explicit diagnostic limits, not G3's 16 MiB / 30 s acceptance. Eight digs are
not the whole-foundation-collapse gate, eight-client load or thirty-minute
soak. No new GPU presentation percentile or capture is claimed. ENG-114 stays
in progress; next work is ENG-114 commit/transfer profiling, reducing atomic
server reset peak below 8 GiB, and the outstanding full-size destruction gates.
