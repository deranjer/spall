# ENG-114 commit, transfer and reset follow-up

2026-10-06. Continuation of the full 4096-cell seed-1 memory/stress pass.
All 196,608 terrain bricks, water, ecology, exact collision, authoritative
ownership and the existing admission/acceptance limits remain unchanged.

## Changes and isolated evidence

`JobToken::reading_many` adds the complete dependency batch with one stable
canonical sort. The last observation of a duplicate brick wins, exactly as
with repeated individual builders. `StructureIndex::token` uses this path for
resident revisions, absent sentinels and failed-load sentinels. Validation
still checks generation, epoch and every dependency in canonical order.

The release ignored full-world staging profile measured:

| Stage | Before | Batched dependencies |
| --- | ---: | ---: |
| Structure index build | 344.046 ms | 354.211 ms |
| Read-dependency token | 2,081.948 ms | 11.071 ms |
| Dry run and support reclassification | 239.796 ms | 242.572 ms |
| Total staging | 3,205.659 ms | 1,130.852 ms |

Both runs had 218,624 dependencies, 44 detached components and 2,109 destroyed
cells; both conservation ledgers balanced. This is full-world staging evidence,
not a <=100 ms admission-to-commit gate. Conflict retries repeat staging.
Logs: `.local/eng114-next-stage-profile.log` and
`.local/eng114-next-stage-profile-batched.log`.

The label cache now shares immutable empty and fully occupied label shapes
within that cache. Full connectivity, counts, layers and face masks are
identical across material IDs. Partial connectivity retains its own payload.
Per-brick revisions and cache eviction remain separate. There is no global
cache and no support approximation. Resident-only solid counting uses the
brick's exact memoized solid count; a cell reference and snapshot-fork test
checks parity after edits. This scan was a separate inefficiency, not the
measured staging-token bottleneck.

The ignored transport profile sends 4 MiB of exact framed bulk data through
the same 8 MiB/s, 200 ms RTT, 20 ms jitter, 5% encrypted-packet-loss profile,
without world generation, physics or baseline decompression:

- CUBIC did not finish within 90 s. At the deadline, server RTT was 235.98 ms,
  congestion window 7,992 bytes, 105 packets declared lost, 97 congestion
  events. The proxy actually dropped 106 server-to-client packets. No stream
  or connection flow-control blocking frames were recorded.
- Explicit experimental BBR finished in 4,827 ms. Server RTT was 238.74 ms,
  congestion window 4,324,702 bytes, 194 packets declared lost. The proxy
  dropped 151 server-to-client packets. The receiver verified every byte.

Quinn 0.11.17 labels its BBR implementation experimental. It is opt-in through
`TransportConfig::congestion`, sandbox `--experimental-bbr`, and the distinct
`eng114-worldgen-1024m-bbr-impaired` diagnostic fixture. The ordinary production
and portable default remains CUBIC. No limits, timers, loss, world size or
workload were weakened. A small two-client real-loss regression covers reliable
records and bulk transfer with BBR, but does not establish fairness or G3/G4.
Logs: `.local/eng114-next-transfer-profile.log` and
`.local/eng114-next-transfer-bbr.log`.

## Integrated evidence

The full clean CUBIC run passed in 351,028 ms including build. Eight digs each
received exactly one committed receipt; both clients installed both resets;
31 transactions applied, zero rejected/unresolved actions and zero baseline
failures. Final server/client hash remains
`a4dfd69be466860501872ebee18ca86c1e7a7963e208bcff444ca6bc461000bd`.
Evidence: `.local/runs/eng114-next-batched-token-clean`.

Peak working sets: server 8,426,024,960 bytes (7.847 GiB); clients
2,359,537,664 / 2,361,249,792 bytes (2.197 / 2.199 GiB). These are below the
8 GiB server and 4 GiB client targets for this two-client full-residency probe.
Server headroom is only 156.32 MiB; this does not establish an eight-client or
soak memory gate. The previous reset peak was 8.076 GiB. Client ready times
were 53,641 / 53,731 ms; reset responses 41,822 / 43,704 ms.

Single-brick commit p95 was 8,954.14 ms (7 samples), still far above <=100 ms.
One action classified as a large collapse took 8,063.52 ms, also above <=2 s.
The previous single-brick p95 was 27,071.08 ms (8 samples); classification
differs, so do not present this as an identical per-class sample population.
The workload and final authoritative hash are unchanged. Neither run enabled
latency acceptance assertions; the reported functional pass is not a latency
pass. Initial network diagnostics and regression checks overlapped parts of
startup; these are observed integrated timings, not an isolated hardware gate.

The first experimental full-size impaired run failed in 337,370 ms
(`.local/runs/eng114-next-bbr-impaired`). Both clients installed the initial
world, but the shaped client's second reset was interrupted at server
shutdown. Server peak was 8,546,631,680 bytes (7.960 GiB). Preserve this as
failed-workload evidence, not full-network acceptance.

This exposed a shutdown race: the outer host closed the QUIC endpoint when
the simulation thread completed, even though a writer still owned accepted
baseline bytes. `BulkSend::finish` marks FIN without waiting for acknowledgement.
The host now stops new admission, queues shutdown after durable simulation
completion, retains control readers/heartbeats while writers drain, and closes
the endpoint afterward. `finish_and_wait` confirms all bulk bytes and FIN were
acknowledged before sending `BaselineEnd`. The drain is bounded by the existing
transport idle timeout (30 s default, 6 s CPU tests); timeout makes the server
summary fail explicitly. No tick, join, allocation or acceptance budget grows.

The CPU regression uses eight fully solid bricks with noisy material IDs and
a 64 KiB/s / 200 ms RTT link, making an accepted transfer outlive the 20-tick
server run. Immediate closure reproduces `quic stream: connection lost`; the
fixed path completes with exact final hash, zero baseline failures and exactly
20 gameplay ticks. Logs: `.local/eng114-next-shutdown-before-fix.log` and
`.local/eng114-next-shutdown-regression.log`.

The corrected full-size BBR impaired run still **failed**, in 382,069 ms
(`.local/runs/eng114-next-bbr-drained`). All eight scripted actions received
their unique commit receipts (four per client), both initial joins completed,
31 transactions committed, and the server completed both resets and exactly
7,200 ticks. Client 0 installed both resets; client 1 installed only the first
and reported one baseline-transfer failure. Its second replacement reached
64 / 89 MiB before the existing 30-second shutdown drain expired. The server
now explicitly fails with `accepted reliable traffic did not drain within
the shutdown bound`, rather than silently closing accepted traffic. Final
hash equality and full impaired convergence therefore fail. The two individual
client summaries say `passed`; their reset/failure counters and the aggregate
failed result are the acceptance evidence.

Peak working sets were server 8,429,801,472 bytes (7.851 GiB), client 0
2,362,167,296 bytes (2.200 GiB), client 1 2,352,861,184 bytes (2.191 GiB).
These measured peaks alone do not turn the failed workload into an acceptance
pass. Shaped initial ready was 114,902 ms with 94,280,212 compressed bytes
(89.912 MiB), within the diagnostic 240 s / 256 MiB limits but above G3's
30 s / 16 MiB targets. Reset responses were 42,983 / 47,552 ms. Single-brick
p95 was 3,027.30 ms (seven samples); large-collapse p95 3,027.28 ms (one
sample), both still above their targets. Early server/network checks overlapped
startup; these are observed integrated timings. No deadlines, tick counts,
network profile or admission limits were increased to disguise the failure.

The final default-CUBIC small reset regression passed in 11,665 ms including
build (`.local/runs/eng114-next-small-final`): two cuts, one unique receipt per
client, two installed resets per client, zero baseline failures and final hash
`6ded20d0a3205906576fb88cc4bfb04c88a76cea2beffb894b3e2412f95d9119`.
Reset responses were 127 / 130 ms. This checks the final shutdown code under
ordinary transport; it is not full-world performance evidence.

## Reproduction and verification

```powershell
cargo test -p spall_jobs -p spall_structure -p spall_sim --lib
cargo test -p spall_sim --test collider_origin --test atomic_commit --test logical_reload
cargo test -p spall_net
cargo test -p spall_server --lib --test segmented_join --test late_join_session --test replication_session --test water_replication
cargo test -p xtask
cargo test -p spall_sim --release full_world_stage_profile --lib -- --ignored --nocapture
cargo test -p spall_net --release --test transport impaired_bulk_transfer_profile -- --ignored --nocapture
$env:SPALL_TRANSFER_PROBE_CONTROLLER='bbr'
cargo test -p spall_net --release --test transport impaired_bulk_transfer_profile -- --ignored --nocapture
Remove-Item Env:SPALL_TRANSFER_PROBE_CONTROLLER
cargo clippy -p spall_jobs -p spall_structure -p spall_sim -p spall_net -p spall_server -p sandbox -p xtask --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
rustfmt --edition 2024 --check crates/spall_jobs/src/token.rs crates/spall_structure/src/support.rs crates/spall_structure/src/label.rs crates/spall_structure/src/graph.rs crates/spall_sim/src/world.rs crates/spall_sim/src/stage.rs crates/spall_net/src/config.rs crates/spall_net/src/tls.rs crates/spall_net/src/conn.rs crates/spall_net/tests/transport.rs crates/spall_server/src/serve.rs crates/spall_server/tests/segmented_join.rs examples/sandbox/src/bin/sandbox-server.rs examples/sandbox/src/bin/sandbox-client.rs tools/xtask/src/session.rs
git -c core.safecrlf=false diff --check
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-stress -OutputDirectory .local/runs/eng114-next-batched-token-clean -TimeoutMs 900000
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-bbr-impaired -OutputDirectory .local/runs/eng114-next-bbr-impaired -TimeoutMs 900000
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-bbr-impaired -OutputDirectory .local/runs/eng114-next-bbr-drained -TimeoutMs 900000
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-reset-small -OutputDirectory .local/runs/eng114-next-small-final -TimeoutMs 180000
& tools/package-portable.ps1
```

The CUBIC manual diagnostic is an expected measured deadline failure, not an
ordinary CI test. New ordinary tests cover batched duplicate/sentinel parity,
exact shared label shapes, cached count parity and BBR reliable recovery.
Profiling remains bounded and ignored in ordinary CPU CI.

Final ordinary checks passed: jobs 25, structure 39, simulation 72 (four
ignored profiles), collider/atomic-commit/reload integration 15; network 24
unit, 17 transport and one peer-identity test (two manual tests ignored);
server 75 unit and nine selected integration tests including the shutdown
regression and water replication; xtask 36 (one ignored). Clippy and scoped
format/diff checks passed. Clippy allowances cover existing unrelated lint
patterns; they do not suppress errors in the changed behavior.

Files in this follow-up: jobs `token.rs`; structure `support.rs`, `graph.rs`,
`label.rs`; simulation `world.rs`, `stage.rs`; net `config.rs`, `tls.rs`,
`conn.rs`, `tests/transport.rs`; server `serve.rs` and `tests/segmented_join.rs`;
sandbox server/client CLI; xtask `session.rs`; the new
experimental impaired fixture; report/protocol/validation/task documentation.
Portable README generation in `tools/package-portable.ps1` documents the
measured clean headless peak and separate GPU/long-session validation.
The final portable rebuild passed; SHA-256 verification matched all four
executables (`spall-editor`, `xtask`, `sandbox-server`, `sandbox-client`) to
their release outputs. Evidence: `.local/eng114-next-package.log` and
`.local/eng114-next-portable-hashes.json`. The shipped controller is CUBIC.
Existing work in these and other files is preserved.

ENG-114 remains in progress. Outstanding gates include <=100 ms commit
latency, full-size impaired join/reset convergence within existing bounds,
controller fairness, full-foundation
collapse, eight-client load, thirty-minute soak, GPU presentation and driver
memory. The experimental controller requires further congestion/fairness
evaluation before any default-controller decision. Next assignment: ENG-114.

## Count-proven occupancy and commit profiling continuation

The next pass adds flat timing spans for commit validation, candidate edits,
child planning, detached removal, colliders, result hashes, encoding and
publication. `full_world_commit_profile` is ignored in ordinary CPU CI. It
generates the complete 4096-cell seed-1 terrain, installs exact per-brick
colliders, warms the initial topology hash, and performs the same radius-8 cut
as the staging profile. The engine-only probe uses the fixture material
manifest and excludes game water/ecology, so its timings are not full sandbox
or G4 evidence. It checks conservation, the published result hashes and one
journal entry. The bounded before/after runs both completed:

| Measurement | Before | Count-proven labels |
| --- | ---: | ---: |
| Generation | 16,170 ms | 16,429 ms |
| Cold label warming | 19,269 ms | 16,676 ms |
| Exact world/collider initialization | 12,646 ms | 12,534 ms |
| Warm staging | 611.460 ms | 610.265 ms |
| Commit validation | 19.861 ms | 19.474 ms |
| Commit result hashes | 75.433 ms | 76.095 ms |
| Commit encoding | 50.473 ms | 53.388 ms |
| Complete commit | 188.748 ms | 190.638 ms |

Both produced 218,624 read dependencies, 44 detached components and 2,109
destroyed cells. Cold labels improved by 2,593 ms (13.46%); warmed staging and
commit were essentially unchanged. This does not explain or close the prior
3–9 s integrated action latency. Retries, concurrent game edits and scheduling
remain relevant, and both isolated staging and commit still exceed 100 ms.
Evidence: `.local/eng114-followup-commit-before.log` and
`.local/eng114-followup-commit-after.log`.

`label_brick` now uses the snapshot's exact solid count to prove empty or
completely solid occupancy, regardless of material diversity. A full brick
has exactly one six-face component, all face bits and all layers; a partial
brick still uses the existing flood fill. The cached count is invalidated by
edits and preserved only on immutable snapshots. The new parity test compares
every label and component field against the general flood fill for paletted,
wide dense and maximum-ID materials, then removes an interior plane to produce
two disconnected components while verifying the old COW snapshot stays full.
It also covers empty occupancy. No topology approximation, wire, ownership,
revision, allocation budget or timeout changes.

The full experimental-BBR impaired workload with this change **failed** in
358,155 ms including build (`.local/runs/eng114-count-proven-impaired`). All
eight unique script receipts arrived (four per client), 31 transactions
committed, both initial joins completed, and the server ran exactly 7,200 ticks
and two resets with no rejected/unresolved actions. Client 0 installed both
resets without a transfer failure and matched the authoritative final hash.
Client 1 installed only the first reset; the second reached 64 / 89 MiB and
failed when the unchanged 30-second shutdown drain expired. Aggregate final
hash equality fails. The engine's final authoritative hash is unchanged.

Observed ready times were 48,724 / 102,285 ms; compressed baseline remained
94,280,212 bytes. Reset responses were 38,820 / 40,666 ms (previous impaired
probe: 42,983 / 47,552 ms). These are single integrated observations, not an
isolated attribution or latency gate. Single-brick p95 was 4,247.91 ms (seven
samples), large-collapse p95 2,426.30 ms (one): neither target passes and the
single-brick p95 is higher than the previous impaired probe's 3,027.30 ms.

Peaks were server 8,538,198,016 bytes (7.952 GiB), client 0 2,360,553,472 bytes
(2.198 GiB), client 1 2,350,813,184 bytes (2.189 GiB). Server headroom is only
49.34 MiB. These are measured peaks from a failed workload; do not report the
impaired acceptance gate passed. The historical clean default-CUBIC pass above
remains the separate complete workload evidence.

The transport diagnostic now accepts `SPALL_TRANSFER_PROBE_MIB=1..128`, still
with its 90-second bound. Default 4 MiB keeps the original 64 KiB parts; above
16 MiB it uses shipped 1 MiB parts and validated streamed-baseline metadata
under the existing 256 MiB cap. It verifies transfer ID, each consecutive index,
part hashes, exact bytes and final count. The large probe does not decode a
world and does not increase a production allocation ceiling or timer.

The explicit BBR 96 MiB transport-only probe passed byte verification in
72,977 ms (1.315 MiB/s), under the same 8 MiB/s ceiling, 200 ms RTT, 20 ms
jitter and 5% packet loss. Server RTT was 239.79 ms, final congestion window
1,208,452 bytes, 8,909 packets declared lost and 2,865 congestion events.
The proxy actually dropped 4,011 server-to-client packets and forwarded 76,450;
server UDP egress was 115,920,001 bytes. No stream/data flow-blocking frames
were reported. This isolates substantial transport cost from world generation,
decompression, physics and restoration; it suggests rebuilding faster alone
cannot close reset convergence. A fresh raw transfer is not identical to a
replacement on an already-running connection. Further congestion, packet loss,
socket/relay buffering and pacing diagnosis is required before a policy change.
Evidence: `.local/eng114-followup-transfer-96m.log`. CUBIC remains the default.

Final default-CUBIC small regression passed in 39,943 ms including rebuild
(`.local/runs/eng114-count-proven-small`): two cuts, one unique receipt/client,
two installed resets/client, zero baseline failures, and the same `6ded20...9119`
final hash recorded above. Reset responses were 125 / 123 ms. This validates
the final behavior without claiming full-world performance.

Checks for this continuation:

```powershell
cargo test -p spall_structure -p spall_sim --lib
cargo test -p spall_sim --test atomic_commit --test logical_commit --test logical_reload --test collider_origin
cargo test -p spall_server --test segmented_join --test water_replication
cargo test -p spall_sim --release full_world_commit_profile --lib -- --ignored --nocapture
cargo clippy -p spall_structure -p spall_sim -p spall_server -p sandbox --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
rustfmt --edition 2024 --check crates/spall_structure/src/label.rs crates/spall_sim/src/commit.rs crates/spall_sim/src/stage.rs
git -c core.safecrlf=false diff --check
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-1024m-bbr-impaired -OutputDirectory .local/runs/eng114-count-proven-impaired -TimeoutMs 900000
$env:SPALL_TRANSFER_PROBE_CONTROLLER='bbr'
$env:SPALL_TRANSFER_PROBE_MIB='96'
cargo test -p spall_net --release --test transport impaired_bulk_transfer_profile -- --ignored --nocapture
Remove-Item Env:SPALL_TRANSFER_PROBE_CONTROLLER
Remove-Item Env:SPALL_TRANSFER_PROBE_MIB
cargo test -p spall_net
cargo clippy -p spall_structure -p spall_sim -p spall_net -p spall_server -p sandbox --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
rustfmt --edition 2024 --check crates/spall_structure/src/label.rs crates/spall_sim/src/commit.rs crates/spall_sim/src/stage.rs crates/spall_net/tests/transport.rs
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-reset-small -OutputDirectory .local/runs/eng114-count-proven-small -TimeoutMs 180000
& tools/package-portable.ps1
```

Ordinary checks passed: structure 40, simulation 72 (five ignored profiles),
19 selected simulation invariants, three segmented joins including the delayed
shutdown regression, two water-replication tests, Clippy, format and diff checks.
Initial ordinary checks finished before the full workload began; no concurrent
test/build load is intentionally introduced into its measured run.
After the large raw probe, network checks passed again (24 unit, 17 transport,
one peer identity; two manual tests ignored), as did expanded Clippy and format
checks. The default 4 MiB BBR diagnostic also passed in 5,317 ms after its
workload selector changed; evidence: `.local/eng114-followup-transfer-default.log`.
Test logs use `.local/eng114-followup-{core,invariants,server,net,clippy,format}.log`.
Portable rebuild passed and all four binary SHA-256s matched release outputs:
`.local/eng114-followup-package.log` and `.local/eng114-followup-portable-hashes.json`.

Files changed in this continuation: `spall_structure/src/label.rs`,
`spall_sim/src/commit.rs`, `spall_sim/src/stage.rs`, `spall_net/tests/transport.rs`, this report and
validation/task documentation. ENG-114 remains in progress; the next unblocked
work remains integrated commit latency and impaired reset convergence within
existing limits. Controller fairness, full-foundation, eight-client, soak and
GPU gates remain open.

## Bounded buffering and relay priority pass (2026-10-06)

Tested a sorted-vector adjacency/direct single-component edge candidate against
full_world_commit_profile. It preserved 218624 read dependencies, 44 splits,
2109 destroyed cells, conservation and exact published hashes, but staging was
631.267 ms versus the previous 610.265 ms and commit 202.918 versus 190.638 ms.
Index construction 298.785 ms and dry run 218.346 ms also did not improve.
The candidate was reverted; this single comparison is not a statistical
regression claim. Kept a new small exact boundary test: a full parent connects
512 checkerboard buds, cutting its face yields 513 components (17 anchored),
and one-node-budget scan resumption produces the same canonical components.

Added optional TransportConfig.udp_receive_buffer_bytes, bounded before bind to
64 KiB..=4 MiB. None remains the ordinary default. Preserves Quinn client IPv6
dual-stack setup; reports requested/actual OS size through debug tracing.
The ignored transfer profile accepts SPALL_TRANSFER_PROBE_RCVBUF in bytes.
The existing encrypted-loss BBR reliable/bulk regression exercises a 1 MiB
request on both endpoints; invalid zero, sub-minimum and above-maximum requests
must fail before socket creation. socket2 0.6.5 becomes a direct pinned dependency;
it was already locked transitively, with no version upgrade.

The 96 MiB raw BBR probe still verifies every part, byte, index and hash within
the existing 90 s diagnostic bound. Same 8 MiB/s ceiling, 200 ms nominal RTT,
20 ms jitter, 5% loss and 1 MiB parts; no world generation/decode/restoration.

| Probe | Elapsed | Sender packets declared lost | Proxy s2c forwarded/dropped | Client UDP received |
| --- | ---: | ---: | ---: | ---: |
| Prior platform default | 72977 ms | 8909 | 76450 / 4011 | 71640 |
| Requested 1 MiB endpoints | 72531 ms | 3751 | 71526 / 3735 | 71526 |
| Same request, batched due relay | 68108 ms | 4008 | 71585 / 3740 | 71585 |

Endpoint buffering removes the forwarded/received discrepancy in this probe;
it does not establish the prior discrepancy's root cause and scarcely improves
throughput. Actual granted capacity was not captured in these logs, so the
1 MiB value is a request. Keep it diagnostic-only. The relay now drains at most
64 already-due packets in deadline/insertion order per timer wake. It never
releases future packets early, retains fault/rate/queue limits and bounds each
batch to keep receive/stop handling responsive. The roughly 6% elapsed improvement
is one observation, not a repeated hardware gate. Evidence:
.local/eng114-priorities-{graph-profile,transfer-buffer,transfer-batched}.log.

No new full-world network, memory, GPU or G3/G4 acceptance is claimed. This pass
retains production CUBIC, default kernel buffering and all baseline/reset limits.
Next unblocked ENG-114 work: isolate remaining QUIC/relay pacing at high bandwidth,
then validate full-size reset convergence and integrated structural-analysis cost.

Checks for this priority pass (all final ordinary checks passed; the initial
Clippy boolean simplification was corrected before the final run):

```powershell
cargo test --offline -p spall_structure -p spall_sim --lib
cargo test --offline -p spall_net
cargo test --offline -p spall_server --test segmented_join --test water_replication
cargo test --offline -p spall_sim --release full_world_commit_profile --lib -- --ignored --nocapture
$env:SPALL_TRANSFER_PROBE_CONTROLLER='bbr'
$env:SPALL_TRANSFER_PROBE_MIB='96'
$env:SPALL_TRANSFER_PROBE_RCVBUF='1048576'
cargo test --offline -p spall_net --release --test transport impaired_bulk_transfer_profile -- --ignored --nocapture
Remove-Item Env:SPALL_TRANSFER_PROBE_CONTROLLER,Env:SPALL_TRANSFER_PROBE_MIB,Env:SPALL_TRANSFER_PROBE_RCVBUF
cargo clippy --offline -p spall_structure -p spall_sim -p spall_net -p spall_server -p sandbox --all-targets --features sandbox/client -- -D warnings -A clippy::field_reassign_with_default -A clippy::too_many_arguments
rustfmt --edition 2024 --check crates/spall_net/src/config.rs crates/spall_net/src/endpoint.rs crates/spall_net/src/proxy.rs crates/spall_net/tests/transport.rs crates/spall_structure/src/graph.rs
git -c core.safecrlf=false diff --check
& tools/measure-worldgen-stress.ps1 -Scenario eng114-worldgen-reset-small -OutputDirectory .local/runs/eng114-priorities-small -TimeoutMs 180000
& tools/package-portable.ps1
```

Final ordinary totals: 41 structure, 72 simulation plus five ignored profiles,
25 network unit, 17 transport plus one ignored profile, one peer identity plus
one ignored profile, three segmented join and two water replication tests.
No concurrent CPU test/build load was intentionally introduced during the full
commit or 96 MiB transfer profiles. Checks use .local/eng114-priorities-* logs.

Files changed this pass: Cargo.lock, crates/spall_net/Cargo.toml, its config.rs,
endpoint.rs, proxy.rs and tests/transport.rs; crates/spall_structure/src/graph.rs
(new invariant only), docs/dependencies.md, protocol.md, validation.md, tasks.md
and this report. Earlier unrelated work is preserved.

Final small default-CUBIC scenario passed in 44248 ms including rebuild: two
cuts, one unique receipt per client, two resets installed per client, zero
transfer failures/rejections/unresolved actions and exact final 6ded20...9119
hash. Reset responses were 126 / 126 ms. This is the small functional probe,
not full-world performance. Portable rebuild passed; all four SHA-256 hashes
match release outputs (.local/eng114-priorities-portable-hashes.json).
ENG-114 stays in progress with the next priorities and unrun gates above.
