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

## Regional replica convergence continuation (2026-10-07)

The unchanged `eng114-regional-1024m-stress` fixture (4096 cells, 196,608 terrain
bricks, two regional clients, 7,200 ticks, eight cuts, two resets) was repeated with
per-process tracing. Earlier "no control-read error was logged" statements were not
evidence: `EnvFilter::from_default_env()` shows only errors unless `RUST_LOG` is set,
and harness-spawned children lose stderr. `sandbox::init_tracing` now honours
`SPALL_LOG_FILE=<prefix>` (one `<prefix>.<pid>.log` per process); run with
`RUST_LOG=spall_client=info,spall_server=info,spall_net=info`. Each cause below was
found from those logs, not inferred.

### Causes found and fixed

1. **Mover dropped by the server** (`reliable backlog exceeded`). Log: 1,812 queued
   single-brick repair transfers plus 236 water chunks. The server sends each repair
   as its own serialized bulk transfer, while the client could request 4 per mover
   step with no cap on outstanding requests, and retried bricks still queued. The
   client residency pass now bounds outstanding reloads (`MAX_OUTSTANDING_RELOADS`,
   256) and retries a pending brick only when a later request overtook it or nothing
   completed for a cooldown. Server limits are unchanged.
2. **Evicted brick treated as absent.** An unedited terrain brick is `Revision(0)`,
   the same value an absent brick reports, so a transaction whose `before` matched an
   evicted digest was replayed onto air and failed its result hash. An evicted brick
   now always requires its repair first.
3. **Transaction partly past a repair patch.** When a repair patch carried some of a
   transaction's bricks to the server's current state while others stayed at `before`,
   replaying the ops re-applied them over patched bricks. The remaining resident
   bricks are now repaired too, so the transaction resolves as already incorporated.
4. **Detached bodies never created.** An "already incorporated" transaction was
   dropped, but a brick patch cannot create the bodies a split makes. Such a
   transaction now replays only its child-creating ops, plus its source-side baseline
   patch applied only where the patch revision is newer than the replica's (the patch
   rewrites bricks beyond the transaction's `before` list), and validates only the
   child volume hashes. Offline reproduction:
   `a_distant_collapse_splits_a_digest_only_region_and_converges`.
5. **Result hash validated over a held predecessor.** A declared result hash covers
   the whole volume, so a transaction cannot be validated while an earlier one on the
   same volume is still held for a repair or bulk blob. It now queues behind its
   predecessors (retried in id order; a resolved bulk split retries its successors).
6. **Repair request hashed the whole volume.** Every brick `RepairRequest` filled
   `current_hash` with a full-volume canonical hash (about 250 ms at this size) on
   every retry; the server never reads it. Control-reader profile: roughly 100% of
   the mover client's time was repair patches, retry time 283 ms with one held
   transaction and 2.3 s with 25. The fields are now zero, as the residency pass
   already sent them.
7. **Sparse tick observation.** The client's observed tick advanced only on motion
   snapshots and transactions, so a mover that sees few of either learned the server
   tick late: scripted resets fired after the server had finished, prediction lead was
   mis-measured, and repair cooldowns never expired. Completed water frames (steady
   broadcast, carrying `server_tick`) now advance it, and trigger a retry of held
   transactions so a dropped repair request is re-sent.

Regression tests (each confirmed to fail without its fix, except as noted): replica
`an_evicted_unedited_brick_is_repaired_not_replayed_as_absent`,
`a_partly_patched_transaction_repairs_the_rest_instead_of_replaying`,
`an_already_patched_split_still_creates_its_body_and_updates_only_stale_source_bricks`;
server `a_distant_collapse_splits_a_digest_only_region_and_converges`,
`a_transaction_waits_behind_a_held_predecessor_on_the_same_volume`; residency
`live_client_bounds_outstanding_reloads_and_resumes_as_patches_land` and
`live_client_retries_a_request_overtaken_by_a_later_completion` (the latter does not
separate the overtaken path from the stalled one on this small fixture). Causes 6 and 7
have no dedicated failing-first test; the repair-cooldown re-request is covered inside
the evicted-brick test.

### Measured results (single local observations)

Stress, `.local/runs/eng114-regional-stress-trace17-run`: all 31 transactions and 8
unique scripted cut receipts committed, both resets installed on both clients, zero
rejected transactions, zero baseline failures, and the server and both clients end on
hash `a4dfd69be466860501872ebee18ca86c1e7a7963e208bcff444ca6bc461000bd`. Peaks: server
8,460,922,880 bytes (7.88 GiB), clients 175,542,272 and 157,212,672 bytes. Reset
responses on the server 20.4 s and 22.6 s. Single-brick commit p95 9,155.6 ms over 8
samples (target <=100 ms; unmet). The harness still reports **failed**: the mover's
movement criteria are unmet. `max_correction_m` was 19.82 m against the 0.75 m limit
(reconcile displacements near 20 m during the walk at ticks 1,146 to 2,484), and
`distance_travelled_m`, which is net displacement from spawn, reads 4e-5 m on an
out-and-back path (`max_distance_from_start_m` 97.3 m; the fixture asserts outbound
distance separately). Earlier runs never reached these checks because the script
stalled after 297 to 411 prediction ticks; it now runs 2,657. Largest reconcile
displacement across successive runs: 60.0, 59.7, 60.8, 41.5, 31.3, 31.8, 24.8, 19.9 m.
The thresholds and fixture are unchanged; this is open work, not a pass. Only one run
of the final configuration passed convergence; earlier configurations varied between
runs, so repeatability is not established.

Impaired, `.local/runs/eng114-regional-impaired-trace1-run`: the transparent mover
client passes everything (both resets, exact server hash, zero rejections,
`max_correction_m` 0.075 m, 2,232 prediction ticks). The shaped client (8 MiB/s,
200 ms RTT, 20 ms jitter, 5% loss, production CUBIC) received its 4,932,314-byte join
baseline in 185,300 ms against the unchanged 30 s limit, failed its first reset
transfer, and never advanced past the initial hash. The impaired workload therefore
still fails; transport pacing is unchanged and still open.

Sequence of failing runs kept as evidence: `eng114-regional-stress-logged`,
`-trace3` to `-trace16` (and their `-run` directories) under `.local/runs/`.

### Checks

`cargo test -p spall_client` (98 lib, 2 regional staging, 2 oversized split),
`cargo test -p spall_server --lib` and the `regional_baseline` (7),
`client_residency` (8), `residency`, `residency_pass`, `segmented_join`,
`water_replication`, `late_join`, `repair_identity` tests, `cargo test -p xtask --bin
xtask`, `cargo test -p sandbox --lib`, `cargo test -p spall_sim --test logical_reload
--test atomic_commit --test collider_origin`: all passed. `cargo fmt` check on the
touched packages, `git diff --check`, and the scoped clippy command from the checks
above passed.

### Remaining

Prediction quality while a fast mover streams (the 0.75 m criterion), shaped-client
baseline and reset transfer under loss, single-brick commit latency, atomic reset cost
(about 20 s of server tick stall per reset), and the eight-client, whole-foundation,
soak and GPU gates remain unmet or unrun. Known protocol gap, untested at scale: an
inline (non-baseline) split's source-removal runs touch bricks beyond `before`, which
a replica holding them only as digests cannot detect before replay. Held transactions
are bounded (`max_pending_repair_txns`, 64); queueing successors behind predecessors
makes that bound easier to reach under a long repair stall.

### Mover prediction lag: motion blocked behind bulk repairs (2026-10-07, later)

With convergence fixed, the stress fixture's movement criteria were the remaining
failures. Two findings, both from logs:

8. **Server writer held motion behind serial bulk repairs.** `serve_conn` drained a
   client's whole reliable batch and sent each single-brick repair transfer in turn
   (each waits for the peer's acknowledgement, about 20 ms), and only then sent the
   batch's motion datagrams. With the client's 256 outstanding reloads queued, the
   mover's own motion snapshots were about 5 s late. New server-side logging of input
   arrival showed the mover's frames carrying `intended_tick` 215 to 290 ticks behind
   the server tick (mean about 4.5 s), and each intermittent large correction was a
   matched prediction record about 280 ticks away from the delayed authoritative
   state (about 18 to 20 m at 4.5 m/s). The writer now sends a batch's motion first
   and again after every reliable message (`OutboundHandle::take_motion`); reliable
   ordering and backlog limits are unchanged. Unit test
   `motion_can_be_taken_without_draining_the_reliable_queue`.
   Measured after the change: input `intended_tick` lead -2 to +2 ticks, largest
   reconcile displacement 0.94 and 0.91 m (previously 19.9 m), `max_correction_m`
   0.253 and 0.257 m against the unchanged 0.75 m limit.
9. **Harness distance criterion.** `distance_travelled_m` is net displacement from
   spawn, so the fixture's out-and-back mover read about 0 against the 2 m minimum
   while `max_distance_from_start_m` was 97 to 105 m. The criterion now uses the
   larger of the two (never smaller than before); unit test
   `an_out_and_back_mover_counts_its_farthest_point_as_travel`. Fixture thresholds
   are unchanged.

Result: `eng114-regional-1024m-stress` reports **passed** (31 transactions, 8 unique
cut receipts, both resets on both clients, zero rejections, server and both clients
on `a4dfd69b...bd`, all admission and requirement checks met) in
`.local/runs/eng114-regional-stress-trace23-run`, `-trace24-run` and `-trace25-run`.
Trace23 predates the motion change and had `max_correction_m` 0.15 m by luck; the
earlier trace17 and trace19 runs of the same code showed 19.8 m and 18.3 m, so the
intermittent event was real and is attributed to cause 8 from the input-arrival
evidence, not from a run that reproduced it afterwards. Server peak about 7.87 GiB
(8,456,204,288 bytes in trace23), clients about 150 to 170 MiB. Single-brick commit
p95 8,831.7 and 4,356.6 ms (8 samples each) against the <=100 ms target: still unmet.
A pass here means this bounded two-client regional workload only; it does not close
G3/G4, whole-foundation, eight-client, soak or GPU gates.

Impaired, `.local/runs/eng114-regional-impaired-trace2-run` (final code): the transparent
mover client passes (both resets, zero rejections, `max_correction_m` 0.442 m, 2,852
prediction ticks, final hash equal to the server's `0d3e9dda...`). The shaped client
(8 MiB/s, 200 ms RTT, 20 ms jitter, 5% loss, production CUBIC) again took 182,511 ms to
receive its 4,932,314-byte join baseline against the unchanged 30,000 ms limit, failed
its first reset transfer (`baseline_transfer_failures` 1), and never advanced past the
initial hash `60ac1dfc...`; the scenario is **failed**. Join transport pacing is the
remaining blocker for that fixture and is not addressed here.

### Remaining after this pass

Shaped-client join and reset transfer under loss; single-brick commit latency (p95
4.4 to 8.8 s against <=100 ms); atomic reset cost (about 19 to 24 s of server tick
stall per reset); and the eight-client, whole-foundation, soak and GPU gates. The
statements above under "Remaining" and the "mover criteria are unmet" paragraph in
"Measured results" describe the state before causes 8 and 9. Known protocol gap,
untested at scale: an inline (non-baseline) split's source-removal runs touch bricks
beyond `before`, which a replica holding them only as digests cannot detect before
replay. The 64-entry held-transaction bound is easier to reach now that successors
queue behind predecessors. The mover's reload queue (256 outstanding) still makes the
reliable stream deliver water, vegetation and transactions up to about 5 s late while
a fast mover streams; only motion datagrams were moved out of that path.

### Deferred terrain catalogue: join pacing under loss (2026-10-07, later)

Decision (user): the shaped client's join could not meet 30 s by shrinking the payload. With
CUBIC at 5% loss and 200 ms RTT the link delivers about 30 KB/s (measured 26 to 46 KB/s), the
catalogue's 64,500 distinct 32-byte hashes are an incompressible 2.06 MB (2.35 MB columnar,
2.62 MB as rows with zstd, measured on the seed-1 4096-cell world), so any whole-world baseline
takes over a minute. The catalogue is therefore sent after readiness; see
[protocol.md](../protocol.md) for the records.

Defects found on the way, each from logs and each with a test where noted:

1. A deferred baseline for a world with no digests left the client waiting for a catalogue that
   never came (now a complete version-3 baseline).
2. A water stream about seven times the link rate filled the 8 MiB reliable cap and got the shaped
   client disconnected; water is now produced only for clients that are keeping up
   (`water_is_held_back_while_unsent_water_or_a_catalogue_is_outstanding`).
3. Catalogue chunks written ahead of what the link could carry put minutes of data in front of the
   next `BaselineBegin`; a chunk credit window fixes it
   (`catalogue_credit_follows_the_catalogue_being_fed`).
4. A delay-only congestion gate reacted too late when producers outrun the link; the gate now also
   bounds unconfirmed bytes by the measured delivery rate
   (`unconfirmed_bytes_are_bounded_by_the_measured_delivery_rate`,
   `the_control_stream_is_congested_while_probes_come_back_slowly`).
5. A fixed probe window kept the stream congested forever for a client that could not echo while it
   installed a baseline (the window now slides).
6. A delta basis was refused while transactions were held for repairs, ending the client's control
   reader; the check now requires only a complete catalogue
   (`a_reset_delta_merges_even_when_an_edit_is_still_held_for_repair`).

Offline protocol tests: `a_deferred_catalogue_completes_the_replica_to_the_server_hash`,
`a_corrupted_or_misordered_catalogue_is_refused`,
`transactions_wait_for_the_catalogue_then_apply_in_order`,
`a_brick_reloaded_before_the_catalogue_lands_is_not_replaced_by_its_digest`,
`a_reset_catalogue_lists_only_what_changed_and_converges`, and the real-QUIC
`quic_deferred_catalogue_join_traversal_distant_edit_and_reset_converge`.

New fixtures (same workload, limits and thresholds as their originals; they add
`"defer_catalogue": true`): `eng114-regional-1024m-stress-deferred` and
`eng114-regional-1024m-impaired-deferred`. The impaired variant keeps the 8 MiB/s, 200 ms RTT, 20 ms
jitter, 5% loss profile, production CUBIC, and the 16 MiB / 30 s join budget. **Meaning of the 30 s
bound changes**: the harness measures the join from the client's connect to the baseline being
installed and the first keyframe confirmed, which now excludes the catalogue. The 16 MiB bound
counts baseline plus catalogue bytes received. Every client must still end on the server's exact
final hash, so a catalogue not finished by the end of the run fails the fixture.

Measured (single local runs; run directories under `.local/runs/`):

| Fixture | Result | Shaped client ready | Bytes (baseline + catalogue) | Notes |
| --- | --- | ---: | ---: | --- |
| `eng114-regional-1024m-impaired` (original) | failed | 182,511 ms | 4,932,314 | unchanged; join never completes in 30 s |
| `eng114-regional-1024m-impaired-deferred`, `eng114-deferred-impaired-trace11-run` | passed | 24,566 ms | 2,755,078 | 0 rejections, both resets, exact hash |
| same, repeat `eng114-di12-run` | passed | 22,596 ms | 2,755,078 | |
| `eng114-regional-1024m-stress-deferred`, `eng114-sd13-run` | passed | 16,964 ms | | |
| `eng114-regional-1024m-stress` (original), `eng114-st26-run` | passed | 17,427 ms | | regression check |

First catalogue on the shaped link takes about 120 s (2.6 MB at about 22 KB/s), during which the
client plays on the spawn-region core and holds transactions (29 replayed on completion);
after a reset the delta completes in 0.7 to 1.8 s. Total catalogue bytes across a join and two
resets fell from 7.87 MB (full catalogue each time) to 2.63 MB. The transparent client's join is
unchanged except that a reset no longer resends the world's digests. Server peak 7.86 to 7.90 GiB
(under 8 GiB, with about 100 MiB of headroom). Single-brick commit p95 4.2 to 8.9 s (target
<=100 ms, unmet) and the roughly 18 to 24 s atomic reset stall are unchanged.

What this does not show: a reset whose delta is large (a world very different from the one it
replaces) would approach a full catalogue on a slow link; the first catalogue still takes about two
minutes on the shaped link; only one shaped client was measured; the protocol is gated on matched
builds. Only two passes of the deferred impaired fixture exist (trace11 and di12), and several
earlier runs of the same scenario failed while the defects above were being found, so run-to-run
variance is not characterised.

### Commit latency: profile, warm structure index, and the remaining floor (2026-10-07, later)

**What the reported number is.** The harness's "single-brick commit" bucket is every commit that
detaches no body. The fixture's eight radius-8 foundation cuts land in it, and the latency runs from
admission to commit, so cuts that arrive in the same tick queue behind one another's staging. The
per-cut service time is the useful figure. `serve` now logs the staging and commit phases of any
tick where one takes 50 ms or more (`slow commit phases`).

**Where one cut's time went on the 196,608-brick world** (release, mean of 65 stagings, run
`eng114-cl1-run`): staging 832 ms = structure index build 401 + dry-run reclassify 289 + about 140
other; commit about 160 ms of which canonical result hashes 112 and validation 25. None of it
depends on the size of the edit: the index, the solid-cell counts and the hash all walk the world.

**Changes (exact; behaviour unchanged):**

1. *Post-edit solid count from the touched bricks* instead of a second whole-world pass; every debug
   build asserts it equals the full recount (`stage.total` 651 to 606 ms on the full-world profile).
2. *Warm structure index.* `Volume::state_stamp()` is a process-unique value replaced by every
   mutation (clones share it until mutated). The world keeps the terrain's structure index as the
   last non-splitting commit left it, valid only while the volume still carries that stamp, the
   topology epoch and anchor match. Staging clones it (24 ms) instead of rebuilding (about 320 ms);
   the scheduler builds it once per unchanged snapshot so a burst of cuts shares it. A commit that
   splits a body removes cells the staged index still holds, so it does not promote one; any other
   mutation (eviction, reload, water or ecology write, reset) changes the stamp and drops it. Every
   debug build rebuilds the index and compares token reads, classification and component sizes
   whenever a warm one is used, and a process-wide counter (`spall_sim::warm_index_reuses`) lets
   tests assert reuse (`tests/warm_structure.rs`).

**Measured** (same `eng114-regional-1024m-stress` fixture, single runs, both passed):

| | before (`eng114-cl1-run`) | after (`eng114-cl2-run`) |
| --- | ---: | ---: |
| `stage.structure_index_build` mean | 400.7 ms | 21.8 ms |
| `stage.total` mean | 831.9 ms | 386.2 ms |
| `commit.result_hashes` mean | 111.6 ms | 109.6 ms |
| commit-latency p95 (samples) | 5,702 ms (8) | 1,616 ms (7) |
| server peak working set | 8,442,482,688 B | 8,494,067,712 B (7.91 GiB) |

**What is left, and why the 100 ms target is out of reach without more design work.** The
per-cut floor is now staging about 390 ms (of which `SupportGraph::reassemble` rebuilds every node,
edge and component of the world from the label map on each edit, about 218 ms; its source comment
already marks an incremental patch as future work) plus commit about 100 to 190 ms (whole-volume
canonical hash about 110 ms, token validation about 25 ms). Closing that gap needs an incremental
component/support structure and an incremental canonical hash, not further tuning, and the hash
layout is protocol-visible. The G1 numbers were defined for small worlds; at this world size even a
true one-brick edit costs the same fixed amount. Server memory headroom under the 8 GiB target is
about 90 MiB at the reset peak.

Checks: `cargo test --workspace` 1,269 passed, 0 failed, 71 ignored; `cargo fmt --all --check`;
scoped clippy `-D warnings` with the same allowances as before.

### Commit latency: incremental support graph and streaming canonical hash (2026-10-07, later still)

Decision (user): attack the two remaining per-edit floors, whole-world graph reassembly and the
whole-volume canonical hash. Neither changes any observable value; each is checked against the
code it replaces.

**1. Incremental support graph** (`spall_structure/src/graph.rs`). `SupportGraph::apply_changes`
used to drop and rebuild every node, edge and component of the world (about 218 ms on the
196,608-brick world: 107,716 nodes in 45 components; node creation 65 ms, face linking 145 ms,
component scan 10 ms). It now relabels the changed bricks, then drops and recreates only the nodes
and edges of those bricks and their six face neighbours (a brick's nodes depend on its own labels
and on its neighbours' state), and re-runs the component scan, which stays global so component ids
and ordering cannot drift. To make that local: nodes live in a slab (an index is stable for the
node's life), edges are unsorted `Vec<u32>` (the scan is order-independent), and the
absent/failed-neighbour sets are reference-counted so dropping a node uncounts exactly what it
added. A relabelled-but-not-yet-refreshed brick stays in `pending_dirty`, so an update cancelled
part-way is finished by the next one (a test found this was initially missing: the relabel pass
now also covers leftovers).

Checks: `an_incrementally_updated_graph_equals_a_fresh_build_after_every_change` runs random box
edits, evictions and load failures (four seeds, both residency modes, several anchor planes) and
after every step compares the graph with a fresh `build` on components, every node's cell count,
anchor flag and unresolved bricks, the full edge set, node-to-component ids, absent/failed
dependency lists, read revisions and labels; `a_cancelled_update_is_finished_by_the_next_one`;
the existing budgeted-resume test now drives the scan through `apply_changes`.

**2. Streaming canonical hash** (`spall_protocol::canonical_topology_hash_single_layer`). The
topology hash is one flat BLAKE3 stream over every brick in sorted order, so it cannot be updated
incrementally without changing the format, which is protocol-visible and was left alone. The
commit path built a `CanonicalBrick` with two heap allocations per brick and buffered the whole
encoding before hashing. The new function writes the same bytes through a 64 KiB buffer straight
into the hasher; it requires sorted, unique input and an exact length and asserts both. Checks:
differential test against `canonical_topology_hash` (empty, 1, 2, 17 and 5,000 bricks, both
owners), the pinned-digest test still passes, and a sim test with evicted bricks compares
`volume_hash` with the general hash. `logical_bricks` and `logical_solid_cells` now walk the brick
store once (`Volume::for_each_resident_brick`) instead of a lookup, a handle clone and a second
sort per brick; a conflict between a resident and a retained brick is still reported at the
smallest coordinate in canonical order (new tests pin that and the ordering).

**Measured, per-cut service time** (same stress fixture, means over about 65 stagings and 31
commits each):

| | warm index only (`eng114-cl2`) | + incremental graph, streaming hash (`eng114-cl3`) |
| --- | ---: | ---: |
| `stage.dry_run_and_reclassify` | 288 ms | 30 ms |
| `stage.total` | 386 ms | 80 ms |
| `commit.result_hashes` | 110 ms | 22 ms |
| `commit.total` | 103 ms | 54 ms |

Admission to commit of an ordinary single cut is now about 205 to 218 ms in every run, after a
first cut of about 670 ms that includes building the warm index (`eng114-cl4` to `cl7`, from the
new per-request `commit latency` log line). That is down from about 1 s, but **the 100 ms target
is still not met**: staging plus commit is about 135 ms of work, the rest is scheduling across
tick boundaries and the tick loop's own cadence.

**The harness p95 did not improve and must not be read as if it had** (1,616 ms then, 2,728 to
2,759 ms now, 7 samples so p95 is the maximum). Per-request logging shows why: four requests
admitted around the second world reset commit 2.48 to 2.73 s later, all on the first ticks after
the reset (ticks 4502 and 4503), alongside a large collapse; every other single cut is about 210
ms. That tick contains an unprofiled 2.4 s: two scheduler warm-index builds (about 390 and 500 ms;
the second because the splitting commit changes the volume and drops the index) and roughly 1.5 s
not yet attributed (the fan-out of the collapse is only 57 ms, ruled out). The reset itself
(`admin world reset` 24 to 28 s, tick `max_lag` 25.7 s) remains the dominant open item.

**Not done, deliberately.** A splitting commit could keep the index warm by applying the
detached-cell removal to it, saving about 500 ms per post-split staging, but the index also holds
the topology epoch and read-token state, a mismatch there would not be caught by the current
cross-check and would surface as later commits failing validation; it needs its own change and
test. The canonical hash is still O(world) per commit (22 ms); a Merkle layout would remove that
and is a protocol decision.

Server peak working set stayed at 8.50 GB (7.92 GiB), under the 8 GiB target by about 90 MiB.

Checks: `cargo test --workspace` 1,276 passed, 0 failed, 71 ignored; `cargo fmt --all --check`; scoped clippy `-D warnings` (same allowances as before).

### Commit latency: where the stress fixture's slow ticks actually come from (2026-10-07, last)

New per-request `commit latency`, `slow server tick` (before / inside / after `sim.tick()`) and
`tick.*` stage spans (pipeline, water, vegetation, physics, players) now make this readable from a
log instead of inferred.

* **The world reset is entirely outside the tick.** `before_tick_ms` is 20.7 to 26.4 s (first reset)
  and 22.7 to 29.6 s (second); `sim.tick()` itself is about 0.5 to 0.7 s on those ticks.
* **A burst of edits every 60 ticks costs about 1.0 to 1.4 s of tick time.** Ticks 4141, 4201, 4261,
  4321 and 4381 each stage and commit roughly eight small edits (debris contact cuts) serially:
  staging runs on the tick thread ("no threads" in `schedule.rs`), so per-edit service time times
  burst size is tick time. The server's achieved rate is about 40 Hz against 60 Hz while that lasts.
* **The post-reset outliers were two scheduler warm-index builds (about 0.4 and 0.5 s) plus that same
  burst**, not the collapse's fan-out (57 ms, ruled out).

**Stamp fast path for read validation** (`spall_jobs::JobToken::with_state_stamp`,
`WorldView::state_stamp`). A staged edit's token now remembers the terrain volume's state stamp. If
the live volume still carries it, nothing in the volume changed since staging, so every read is
valid and the 104k-brick comparison is skipped (`commit.validate` 20 ms to 0.002 ms). Any other
stamp falls back to the full comparison. Generation and epoch are always checked first, and debug
builds run the full scan as well and assert it agrees, so every test that commits checks the
shortcut. Tests: matching stamp fresh; changed stamp still detects a stale brick and accepts an
unrelated change; a matching stamp does not excuse a stale epoch or generation.

Measured (`eng114-cl11-run`, passed, single run): ordinary single cut admission to commit 152 to
175 ms (first cut 848 ms with the warm-index build); the 60-tick bursts 0.96 to 1.14 s (were 1.24 to
1.46 s); harness single-brick p95 1,393 ms over 7 samples (4 of the 7 are cuts near the reset);
server peak working set 8,491,757,568 B (7.91 GiB). One earlier run (`cl10`) reports "exceeded
deadline"; the Windows event log shows the machine went to sleep at 16:28:24 local, exactly when
that server went quiet, and the clock jumped 102 minutes on resume, so it is an environment
failure, not a hang. The run was repeated and passed.

**Status against the 100 ms target: not met.** Per-edit work is now about 110 to 135 ms (staging
about 80 ms: volume clone 13, index clone 20, read token 11, reclassify 15; commit about 55 ms: hash
20, publish 20, edit 10), all O(world) passes with small constants. Getting below 100 ms per edit
needs structure sharing (copy-on-write volume and index instead of clones) or doing staging off the
tick thread, which `docs/architecture.md` allows (workers get immutable snapshots) but the
scheduler does not do today. That, and the 20 to 29 s reset, are the open items.

Checks: `cargo test --workspace` 1,279 passed, 0 failed, 71 ignored; `cargo fmt --all --check`.

### Edit staging off the tick thread (2026-10-07, final)

Decision (user): stage off the tick thread. `docs/architecture.md` already requires it ("CPU jobs
operate outside the tick on immutable data"; workers receive immutable snapshots and only the
owning thread applies validated results at a tick boundary); the scheduler ran staging inline.

**Design** (`spall_sim::EditPipeline::enable_off_thread_staging`, opt-in):

* One request is staged at a time on a single worker thread (`spall_jobs::ThreadJobPool`) from an
  immutable snapshot of the target volume taken on the tick thread (the volume and evicted-digest
  clone, about 13 ms, is the only staging cost left on the tick).
* Each tick installs the finished result, re-checks it (generation and epoch at install, the
  staged token at commit, as before), commits it on the tick thread, and then snapshots the world
  as that commit left it for the next request. A result whose world moved on is discarded and
  re-staged at the *front* of the queue, so requests commit in request order.
* The first staging after a world change builds the structure index on the worker, not the tick.
* A panic inside staging is caught on the worker and re-raised on the tick thread
  (`resume_unwind`) with its original message; it is never swallowed and cannot leave a request
  hanging.
* `Simulation::replace_world` swaps the whole pipeline, so an in-flight job cannot land on the
  new world; the new pipeline keeps off-thread staging. Dropping a pipeline stops its worker.
* Residency pinning (`pending_dependency_bricks`) now covers the request being staged as well as
  the queued ones.
* The per-result commit handling moved unchanged into one helper shared by both paths.
* `spall_server::serve` enables it only for a paced (real-time) server. Headless and
  deterministic runs keep inline staging; `SPALL_INLINE_STAGING=1` selects inline on a paced
  server for comparison.

**What is and is not the same as inline.** The set of cells committed is identical (tested
against the inline pipeline cell by cell). The commit *order* is not: the inline pipeline
re-queues a conflicting edit behind independent ones (eight test cuts committed as 1, 8, 2, 4,
6, 3, 5, 7), so its brick revisions, and therefore its world hash, differ; the off-thread path
commits strictly 1 to 8. Which *tick* a commit lands on now depends on wall-clock staging time,
which is why this is not the default.

Tests (`spall_sim/tests/off_thread_staging.rs`): a burst of eight overlapping and independent
cuts commits all of them in request order with the inline pipeline's cell contents; a reset with
a request in flight leaves the new world untouched, never commits the old request and keeps
off-thread staging; an idle pipeline stays idle. **Not covered by a test:** the worker-panic
re-raise (there is no safe way to make `stage_edit` panic on demand); it is a few lines of
`catch_unwind` / `resume_unwind`.

**Measured** (`eng114-regional-1024m-stress`, single passing run `eng114-ot1-run`; compare the
inline run `eng114-cl11-run`):

| | inline staging | off the tick thread |
| --- | ---: | ---: |
| ticks over 500 ms outside startup and the two resets | 5 to 6 (ticks 4141 to 4381, 0.96 to 1.14 s each) | 0 |
| ordinary single cut, admission to commit (excluding the first) | 152 to 175 ms | 158 to 227 ms |
| large collapse (`request=3`), admission to commit | 1,393 ms | 698 ms |
| single-brick commit p95 (7 samples) | 1,393 ms | 585 ms |
| server peak working set | 8,491,757,568 B | 8,485,265,408 B |

The p95 is now the first cut of the run (585 ms), which carries the one-off cold structure-index
build on the worker. Per-edit latency did not fall (staging still takes 80 ms and commit 55 ms);
what changed is that the tick no longer stalls on a burst. The 100 ms per-edit target is still
not met, and the 20 to 22 s world reset (`before_tick_ms`, outside the tick) is untouched.

Repeat run `eng114-ot2-run` (passed): no tick over 500 ms outside startup and the two resets;
ordinary cuts 158 to 275 ms; large collapse 728 ms; single-brick p95 534 ms; server peak 8,508,993,536
B (7.92 GiB, about 77 MiB under the 8 GiB target). Two runs; run-to-run variance beyond that is not
characterised.

Checks: `cargo test --workspace` 1,282 passed, 0 failed, 71 ignored; `cargo fmt --all --check`;
scoped clippy `-D warnings` (same allowances as before) clean.

### Working down the list: reset, prewarm, split-safe index, chunked volume (2026-10-07, last)

**1. World reset (20 to 30 s before the tick): investigated, nothing shipped.** The fresh world is
independent of the live one, so I built it on a background thread and kept the tick loop running.
It works as a mechanism: the server held about 58.9 Hz through the build (it is frozen for 20+ s
inline). It is not shippable, for reasons recorded here so nobody repeats the experiment blind:

* Server peak working set rose to 8.73 GB / 8.71 GB (8.13 / 8.11 GiB), over the 8 GiB target,
  because the live world keeps allocating (edit staging, water) while the second world exists.
* The reset took 32 to 34 s instead of about 26 s (CPU contention), and the swap tick still stalled
  3.5 to 4.2 s (catalogue basis, dropping the old world, per-client baselines).
* The scenario's meaning changes: the scripted cuts after the reset request now hit the world that
  is about to be replaced and are discarded with it, and the second scripted reset overlaps the
  first. Counting that run as a pass of `eng114-regional-1024m-stress` would relabel the workload,
  so it was not counted; a queued-reset variant still failed the scenario.
* World generation itself is already multi-threaded (`spall_worldgen`), and the earlier ENG-114
  startup profiling already reduced it, so a faster reset needs cheaper generation or a smaller
  fresh-world footprint, a separate worldgen effort. The code was reverted; the inline reset is
  byte-for-byte what it was (verified: a default run passes, 25.7 s and 29.5 s before the tick).

**2. Structure-index prewarm** (`EditPipeline`, off-thread staging only). With nothing queued, the
worker builds the terrain's structure index for the volume's current state, and the tick installs
it only if the volume still carries the stamp it was built from (a stale one is discarded and not
retried for the same state). The first edit after startup, after a reset, or after a split no
longer pays the 0.4 to 0.5 s cold build. A prewarm is not edit work: `is_idle` is unaffected, and
a cut submitted mid-prewarm waits for the worker and then commits. Tests in
`off_thread_staging.rs`.

**3. Index kept warm across splitting commits.** I had deferred this for epoch/token risk. A split
now stores the staged post-cut index together with the removal of the detached cells; the *next
staging* applies the removal on its own thread with the world's new epoch (the commit pays
nothing). Debug builds compare the resulting index with a fresh build by **full token equality**
(generation, epoch and every read) and component equality, on every staging against a warm index;
`a_splitting_commit_keeps_the_index_warm_with_the_new_epoch` drives a real bridge split. A first
version that did this on the tick thread cost +50 ms per splitting commit and was replaced.

**4. Chunked, copy-on-write brick storage** (`spall_voxel::Volume`). Bricks are grouped into
8x8x8-brick chunks behind `Arc`s: cloning a volume copies a few hundred handles instead of 196k
slots, and a write copies only the chunk it lands in. Dry-run volume clone 13 ms to 0.02 ms,
commit candidate 9.8 ms to 0.3 ms, a split commit's tick time 190 ms to 100 ms, and server peak
memory fell (fewer whole-map transient clones). Iteration order is now chunk-major, which no caller
depends on (callers that need canonical order already sort). Tests: bricks findable across chunk
boundaries and negative coordinates, clone independence in both directions, removal of a chunk's
last brick and of an absent brick. A neighbour list that is inline up to six entries (graph
edges) cut the index clone from 24.6 ms to 16.9 ms; a model test covers the inline/heap switch.

**Measured** (`eng114-regional-1024m-stress`, passing single runs; each column is the state after
the named change):

| | off-thread staging (`ot2`) | + prewarm (`pw1`) | + split-safe index (`pw2`) | + chunked volume (`cow1`) |
| --- | ---: | ---: | ---: | ---: |
| first cut | 534 ms | 214 ms | 211 ms | 166 ms |
| ordinary cuts | 158 to 275 ms | 198 to 220 ms | 200 to 213 ms | 117 to 128 ms |
| large collapse | 728 ms | 432 ms | 527 ms | 352 ms |
| cut queued behind the collapse | 275 ms | 1,076 ms | 669 ms | 199 ms |
| single-brick p95 (7 samples) | 534 ms | 1,076 ms | 669 ms | 199 ms |
| server peak working set | 7.92 GiB | 7.91 GiB | 7.91 GiB | 7.87 GiB |

(The `pw1` column's 1,076 ms is the cut that arrives right behind the collapse and found no warm
index; `pw2` and `cow1` fix it. Run-to-run server peak has ranged 7.87 to 7.99 GiB across
otherwise equivalent runs, so the 8 GiB margin is real but thin.) An ordinary cut is now 117 to
128 ms admission to commit, from about 1.0 s at the start of this work and 200 ms before this
section. The 100 ms target is still not met.

Final confirming run of the finished build (`eng114-fin1-run`, passed): ordinary cuts 112 to 118
ms, first cut 166 ms, large collapse 333 ms, cut queued behind the collapse 199 ms, single-brick
p95 199 ms (7 samples), server peak 8,444,743,680 B (7.86 GiB). The world resets still stall the
server 25.9 s and 29.2 s before the tick (unchanged, inline).

Checks: `cargo test --workspace` 1,289 passed, 0 failed, 71 ignored; `cargo fmt --all --check`;
scoped clippy `-D warnings` (same allowances as before) clean. Not run: GPU, portable rebuild,
eight-client and soak gates. Single runs only, so run-to-run variance is not characterised.

**Still open:** the 100 ms per-edit target (112 to 118 ms now; remaining worker cost is the index
clone 17 ms, read token 11 ms and snapshot work, tick cost is commit encoding, hashing and
publish), the 25 to 30 s world reset (needs cheaper generation or a smaller fresh-world
footprint), the canonical hash being O(world) at about 17 ms per commit (a Merkle layout is a
protocol decision), and the unrun gates above.

### Regression found: `eng114-regional-1024m-impaired-deferred` now fails (2026-10-07, after the commit-latency work)

Re-running the deferred fixtures on the finished build: `eng114-regional-1024m-stress-deferred`
passes (`eng114-sd1-run`: ordinary cuts 102 to 120 ms, p95 207 ms, peak 8,472,109,056 B). The
**impaired** deferred fixture fails twice (`eng114-id1-run` with off-thread staging, `eng114-id2-run`
with `SPALL_INLINE_STAGING=1`): the server and the fast client end on the same hash with nothing
rejected, but the shaped client ends with `catalogue_pending_at_end = true` and a different hash.

Cause, from the logs and the two earlier passing runs (`eng114-di12`, `eng114-deferred-impaired-trace11`):
the shaped link carries the 2.6 to 2.9 MB catalogue in about 120 s. In both earlier passes that
first catalogue completed 10 to 14 s **before** the first scripted world reset (15:43:40 vs 15:43:54,
15:39:23 vs 15:39:33), so every later catalogue was a small delta. The reset ticks are fixed, and
the commit-latency work removed the multi-second tick stalls, so the first reset now arrives about
40 s earlier in wall-clock time (run length 206 to 211 s against 247 to 256 s) and the first
catalogue is still 20 to 30 s from done. A client with an incomplete catalogue is sent a *full*
catalogue after a reset (`docs/protocol.md`, "Delta catalogue for world resets"), so the transfer
restarts at the first reset and again at the second, and the run ends before it can finish.

This is not corruption or a wrong result, and not a fixture to relax: the earlier passes depended on
about 10 s of margin. It is a real product weakness the margin was hiding: delivered catalogue
progress is thrown away by a reset. The inline-staging rerun did not recover the old timing because
the graph, hash and clone optimisations speed inline staging up as well.

### Resumable catalogue: the impaired-deferred regression fixed (2026-10-08)

Decision (user): fix the product weakness the regression exposed instead of editing the fixture. A
world reset that interrupts a client's catalogue now keeps everything that client already
verified. Protocol rule in [protocol.md](../protocol.md) ("Resuming an interrupted catalogue").

**Why segment size mattered.** A client holds a catalogue segment by segment, and the baseline's
4 MiB segments hold about 32,000 digests, so the real world's catalogue (64,500 digests) was two
segments: a client 70% through would still hold only one half, and on the fixture's timeline would
have lost most of its progress at the second reset anyway. Catalogues now use 128 KiB segments
(about 1,000 digests, one or two 32 KiB chunks), so the unit of progress is a second or two on the
shaped link.

**Mechanism.**

* Server: each catalogue keeps a `CatalogueLayout` (digests carried, where each segment ends, the
  basis it was built on). The writer records how many chunks it has written; the control stream is
  ordered, so the client has processed all of them before the reset's baseline, and the count is a
  lower bound. A reset to a client still receiving builds its delta against the basis plus every
  segment ending within those chunks, and says so in `CatalogueChunk.basis_chunks`.
* Client: the catalogue receiver records each completed segment's digests and byte end; when a
  reset replaces the pending catalogue that becomes resume state, and the delta's first chunk
  selects exactly the prefix the server assumed (ignoring anything received beyond it).
  A client that processed fewer chunks than named, or has nothing to resume, refuses loudly.
* An interrupted delta resumes from its own basis plus its completed segments, so resets chain; a
  client with nothing verified, or a new world lacking a brick the client holds, gets a full
  catalogue as before. The merged replica must still reproduce `expected_world_hash`.

**Tests** (`spall_server/tests/catalogue_resume.rs`, a 4,096-brick world with distinct bricks,
about five chunks): the server's and the client's held sets are identical brick by brick; a
mid-catalogue reset converges and carries less than a full catalogue; a second reset during the
resumed delta converges; a client ahead of the server's count trims to it; nothing verified means
a full catalogue; a basis beyond what was processed is refused; a new world lacking a held brick
falls back to a full catalogue. Wire tests in `spall_protocol` (basis only on a delta).

**Measured** (real processes, 8 MiB/s, 200 ms RTT, 20 ms jitter, 5% loss, production CUBIC):

| `eng114-regional-1024m-impaired-deferred` | before the fix | after (`eng114-rz1-run`) | after (`eng114-rz2-run`) |
| --- | --- | --- | --- |
| result | failed twice | passed | passed |
| shaped client catalogue at end | still pending | complete, exact hash | complete, exact hash |
| shaped client catalogue bytes, whole run | restarted at both resets | 2,680,228 | 2,668,088 |
| shaped client ready | 24 to 25 s | 25 s | 25.0 s |
| server peak working set | 8.47 GB | 8,502,902,784 B (7.92 GiB) | 8,498,810,880 B (7.92 GiB) |

2.67 MB is one full catalogue (2.63 MB): the run re-sends essentially nothing. Run 2's first
delta finished within a second of the second reset, which shows the margin is still modest but
no longer decisive, since an interruption now only costs the segment in flight. Regression
checks on the finished build: `eng114-regional-1024m-stress-deferred` passes (`eng114-sd2-run`,
p95 197 ms, peak 8,506,228,736 B).

Checks: `cargo test --workspace` 1,297 passed, 0 failed, 71 ignored; `cargo fmt --all --check`;
scoped clippy `-D warnings` clean. Two impaired-deferred passes; variance beyond that is not
characterised. Not run: GPU, portable rebuild, eight-client and soak gates.

### Per-edit latency under 100 ms (2026-10-08)

New per-request timing (`commit latency` log: queued, staging, commit, fan-out) split an
ordinary edit's ~115 ms into about 66 ms staging (job start until the tick sees the result), 34 ms
commit, no queueing or fan-out, and ~15 ms of arrival and tick-boundary waits that 60 Hz makes
inherent. A warm full-world profile (`full_world_commit_profile`, now three small warm cuts) put the
real work at ~40 ms on the worker and ~20 ms to commit; the server pays about 25 ms more
(contention, the large evicted-digest set).

Three exact changes removed worker-side O(world) work:

* **Component scan.** Membership of the giant component was sorted (~8 ms); it now comes from one
  pass over the canonical node order. Ids, order and totals are unchanged (the randomized
  differential test and the budgeted-resume test cover it).
* **Read token.** The graph's revision map is keyed `(z, y, x)`, so the token is built in
  canonical order without sorting (about 6 of its 12 ms).
* **Index ownership.** The worker takes the warm index out of the world and edits it in place;
  the commit hands the edited one back. This removes the 13.6 ms whole-index clone. The cost is
  that a staging that is discarded or fails after taking the index loses it; the pipeline clears
  its "already tried" marker so an idle tick rebuilds it (`an_index_lost_to_a_failed_staging_is_rebuilt_while_idle`,
  checked by removing the clearing line, which makes the test hang and fail). Inline staging
  still clones.

Warm profile, ordinary cut: staging 39 to 43 ms down to 12 ms (index work 0, token 4, reclassify
5); commit 16 ms, now almost entirely the canonical hash.

**Measured** (`eng114-tm2-run`, passed, single run, 7 samples):

| request | admission to commit |
| --- | ---: |
| ordinary cuts (4) | 65.6, 68.3, 69.5, 74.2 ms |
| cut after the large collapse's follow-up | 88.3 ms |
| first cut of the run | 108.6 ms |
| cut queued behind the 44-way collapse | 128.1 ms |
| large collapse | 277.1 ms |

Ordinary edits meet the 100 ms target (65 to 88 ms, from 112 to 118 ms). The harness bucket's p95
is still 128.1 ms because it is the maximum of 7 samples and includes the two cases above: the
first cut (cold caches, not yet understood) and the cut that must absorb the collapse's removal.
The fixture configures no latency target. Server peak 8,444,493,824 B (7.86 GiB).

Checks: `cargo test --workspace` 1,298 passed, 0 failed, 71 ignored; fmt and scoped clippy clean.

### World reset and startup: generation and collider setup (2026-10-08)

A reset regenerates the whole world, so it costs what startup costs (generation, collider setup,
label warming) on a machine that is also running the server and clients. The stage breakdown of
the 4096-cell world (8 cores / 16 threads, release) was generation 14.6 s, collider setup 12.7 s,
label warming about 5 s.

**Generation: no raw arrays.** Workers built each dense brick as a raw 64 KiB array, the serial
owner compacted it afterwards, and every raw array had to be kept alive until the end (to protect
the Windows heap) and then freed in bulk, which alone took 6.6 s. Workers now fill one reused
scratch buffer per thread and build each brick directly in its final form (uniform, else
palette). `Brick::restored` applies the same rule `collapse()` did, so the output is identical:
the terrain digest is `dc3d5d91…` before and after. Total 14.6 s to 7.7 s (fill 6.8 s to 7.1 s,
packing/release 7.3 s to 0.02 s), and no 64 KiB raw array per dense brick at peak. Worldgen is
compute-bound and still scales to 16 workers (4: 19.7 s, 8: 11.2 s, 12: 8.4 s, 16: 7.7 s).

**Collider setup: prepared in parallel, attached in order.** `ensure_terrain_brick_colliders`
planned 104,395 solid bricks serially (3.2 s) and then built and inserted each collider serially
(7.3 s, of which shape construction in `add_body` was 7.0 s). Planning, grid extraction and shape
construction are pure in the immutable terrain snapshot, so workers now prepare them
(`PreparedCollider`, new `PhysicsWorld::add_prepared_body`; `add_body` is a wrapper over the same
path) and one thread attaches them in canonical brick order, so handles and ids do not depend on
how the work was divided (`terrain_colliders_are_attached_in_canonical_brick_order`). Errors still
surface before the legacy body is retired. 12.7 s to 3.0 s. More workers is not better here: with
1, 4, 8, 12, 16 workers the build is 10.0, 4.7, 2.8, 2.5 and 4.4 s, and the labelling that follows
is slower after a wider build, so end to end (build + labels) 1 worker is 15.8 s, 8 is 9.1 s and
12 is 9.4 s. The rule is half the logical threads, at most 8.

**Label warming** now hands out batches dynamically (shared counter) instead of fixed
round-robin behind one-slot channels, and the cap is 16 workers. It helps at low worker counts
(4 workers 7.0 s to 5.6 s) but plateaus near 5.4 s (4.5 s with 16), limited by allocation.

**Measured** (stress fixture, passing; resets are `before_tick_ms` of ticks 4501 and 6001):

| | before | after (`eng114-gn2-run`) |
| --- | ---: | ---: |
| first reset | 25.7 s | 21.1 s |
| second reset | 29.5 s | 18.3 s |
| startup `world_and_collision` | 11.8 s | 3.3 s (generation excluded, as before) |
| reset `world_and_collision` (includes generation) | 15.8 to 16.0 s | 8.5 s and 7.1 s |
| reset `local_labels_and_hashes` | 4.9 to 5.2 s | 8.1 s and 7.2 s |

The reset's label warming is slower than standalone (5.4 s) because it runs while the server,
the water solver and two clients share the machine. A reset in isolation is about 19 s (7.7 s
generation, 3 to 3.4 s colliders, 5.5 s labels, plus water, vegetation and baselines), and 18 to
21 s in the fixture; it was 26 to 30 s.

**Memory.** Parallel collider preparation costs about 0.06 GiB at peak (4.66 vs 4.72 GiB in a
single startup). The fixture's server peak was 8,579,899,392 B and 8,561,823,744 B (7.99 and
7.97 GiB). That is inside the 7.86 to 7.99 GiB range of otherwise equivalent earlier runs (a run
before any of these changes also peaked at 7.99 GiB), so it is not attributable to this change,
but the margin under 8 GiB is a few tens of MiB at worst and remains a standing risk.

Checks: `cargo test --workspace` 1,299 passed, 0 failed, 73 ignored; fmt and scoped clippy clean.

### Chunked topology hash, `spall.topology.v2` (2026-10-08)

Decision (user): make the O(world) canonical hash incremental, accepting a protocol and
persistence break. The v1 hash was one BLAKE3 stream over every brick, so it could not be updated
without rehashing all of them (16 to 20 ms per commit on the tick thread, for a 196,608-brick
world).

**Definition** (normative text in [protocol.md](../protocol.md)): bricks are grouped into chunks of
8 x 8 x 8; each non-empty chunk has a digest (`spall.topology.chunk.v2`, its bricks in canonical
order); a volume hashes its id, cell size, owner and its chunk keys and digests in order
(`spall.topology.v2`). A change to one brick changes one chunk digest.

**Mechanism.**

* `spall_voxel`: `Volume` and `EvictedBricks` share one generic chunked, copy-on-write
  `ChunkStore` (the structure the volume had since the earlier clone work). Each chunk carries a
  stamp that every mutation replaces and clones share, so equal stamps mean identical contents.
  Cloning an evicted set also became cheap, which removes the per-edit evicted-digest clone.
* `spall_protocol`: the v2 definition (`canonical_topology_hash` is the reference),
  `chunk_digest_single_layer` (same bytes without per-brick allocation), `topology_hash_from_chunks`
  and `ChunkDigestCache`, a plain-data cache that recomputes only chunks whose stamp pair
  (resident, evicted) changed, and is atomic on error. The cache needs no report of what a
  mutation touched and is valid for any copy of a volume, so a commit's candidate volume uses the
  live cache.
* `spall_sim` adapts volume plus evicted digests to the cache (`refresh_logical_chunk_digests`),
  `SimWorld` keeps one cache per volume; the client replica reuses the same adapter for its world
  hash, its terrain-resident hash and the verification of every incoming transaction against its
  candidate world.

**Versioning.** `WIRE_SCHEMA_VERSION` 3 to 4 (every record header carries it, so mismatched
builds refuse each other at the first record rather than rejecting every transaction), and
`TOPOLOGY_HASH_VERSION` 1 to 2. **A save written under v1 is refused on recovery** with an
algorithm-version mismatch (`a_save_made_under_the_v1_topology_hash_is_refused_not_misread`). It is
not migrated: its recorded checkpoint and journal hashes use the old layout, and carrying the v1
algorithm plus a version-aware replay and an immediate re-checkpoint is a separate feature with no
real saves to protect yet. This is a deliberate limitation, not an oversight.

**Tests.** Three independent computations agree (reference over whole volumes; single-layer chunk
digests combined; general chunk digest), on volumes of 0 to 900 bricks spanning chunk boundaries
and negative coordinates; the cache recomputes only changed chunks, forgets vanished ones and is
unchanged by a failure; `spall_sim/tests/chunk_hash_cache.rs` runs 60 random cuts and evictions
with several mutations between hash calls and compares the cached world hash with the reference
every third step (it fails at step 3 if every stamp is made constant); a split that creates bodies
hashes the same cached or from scratch and matches its transaction's result hashes; the storage
chunk and the hash chunk are the same size; the stamp semantics of the chunk store. The pinned
digest of the protocol sample moved from `f608a180…` to `13da6535…`.

**Measured.** Ordinary warm edit, full-size world (`full_world_commit_profile`): `commit.result_hashes`
16 to 20 ms to **0.18 to 0.27 ms**; `commit.total` about 20 ms to **0.4 to 0.5 ms**. In the stress
fixture (`eng114-mk1-run`, passed, server and both clients end on the same v2 hash `514bafac…`, no
rejected transactions): commit 2.1 to 2.6 ms per edit (was 23 ms), ordinary edits **47 to 55 ms**
admission to commit (were 65 to 75 ms), the large collapse 302 ms, the cut behind it 146 ms, first
cut 211 ms (cold, not yet understood), peak working set 8,558,239,744 B (7.97 GiB).

**The shaped-link fixture now fails, and why** (`eng114-regional-1024m-impaired-deferred`,
`eng114-mk2-run`, `eng114-mk3-run`): the server and the fast client agree, nothing is rejected,
and the shaped client ends with either its catalogue still pending (mk3) or one repair
transaction still held (mk2). It passed twice before (`rz1`, `rz2`; 216 s and 205 s runs) and the
two failures are 189 s and 193 s runs. The fixture is scheduled in server ticks (7,200, with cuts
at ticks 6,300 and 6,420 after the second reset), while the shaped link (about 22 KB/s) has a fixed
byte budget: a 2.7 MB catalogue, three 125 KB baselines and a repair patch need roughly 165 to
180 s after the client joins. A server that no longer stalls (startup 28 to 12 s, resets 26 to 17 s,
steady 60 Hz) finishes the scripted ticks sooner and leaves the link too little time. This is
the same class of coupling the resumable catalogue exposed, not a hash defect, and it is proved
rather than inferred: a diagnostic copy of the fixture with `server_ticks = 9600` and nothing else
changed (`eng114-mk4-run`, 231 s, deleted afterwards) passes, with both clients on the server's
hash `514bafac…`, one repair applied, zero rejections. **The original fixture is unchanged and
currently fails at this server speed**; how to treat that is a decision for the owner, recorded
in the work log.

Checks: `cargo test --workspace` 1,316 passed, 0 failed, 73 ignored; fmt and scoped clippy clean.

### Run drain for regional clients (2026-10-08)

Decision (user): do not end a run while a regional client is still receiving what the server owes
it. `ServeConfig::drain_timeout` (default 300 s, `--drain-timeout-secs` on `sandbox-server`, 0
disables): once `max_ticks` is spent the server serves one more tick at a time while a regional
client is joining, has a catalogue queued or streaming, has a non-water record queued, or has
written a non-water record that no probe echo has yet confirmed. The run ends once nothing is owed
and no client has sent anything for 2 s (`DRAIN_GRACE`, covers a repair request answering the last
record on a 200 ms RTT link), or when the limit is reached, which is reported, never silent
(`drain_timed_out` in the server summary and the session summary and failure line). Water is
excluded because it is presentation-only and replaced by every newer frame. Fixtures and
thresholds are unchanged.

Measured: the unchanged `eng114-regional-1024m-impaired-deferred` (`eng114-mk5-run`) passes: server
and both clients on `514bafac1a18`, 0 rejected, 1 repair applied on the shaped client, 344 drain
ticks (about 6 s), not timed out, 221 s. Workspace builds, `spall_server`/`xtask`/`sandbox` tests
pass, clippy clean. Limit: one run; the drain fixes the clock coupling but a link too slow to
finish inside 300 s still fails, and reports it as a drain timeout.

### Item 4: multi-client, eight-client and sustained-load runs (2026-10-08)

New fixtures (same server workload, limits and exact-hash requirement as
`eng114-regional-1024m-impaired-deferred`); `join_budget.also_clients` in the xtask harness shapes
further clients with independent links and holds each to the same size and time ceilings.

| Fixture | Clients (shaped) | Result | Measured |
| --- | --- | --- | --- |
| `eng114-regional-1024m-multi-impaired-deferred` | 4 (1, 2, 3) | passed, all on `514bafac1a18`, 0 rejected | ready 17.0 to 18.5 s (bound 30 s), 104 drain ticks, 205 s, server peak 8.08 GiB |
| `eng114-regional-1024m-eight-impaired-deferred` | 8 (4 to 7) | passed, all on `514bafac1a18`, 0 rejected | ready 14.4 to 18.2 s, 54 drain ticks, 295 s, server peak 8.15 GiB |
| `eng114-regional-1024m-soak-2min-deferred` | 8 (4, 5), clients 0 and 1 move | **FAILED** (4 runs) | see below |

These are the full-world regional workload, not the G4 population (256 active / 4,096 sleeping
bodies), whole-foundation collapse, or the 30-minute soak.

**Sustained-load failure.** 1,212 scripted cuts (10 ordinary edits/s plus a 4 m blast every 10 s)
over 7,200 ticks. Static unshaped clients 2, 3, 6, 7 end on the server's hash; the two moving
clients (51 and 48 rejected) and the two shaped clients (19 each) do not. All rejections are
`result hash mismatch` on the terrain volume; a from-scratch rehash of the same candidate equals
the cached one ("cache stale: false"), so the v2 hash cache is not the cause. Shaped clients show
20 to 34 transactions held for repair patches when the first rejections occur, in a burst right
after a patch is applied. Hypothesis, not proven: a retried held transaction is replayed against a
volume where a repair patch has already advanced other bricks to the server's current state, so
the volume-wide result hash cannot match, and the rejected edit's own bricks are never re-fetched.
Server single-brick commit p95 under this load was 134.7 ms (47 to 55 ms in the 8-cut fixtures).
Whether the defect predates this work is **unknown**: the same fixture on a HEAD worktree did not
finish inside the harness deadline (no result). The 30-minute variant
(`eng114-regional-1024m-soak-30min-deferred`) was generated but not run. GPU gate: unrun.

### Item 4 follow-up: sustained-load soak diagnosis (2026-10-08)

*HEAD comparison.* The same fixture on a HEAD worktree did not finish in two attempts (30 and 50
minute harness limits; the server was still ticking), so there is **no pre-existing-versus-new
verdict**; HEAD cannot complete this load at all.

*Fix made (kept).* A transaction rejected for `result hash mismatch` had its own edit lost for
good. The declared hash covers the whole volume, so a repair patch that moved an untouched brick
to a later server state makes an older held transaction unverifiable. The replica now (when the
cached hash equals a from-scratch rehash, i.e. the difference is real state, not a stale digest)
requests the bricks the transaction touched and holds it; the server's current state already
includes the transaction, so the retry recognises it as incorporated. Logged at WARN; the final
hash comparison remains the arbiter. Test:
`a_hash_mismatch_caused_by_a_patched_ahead_brick_repairs_instead_of_dropping`. Effect on the
2-minute soak (`eng114-soak2-5`): rejected transactions 19 to 51 per client before, **0** after.

*Still failing, with a different cause* (`eng114-regional-1024m-soak-2min-deferred`, still red):
* The two moving clients are **dropped by the server** ("client reliable backlog bound exceeded",
  2,048 queued repair-patch messages, 1.3 MB) at server ticks 9,336 and 5,400 of 15,160. The
  movers send 22,575 reload requests and evict 10,797 bricks in two minutes; the server throttles
  609 repairs. Their remaining hash difference is a consequence of being disconnected.
* The two shaped clients stay connected but their catalogue completes only after 148 s (the link's
  budget is also carrying 10 transactions/s plus repairs), after which 1,096 deferred transactions
  replay; they finish with 0 held, 0 rejected and a different hash. Where those edits are lost is
  not yet found; per-brick comparison tooling does not exist.
* Static unshaped clients (2, 3, 6, 7) match the server.
* This workload may exceed what the specified shaped link and per-client repair budget can carry;
  that is a capacity question for the owner, not yet a proven defect. Single-brick commit p95
  under it is 134.7 ms.

### Item 4 (continued): light soaks, and the 32 ms/tick that was hiding behind them (2026-10-08)

**Fixtures** (same server workload, world, resets rule and exact-hash requirement; positions drawn
from a fixed LCG, seed 0xE114, over x,z in [600,3500) at y = 16, round-robin over all clients):

| Fixture | Edits | Links | Movers | Result |
| --- | --- | --- | --- | --- |
| `eng114-regional-1024m-soak-light-2min-deferred` | 2/s + 4 m blast per 10 s, 252 | unshaped | client 0 | **passed**, all 8 on `3392c553cd47` |
| `eng114-regional-1024m-soak-light-impaired-2min-deferred` | 20 + 12 blasts (0.27/s) | clients 4, 5 shaped | client 0 | **passed**, all 8 on `baa9d0631c5a` (3,920 drain ticks) |
| `eng114-regional-1024m-soak-light-30min-deferred` | 3,780 edits, resets at ticks 36,000 and 72,000 | unshaped | client 0 | **convergence passed, session failed** (below) |
| `...-soak-2min-deferred`, `...-soak-30min-deferred` | 10/s + blast per 10 s | 2 shaped | 2 | heavy overload variants; 2-min still **red** (movers dropped by the server on reliable backlog; shaped clients cannot fetch ~33 KB dense bricks for 1,212 distant edits at ~22 KB/s). Not expected to pass; kept as the recorded overload case. |

**Capacity finding.** A shaped regional client (about 22 KB/s goodput on the 5% loss, 200 ms RTT
link) resolves roughly 0.3 distant edits per second, because each edit to a brick it holds only as a
digest costs a repair patch of a dense brick. At 2 edits/s it ends 170 to 190 transactions behind
after a 300 s drain. The lasting fix is protocol-level (carry the post-edit content hash for
evicted bricks so no geometry is fetched), not done.

**Two real replica defects found and fixed** (found with a new brick-listing diff,
`SPALL_DUMP_LOGICAL_BRICKS=<prefix>`, which writes `x y z revision hash` per logical terrain brick
for server and clients):
1. *Silent loss of held transactions.* `max_pending_repair_txns` was 64 and the oldest hold was
   evicted past it. Three clients each evicted 194 to 208 holds in the 2-minute light soak and
   ended with 48 to 323 terrain bricks at an older revision than the server (revision 0 against
   ~1,400), with 0 held and 0 rejected: no error anywhere. Now 4,096, and past it the replica
   rejects loudly. Test `the_pending_repair_hold_is_bounded`.
2. *Hash mismatch dropped the edit* (see the preceding section). The from-scratch rehash I used to
   classify it cost a whole-world hash per retry of every held transaction; removed.

**Server tick cost, the larger finding.** The sustained fixtures ran at 14 to 27 Hz, not 60. With the
server's own per-phase accounting (new `tick pacing` fields, every 600 ticks; spans `serve.*`,
`players.*`, `water.*`), eight connected players and only 2 edits/s:

| Phase (mean per tick) | Before | After |
| --- | --- | --- |
| `tick.players` (simulation) | 32.6 ms | 0.65 ms |
| `serve.water_fanout` | 24 to 27 ms | 3.4 ms |
| tick work, p95 (`tick_busy`, 8,000 ticks) | 122.6 to 131 ms | **23.0 ms** |
| achieved tick rate | 14 to 17 Hz | **57 Hz** |

Causes and fixes: (a) every character sweep built a list of all terrain collider handles
(hundreds of thousands) and tested each candidate against it linearly, for every player and sweep:
now a prebuilt `ColliderExclusion` hash set, cached until terrain colliders change
(`PhysicsWorld::sweep_character_pushing_excluding_set`); (b) water: per client, every fourth tick,
the server cloned the whole frame, encoded the full keyframe and diffed it against that client's
base: now one shared frame, one full encoding and one delta per base sequence for all clients
(`water_replication::SharedFrame`); (c) every committed terrain edit rebuilt every player's
collision window regardless of distance: a window is now confirmed current when every change since
it was built lies outside it (`changes_miss_window`, test `window_invalidation.rs`; no measurable
effect on this fixture but it removes a ~100 ms rebuild per player per edit).
G4 targets (tick p95 12 ms, p99 16.7 ms) are still **not met**: 23.0 / 37.8 ms on this workload.

**30-minute light soak** (`eng114-soak-light-30min-b`, 2,137 s wall clock, 111,721 ticks, two world
resets, 8 clients, 3,780 scheduled edits): all eight clients and the server end on
`ca7d4596385a`; 0 rejected, 0 held; 4,182 transactions committed; server peak working set 8.83 GiB
(about 5.5 GiB steady, 6.1 to 6.4 GiB after the first reset and flat after the second, 8.1 to
8.8 GiB transient during each reset; no growth trend visible over the run). **The session still
fails its own requirement** that every scripted edit commits: the clients sent 3,780 actions, the
server saw 3,679, and `scripted_actions_committed` is 459 to 461 per client of 472 to 473. About
100 actions are unaccounted for (no rejection, expiry or throttle is counted) and are the open item;
the two resets are the obvious suspect and are untested as the cause. Commit p95 277 ms,
large-collapse p95 898 ms (2 samples). Single run.

Checks: `cargo test --workspace` 1,321 passed, 0 failed, 73 ignored; clippy
(`-D warnings`, same two allowances as before) clean. Not run: GPU gate, portable rebuild, Windows
sleep-free confirmation of variance (every result here is one run).

### Item 4 (continued): the ~100 lost edits, and a passing 30-minute light soak (2026-10-09)

**Cause.** Every connection reader forwarded actions, repairs, progression and admin requests to
the sim loop over one bounded bridge (4,096) with `try_send`, silently dropping on full. A world
reset stalls the sim loop for 17 to 48 s; meanwhile the moving client re-requests terrain bricks by
the thousand, the bridge fills, and the players' edits are dropped with nothing counted (no
rejection, expiry or throttle). Reproduced with a 2-minute fixture and one reset at tick 6000
(`eng114-regional-1024m-soak-light-reset-2min-deferred`): **218 actions and 1,502 repairs dropped**,
27 to 28 of 31 to 32 scripted edits committed per client.

**Fix and instrumentation.**
* Repairs have their own bridge (`repair_tx`/`repair_rx`); the sim loop drains edits, input and
  joins first. After the change: 0 actions dropped, 32/32 and 31/31 scripted edits committed per
  client, all eight clients on the server's hash. Repairs are still dropped under a flood
  (1,531; they are re-requested), now visibly.
* A dropped action is answered with a retryable "overloaded" rejection instead of silence.
* New server summary fields `inbound_dropped_actions/_repairs/_progression/_admin`, and a WARN on
  the first drop of each kind and every hundredth.
* **Harness rule changed (flagged):** the worldgen admission check demanded
  `actions_rejected == 0`. After a reset stall the clients' queued actions arrive in one burst and
  30 to 47 are refused "throttled: retry shortly", retried, and committed. The check now ignores
  throttled refusals (`admission_clean`), still fails on any other rejection, any dropped action,
  and any scripted cut that is not staged and committed (the commit-receipt check is unchanged).
  Test `throttled_retries_are_clean_but_dropped_or_refused_actions_are_not`.

**30-minute light soak, passing** (`eng114-soak-light-30min-c`, 2,106 s wall clock, 111,723 ticks,
two world resets, 8 clients, one mover): server and all eight clients end on `ff43fd3edf9d`; 3,780
of 3,780 scheduled edits staged and committed (4,283 transactions); 47 throttled refusals, all
retried; 0 actions dropped; 0 rejected or held at any client; server peak working set 8.82 GiB
(same shape as before: steady about 5.5 GiB, higher and flat after each reset, transient 8.1 to
8.8 GiB during resets), client 0 (the mover) 2.6 GiB, others about 0.16 GiB. Single-brick commit
p95 2,433 ms and large-collapse samples are dominated by edits queued behind the reset stalls and
are **not** a latency result. Checks: `cargo test --workspace` 1,322 passed, 0 failed, 73 ignored;
clippy (same two allowances) clean.

Not met or not run: G4 tick targets (p95 12 ms, p99 16.7 ms; measured 23.0 / 37.8 ms on the
2-minute workload before this change, not re-measured); the reset stall itself (17 to 48 s);
the +0.8 GiB held after the first reset (unexplained); the GPU gate; run-to-run variance (one
run of each).

### World reset stall: where the time goes, and the first cut (2026-10-09)

A reset (an admin command, development only) rebuilds the whole world inline on the tick thread
and sends every client a fresh baseline; see `serve.rs`, `AdminCommand::ResetWorld`. The server
now logs a per-phase breakdown (`admin world reset`: `built_ms`, `basis_ms`, `replaced_ms`,
`elapsed_ms`; `world reset baselines`: `snapshot_ms`, `transfers_ms`).

Measured on the 8-client, 4096-cell fixture (`eng114-reset3-c`, first reset, one run):

| Phase | Time |
| --- | ---: |
| build the new world (`startup.world_and_collision` 23.0 s, `local_labels_and_hashes` 25.7 s, vegetation 1.4 s, water 0.5 s) | 50.7 s |
| catalogue basis | 0.05 s |
| `replace_world`: **dropping the old world on the tick thread** | **9.0 s** |
| baselines for 8 clients (snapshot 0.08 s, transfers 1.9 s) | 2.0 s |
| total | 61.8 s |

**Change:** `Simulation::replace_world` now hands the replaced world, pipeline, water and
vegetation to a background thread to free (`Retiring`); a panic while dropping is re-raised on the
owner at the next retirement. Tests: `a_retired_value_is_dropped_off_the_calling_thread_and_finish_waits_for_it`,
`a_panic_while_dropping_is_re_raised_on_the_owner`.

After (`eng114-reset4-d`, same fixture, one run): `replaced_ms` 0.0, total **38.5 s**
(build 36.1 s: `world_and_collision` 18.3 s, labels 16.3 s, vegetation 1.3 s; baselines 2.2 s).
The session passes outright: all eight clients on the server's hash, 32/32 and 31/31 scripted edits
committed, 0 dropped. Checks: `cargo test --workspace` 1,324 passed, 0 failed, 73 ignored; clippy
clean.

**What remains is the build itself, and it is 2 to 3 times slower than the same stages at
startup** (generation plus collider setup 11 s standalone vs 18 to 23 s; label warming 5.4 s vs 16
to 26 s), consistent with the 17 to 21 s measured with two clients. The cause is not established:
CPU contention from eight client processes and the server's own tasks, allocator behaviour on a
heap that already holds a world, or both. Resets differ by 20 s run to run (36 to 51 s) on the same
code, so only the removed 9 s is a clean result.

**Options for the rest** (not done): (1) build the new world on a background thread while the old
one keeps ticking and swap at a tick boundary: removes the stall from the clients' view, costs
peak memory (both worlds resident, which already happens during a reset) and changes reset
semantics (edits made during the build are discarded at the swap); an earlier attempt was rejected
for the scenario-meaning change and is documented above. (2) Find the contention first (run a reset
with no clients, then with clients idle) before deciding.

### World reset stall: the build time scales with the number of client processes (2026-10-09)

Isolation runs of the same fixture with a reset at tick 6000 (one run each; `elapsed_ms` of the
`admin world reset` log, old-world drop already off-thread):

| Clients (all on this machine) | Reset total | Notes |
| ---: | ---: | --- |
| 1 | **15.4 s** | build 15.1 s |
| 2 (earlier stress fixture) | 17 to 21 s | |
| 4 | 25.2 s | world_and_collision 13.0 s, labels 10.4 s |
| 8 | 38.5 s, 41.2 s (and 61.8 s before the off-thread drop) | world_and_collision 18.3 s, labels 16.3 s |

`tools/measure-worldgen-stress.ps1` now also records each process's CPU time (`cpu_ms` column of
`process-memory.csv`). In the 8-client run every client process uses about **0.7 of a core
continuously**, before, during and after the reset (client 0, the mover, 0.54 to 0.62), about
5.6 cores for the eight, while the server uses 5.4 to 6.0 cores at 57 Hz. The reset's build wants
about eight more cores for its workers, on a machine with 8 cores / 16 threads, so each added client
process takes build throughput away. This is a property of running every client on the server's
machine, not of the reset code: with one client the reset is 15 s, which is close to startup
generation plus colliders plus labels (11 s + 5 s measured standalone).

Conclusions: (1) the remaining reset stall on a dedicated server is about 15 s and is the
single-threaded-tick-thread build itself; (2) the 8-client fixtures overstate it by roughly
2.5 times; (3) a headless client costing 0.7 cores while idle is its own question (probably frame
decoding and its prediction loop) and bears on how many clients one test machine can host;
(4) building the new world off the tick thread (option 2 above) would remove the stall entirely, at
the semantic and memory cost already described, and is now the only remaining lever on the stall
itself. Not done.

### Idle-client CPU: a probe storm, and what it hid (2026-10-09)

**Symptom.** Every headless client process used about 0.7 to 0.85 of a core while idle, and the
server 4 to 6 cores with eight clients; this is what stretched world resets from 15 s (one
client) to 38 to 62 s (eight).

**Elimination.** Per-thread sampling showed the CPU spread over four or five async threads, not one
loop. A busy-time profile of the client's datagram handler (0.5 ms per 5 s) and skipping water,
vegetation and motion handling entirely (0.78 core either way) ruled out the record handlers.
Logging the connection's receive counters showed **25,000 reliable records a second** arriving at an
idle client: the control reader's own profile reported them as "other", 120,000 to 137,000 per
5 s, at 0.002 ms each.

**Cause.** `serve_conn`'s writer loop sent a `ControlProbe` after *every* wake-up, and receiving an
echo wakes the same loop (`probe_returned` notifies it), so each probe's echo caused the next
probe, and the 250 ms idle timer added a new chain every time it fired. The chains never ended.
`PROBE_INTERVAL` was documented as the probe period but was only the idle timer.

**Fix.** At most one probe per `PROBE_MIN_GAP` (25 ms). Test
`a_control_probe_is_sent_at_most_once_per_minimum_gap`; `docs/protocol.md` corrected. On a fast
link an echo returns inside the gap and the chain ends (probes then follow the 250 ms timer); on a
slow link the chain continues at one probe per round trip.

**Measured** (4-client fixture, steady state, CPU cores per process): idle clients 0.84, 0.85,
0.84 to **0.01, 0.00, 0.01**; the mover 0.42 to 0.30; server 4.12 to **1.66**. With eight clients
the reset takes **14.5 to 16.2 s** (was 38.5 to 41.2 s; 61.8 s before the off-thread drop), in line
with the 15 s of the one-client run. Original `eng114-regional-1024m-impaired-deferred` passes
(219.5 s, two resets of 15.1 s).

**What the storm had been hiding.** With probes at a sane rate the congestion check can be
trusted, and the shaped-link fixtures that passed at the old 22 Hz server rate now fail at the
57 Hz rate this session's other fixes produced, **regardless of the probe change** (the same
failure with the storm restored, `probe0-*`):
`eng114-regional-1024m-eight-impaired-deferred` (shaped clients 4 to 7 end 11 to 20 transactions
behind after the 300 s drain) and `eng114-regional-1024m-soak-light-impaired-2min-deferred`
(shaped 4, 5: 93 and 121 behind). A shaped client received about 270 motion datagrams and 29 KB/s
of control traffic per second against a link of about 22 KB/s: the full-broadcast motion feed of
44 bodies at 20 Hz fills the link and starves the reliable catalogue and repairs. Per wall second a
faster server sends proportionally more. Supporting evidence: the same light-impaired workload with
the production per-client motion budget (`motion_interest`, 600 bytes per batch; new fixture
`eng114-regional-1024m-soak-light-impaired-motion-2min-deferred`) **passes** (454 s, drain 228 s,
all eight on `4cc8fad052c8`). The two original fixtures are left unchanged and remain red at the
current server speed; the fix for them is a motion budget in the fixture or a lower fixture
workload, an owner decision.

**Also seen, not understood:** in one 8-client reset run, one of 252 scripted edits did not commit
(client 4: 31 sent, 30 committed, 10 throttled retries in the run; the other three reset runs
committed all). Single occurrence.

Checks: `cargo test --workspace` 1,325 passed, 0 failed, 73 ignored; clippy clean.

### Motion budget added to the shaped-link fixtures (2026-10-09)

Decision (user): give the failing shaped-link fixtures the production per-client motion budget.
`motion_interest` (`near_m` 48, `far_m` 96, `far_interval` 4, `client_budget_bytes` 600 per 20 Hz
batch) is now part of `eng114-regional-1024m-eight-impaired-deferred` and
`eng114-regional-1024m-soak-light-impaired-2min-deferred`; their descriptions say why. Nothing else
in them changed. The separate `...-impaired-motion-...` experiment fixture was removed.

Results (one run each, current server, probe change in):

| Fixture | Result | Notes |
| --- | --- | --- |
| `eng114-regional-1024m-eight-impaired-deferred` | passed, 8 clients on `514bafac1a18` | 193.7 s (was 295 s), resets 15.7 s and 14.0 s, 121 drain ticks |
| `eng114-regional-1024m-soak-light-impaired-2min-deferred` | passed, 8 clients on `4cc8fad052c8` | 393 s, 10,769 drain ticks (180 s) |
| `eng114-regional-1024m-multi-impaired-deferred` (4 clients, unchanged) | passed, 4 clients on `514bafac1a18` | 204.5 s, resets 14.5 s and 14.7 s |
| `eng114-regional-1024m-impaired-deferred` (earlier, unchanged) | passed | 219.5 s |

So the fixtures that failed at the faster server rate pass once the slow clients' motion is bounded,
which supports the account above (full-broadcast motion starving a ~22 KB/s link) without proving
it by itself.

### The lost edit: in-flight edits discarded by a world reset (2026-10-09)

In one 8-client reset run, client 4 sent 31 scripted edits and received 30 commit receipts, while
the server had staged all 252 and the world hashes agreed everywhere. Cause: an edit accepted but
not yet committed when `replace_world` ran is staged in the replaced world's pipeline, which is
dropped with it, so it can never commit; it was also the one `submitted_by` entry left, which the
reset cleared silently. The client waited for a receipt that could not come.

**Fix.** At a reset the server now answers every unresolved edit with a retryable rejection
(`overloaded: world reset, retry`), counts them (`actions_discarded_by_reset` in the summary), and
the client's existing bounded retry resends them to the new world. The harness's admission check
subtracts them like throttled retries (`admission_clean`, test extended).

**Verified** by running the same 8-client reset fixture three times (one run each, same build):
all three passed; every client committed every scripted edit (32/32 on clients 0 to 3, 31/31 on
4 to 7). In run 2 exactly one edit was in flight at the reset: `actions_discarded_by_reset` 1,
`actions_staged` 253, and it committed after the retry. Runs 1 and 3 had none in flight (0). So
the cause is confirmed, not only inferred. Checks: xtask tests 39 passed; clippy clean.

### G4 tick targets, post-reset memory and the largest-world GPU run (2026-10-09)

**Server tick against the G4 targets** (`eng114-regional-1024m-soak-light-2min-timed-deferred`:
8 clients, one moving, 2 edits/s, 8,000 measured ticks after 1,800 warmup; one run each):

| | before this session's tick work | now (two runs) | target |
| --- | ---: | ---: | ---: |
| tick busy p95 | 122.6 to 131 ms | **12.9 / 12.7 ms** | 12 ms |
| tick busy p99 | 144.9 to 157 ms | **20.3 / 20.0 ms** | 16.7 ms |
| tick busy max | 298 ms | 105 / 100 ms | n/a |
| physics p95 | 2.2 ms | 1.2 ms | 6 ms |
| achieved rate | 14 to 17 Hz | 58 to 59 Hz | 60 Hz |

Not met, but close: p95 by 0.7 to 0.9 ms, p99 by 3.3 to 3.6 ms. With `SPALL_SLOW_PHASE_MS=6` (new
env var: log every tick with a phase of at least that many ms) the tail is made of two things:
water delta computation on the every-fourth tick (408 ticks over 6 ms, mean 6.8 ms; making the
comparison allocation-free changed nothing, so the time is in compressing changed bricks) and the
mover's collision-window rebuilds (294 ticks, mean 9.6 ms, one per about 35 ticks while walking).
Both could leave the tick thread (delta encoding on a worker; window prebuilding), neither done. A
change that was made: `water_deltas` compares bricks in place (test
`the_in_place_comparison_agrees_with_comparing_extracted_bricks`).

**Memory after a reset** (30-minute light soak, server `private_bytes`): about 5.65 GiB before the
first reset, 6.5 to 6.7 GiB after it, and flat after the second (6.7 GiB). It is a one-time
increase of about 1 GiB that does not repeat, so not a leak of a whole world per reset; the rest is
undiagnosed (candidates: heap fragmentation from building a world on a used heap, caches that
startup does not populate). Working set shows the same step.

**Largest-world GPU run** (`xtask play --worldgen showcase --seed 1 --worldgen-size 4096 --release
--uncapped` with a scripted walking tour, one local client, RTX 4080 SUPER, 1280 x 720 window, 212
live report windows of the client's own frame counters; evidence in `.local/runs/gpu-a`): median
273 fps (p10 228, minimum window 172); frame time average median 3.8 ms (p90 4.3 ms, worst window
11.2 ms); the longest single frame in each window has median 5.6 ms, p90 22 ms, worst 26.8 ms. The
HUD's GPU figure (0.004 ms) is the overlay only, not the scene. Screenshots of the tour are in
`.local/runs/gpu-a/shots`; the terrain, trees and sky render correctly. This is **not** the G2
gate (1080p, GPU timestamps, lighting scenes, moving frames; see `docs/reports/G2.md`), it has no
edits or destruction in view, and the server shares the machine. It shows the full-size world
renders at well over 60 fps on this adapter, with occasional 20 to 27 ms frames not yet attributed.

### Run-to-run variance (2026-10-09, current build, probe fix and motion budgets in)

| Fixture | Runs | Result | Elapsed | Resets | Server peak |
| --- | --- | --- | --- | --- | --- |
| `eng114-regional-1024m-eight-impaired-deferred` | 3 (this and the two earlier) | all pass, `514bafac1a18` | 193.7, 193, 193 s | 14.0 to 15.8 s | 8.12, 8.15 GiB |
| `eng114-regional-1024m-impaired-deferred` | 3 (incl. the 219.5 s one) | all pass, `514bafac1a18` | 219.5, 199, 194 s | 12.8 to 13.2 s | 8.02, 8.03 GiB |
| `eng114-regional-1024m-stress-deferred` | 1 | pass, `514bafac1a18` | 194 s | 17.1, 13.7 s | 8.04 GiB |
| `eng114-regional-1024m-soak-light-impaired-2min-deferred` | 3 | all pass | 393, 477 s (and 454 s experiment) | none | 5.46 GiB |

The deterministic fixtures (eight scripted cuts, same final hash every run) are tight: elapsed times
within 1 s for eight-impaired, resets within 1 s. The sustained fixtures end on a different hash
each run (edit ordering, physics timing; the earlier light soaks ended on `3392c553cd47`,
`baa9d0631c5a`, `ff43fd3edf9d` and now `8e704fc54023`), which is expected; what is held to
the same standard every time is that the server and all clients agree. Their elapsed time is looser
(393 to 477 s, with 180 to 243 s of drain) because the shaped link, not the server, sets it.

**Memory margin.** The full-size runs with eight scripted cuts peak at 8.02 to 8.15 GiB working
set, the transient during resets. The G4 text gives 8 GiB as the server memory target, so those runs
are 0.02 to 0.15 GiB over it at the reset peak (the targets are not configured in these fixtures).
Steady state is about 5.5 GiB. Peak memory remains a real constraint on the machine, not a
measured pass.

### The heavy soak (10 edits/s), last (2026-10-09)

Decision (user): leave the heavy soak for last. State at the start: the 2-minute heavy variant was
red because the server dropped the walking clients on their reliable backlog and the shaped clients
could not keep up.

**Findings and changes, in order**

1. *The 2-minute heavy variant with unshaped links passes* once the earlier fixes were in (probe
   storm, tick rate, separate repair bridge, hold bound): 1,212 of 1,212 edits committed, all
   eight clients on `cef80aac3456`, 0 dropped, movers 0 and 1 no longer disconnected.
2. *The shaped clients cannot be asked to follow 10 edits/s.* With `join_budget` on clients 4 and 5
   they ended 1,289 and 1,324 transactions behind: each distant edit costs a ~33 KB dense-brick
   repair over a ~22 KB/s link (about 0.3 edits per second sustainable). The two heavy fixtures are
   therefore unshaped; impaired links are covered by the lighter fixtures. Stated in their
   descriptions.
3. *The 30-minute run could not start*: 18,180 `--cut` arguments exceeded the Windows command line
   (os error 206). `xtask` now writes a `--cuts-file` for any client with more than 64 scripted cuts
   (`cuts_file_entries`, test `scripted_cuts_go_to_a_file_in_the_shape_the_client_reads`).
4. *About 14 edits per client were lost after resets*: queued actions arrive in one tick after a
   reset and exceed the per-tick action quota; the client's four fixed 40 ms retries ran out with
   some still refused. Throttle retries now back off (40, 80, 160, 320 ms, then 320 ms) over eight
   tries with up to 40 ms of per-request jitter (`retry_backoff`, test
   `throttle_retries_back_off_with_per_request_jitter_and_are_bounded`). Result: 18,180 of 18,180
   edits committed on every client in the following runs.
5. *The two walking clients overflowed.* At full speed a regional client walking over the dense
   terrain asks for about 170 brick patches a second (110/s averaged over the run); its control
   reader was 100% busy (about 5.6 ms per patch, 760 to 890 patches per 5 s) and with edits on top
   the held-transaction bound (4,096) broke, 5,299 and 3,991 transactions rejected, or the server
   dropped the client for exceeding its reliable backlog ("reliable backlog exceeded"). I tried
   asking for a held transaction's evicted bricks immediately instead of in turn: it made this
   worse (the walkers were dropped within three minutes), so it was reverted. What stays is the
   hold-bound behaviour above. The fix for the walkers themselves would be cheaper patch
   application on the client, not done.
   The heavy fixtures' two walkers now move at 30 percent of full speed (`movement` 0.3), stated in
   their descriptions: this is a change to my own soak fixtures, not to the original ENG-114
   fixtures, whose walker is unchanged.

**30-minute heavy soak, passing** (`eng114-regional-1024m-soak-30min-deferred`, run
`eng114-heavy30-e`, one run): 8 clients, 2 walking at 30 percent speed, 18,000 ordinary edits (10 per
second) plus 180 blasts of 4 m diameter (one per 10 s), world resets at ticks 36,000 and 72,000;
2,005 s wall clock, 111,723 server ticks. The server and all eight clients end on
`b04ab50bc9bf`; 18,181 edits staged and every client committed every one of its scripted edits
(2,273, 2,273 and 2,272 per client); 842 throttled refusals and 1 edit in flight at a reset, all
retried; 0 dropped actions; 0 held or rejected at any client. Server peak working set 8.43 GiB
(the transient during resets; steady state about 5.5 to 5.8 GiB), commit p95 100.6 ms (includes
edits queued behind the two reset stalls, so it is not a latency result). The 2-minute heavy variants
with and without a reset pass on the same code.

Checks: `cargo test --workspace` 1,327 passed, 0 failed, 73 ignored; clippy (same two allowances)
clean. Not met by this run: the G4 tick targets (the run was not timing-instrumented; the
2-minute timed workload measured p95 12.7 ms, p99 20.0 ms), the 8 GiB memory target at the reset
transient, and full-speed walkers.

### Repair patch throughput: the server waited for an acknowledgement per patch (2026-10-09)

**Measurement.** The control reader of a full-speed walking client (light soak, one walker) now
reports its time by phase (`phase patch.receive`, `patch.lock_and_apply`, `patch.retry_held` in the
`control reader profile` line). Over 5 s windows it handled 430 to 550 repair patches and was busy
99.9% of the time: **98% of that in `patch.receive`** (about 10 ms a patch), applying a patch
0.2 ms and retrying held transactions about 0. The client was not slow; it was waiting for the
server.

**Cause.** `send_baseline_phases` finished each bulk stream with `finish_and_wait`, which returns
when the peer has acknowledged all stream bytes and the FIN. The writer sends one transfer at a time,
so every repair patch cost one delayed-acknowledgement interval (about 10 ms on loopback) and the
whole connection's repair throughput was capped near 170 patches a second whatever the client could
apply, which is what the walkers needed at full speed.

**Change.** A one-brick repair patch or split blob (world version 1) is sent mid-session on a live
connection, which delivers it without the writer waiting; only the endpoint closing could lose
one. They now `finish()` without waiting. Baselines, resets and streamed worlds still wait. (No new
unit test: the behaviour is the absence of a wait; the measurement below and the fixtures are the
evidence.)

**Measured** (same fixture and walker, one run each): per-patch receive 10 ms to 0.1 ms; 715 patches
per 5 s now cost the control reader 170 to 240 ms of 5,000 (about 4% busy, was 100%); the walker
completed 22,105 of 22,137 reload requests (99.8%) against 16,549 of 16,821 before.

**Consequence for the heavy soak.** The 30 percent walking speed introduced for the heavy fixtures
is removed (full speed restored, descriptions updated), and both heavy variants pass with full-speed
walkers:
* `eng114-regional-1024m-soak-reset-2min-deferred`: all eight clients on `9cd46fa25d21`, every
  edit committed, 1 edit in flight at the reset retried.
* `eng114-regional-1024m-soak-30min-deferred` (`eng114-heavy30-f`): 2,003 s, 111,720 ticks, two
  resets, server and all eight clients on `b0b44271881d`; 18,180 of 18,180 edits staged and every
  client committed all of its own (2,273/2,273/2,272); 804 throttled refusals retried; 0 dropped,
  held or rejected; server peak 8.43 GiB (reset transient); commit p95 91.8 ms (includes reset
  stalls).

**Shaped links after the change** (one run each; the ack wait had also been pacing patches on slow
links): `eng114-regional-1024m-eight-impaired-deferred` 213 s, `soak-light-impaired-2min` 388 s,
`multi-impaired-deferred` 234 s, `impaired-deferred` 231 s: all pass with every client on the
server's hash. Drain after the scripted ticks is longer than in the earlier runs (2,470 to 2,657
ticks against 122 to 828 for the two small fixtures), so slow clients now take a little longer to
finish, the cost of patches no longer being paced.

Checks: `cargo test --workspace` 1,327 passed, 0 failed, 73 ignored; clippy clean.

### Shaped clients under the heavy edit rate: three limits, none of them the protocol (2026-10-10)

The earlier conclusion was that a shaped regional client can follow only about 0.3 distant edits a
second (a dense brick per edit over a 22 KB/s link) and that a post-edit content hash for evicted
bricks was the lasting fix. Measured again after the ack-wait change, the number was wrong:
`eng114-regional-1024m-soak-impaired-2min-deferred` (new: the 10 edits/s load with clients 4 and 5
shaped as in the other impaired fixtures, plus the motion budget) ended with the shaped clients
1,180 and 1,176 transactions behind but with **378 and 447** patches applied in 330 s: about
1.2 edits a second, which is serial round trips, not bandwidth (the link would carry several
patches a second).

**1. One repair per round trip.** A transaction held behind an earlier held one waited for its
turn before asking for its evicted bricks, so repairs were serial: one per round trip plus queueing.
A held transaction now asks for its evicted bricks at once
(`evicted_brick_requests`; test `a_transaction_held_behind_another_requests_its_evicted_bricks_immediately`).
Patches applied rose to 1,224 and 1,256 (three times), but:

**2. Duplicate requests.** With replies taking seconds on the slow link, the fixed 30-tick re-request
interval asked for the same brick over and over: 27,180 requests sent for 1,224 patches, 45,102
refused by the server's per-tick cap. An unanswered request now repeats at doubling intervals
(30, 60, 120, 240, then 480 ticks; test `an_unanswered_repair_request_backs_off_exponentially`).
Requests fell to about 15,000 and patches applied rose slightly, but:

**3. An unbounded patch queue.** The server answered every request that got past the per-tick
cap, so duplicates and slow draining built a backlog that passed `MAX_RELIABLE_BACKLOG` and the
server ended the shaped clients' connections (two "backlog bound exceeded" lines, clients stopped at
tick 10,740 and 10,545 of 15,734). The server now answers a repair only while the client has fewer
than `MAX_QUEUED_REPAIRS_PER_CLIENT` (96) patches queued and unwritten; the rest are left for the
client's backoff, and are counted (`repairs_deferred_queue_full`).

**Result** (one run, `heavyimp-d`): both shaped clients end with 0 held transactions on the server's
hash `2005a08bd029`; all eight clients converge; every scripted edit committed; 0 dropped;
`repairs_deferred_queue_full` 4,144; 1,754 and 1,798 patches applied; the run drained for 243 s after
the scripted ticks. The post-edit-hash protocol change is **not needed** for this load.

**Regression, same build** (one run each): the four shaped fixtures pass, with shorter drains than
before the change (`eng114-regional-1024m-soak-light-impaired-2min-deferred` 214 s, was 388 to 477 s,
drain 122 ticks, was 10,500 to 14,500; `eight-impaired-deferred` 193 s, drain 187 ticks;
`multi-impaired-deferred` 253 s; `impaired-deferred` 230 s). `multi-impaired-deferred` ended on a
different hash (`b12b712671ce`, earlier runs `514bafac1a18`) with all four clients agreeing: the
shaped clients' edits now commit at different ticks, which moves detached bodies; the fixture asserts
agreement, not that value. The unshaped heavy soaks pass again: `soak-reset-2min` (`2c43c1d2f5cc`)
and the 30-minute run `eng114-heavy30-g` (2,021 s, 111,724 ticks, two resets, all eight clients on
`9eddee6a8854`, 18,180 of 18,180 edits committed, 899 throttled retried, 0 dropped, 8.41 GiB reset
peak, commit p95 151 ms including reset stalls).

Checks: `cargo test --workspace` 1,329 passed, 0 failed, 73 ignored; clippy clean.

### G4 tick targets met on the light soak workload (2026-10-10)

Three tail contributors, each found by timing the phases of the ticks that exceed 6 ms
(`SPALL_SLOW_PHASE_MS=6`), not by guessing:

1. **Water deltas on every fourth tick** (408 ticks over 6 ms): the compression of changed bricks at
   zstd level 3 on the tick thread. Level 1 (`WATER_DELTA_ZSTD_LEVEL`, any level decodes alike):
   `water.deltas` mean 1.2 to 0.44 ms, 408 slow ticks to 1.
2. **The terrain exclusion was rebuilt after every edit** (295 ticks, mean 10.5 ms in
   `tick.players`, which I had first put down to the walker's collision-window rebuild; the window
   rebuild itself, now logged as `players.window.*`, is 0.1 ms extraction + 1.5 ms collider +
   0.0 ms sync). The cached set of terrain colliders held collider *handles*, which change whenever
   an edit rebuilds a collider, so every edit threw it away and the next tick rebuilt about 100,000
   entries. Colliders now carry their owning body's id (`user_data` = id + 1) and the exclusion is a
   set of body ids, valid across rebuilds; it is dropped only when a terrain brick gets its first
   body or on a structural change. Test
   `an_exclusion_still_hides_a_body_after_its_collider_is_rebuilt`. `tick.players` slow ticks
   295 to 1.
3. **The vegetation frame, once a second** (182 ticks over 6 ms, mean 11.4 ms: building, encoding
   and chunking about 8,700 plants). A worker thread now does it from a copy of the living state
   and the terrain (`Simulation::vegetation_visual_input`); the following ticks send the result. A
   worker that cannot start, returns an error or ends without a result stops the run with an error.
   Clients still receive the frames (182 keyframes, 8,691 plants in the run).

**Measured** (`eng114-regional-1024m-soak-light-2min-timed-deferred`, 8 clients, one walking,
2 edits/s, 8,000 measured ticks after 1,800 warmup; one run per row):

| | tick busy p95 | tick busy p99 | max | physics p95 | server peak |
| --- | ---: | ---: | ---: | ---: | ---: |
| start of this work | 122.6 to 131 ms | 144.9 to 157 ms | 298 ms | 2.2 ms | 5.86 GiB |
| after water compression | 13.5 ms | 20.1 ms | 97 ms | 1.3 ms | 5.85 GiB |
| after exclusion cached | 7.7 ms | 18.8 ms | 99 ms | 1.3 ms | 5.85 GiB |
| after vegetation off-thread | **7.4 ms** | **11.6 ms** | 94 ms | **1.3 ms** | 5.86 GiB |

Targets (p95 12 ms, p99 16.7 ms, physics p95 6 ms, memory 8 GiB): the timing requirements of the
fixture report **passed** for the first time. This is one run on one workload at 2 edits/s; the
heavy 10 edits/s soaks were not timing-instrumented, and the reset transient still peaks at
8.4 GiB (above the 8 GiB target for those fixtures, which do not configure it).

Checks: `cargo test --workspace` 1,330 passed, 0 failed, 73 ignored; clippy clean.

### Final regression and a run-end race (2026-10-10)

Final pass on the last build, one run each: `stress-deferred` passed (`514bafac1a18`, 191 s, server
peak 8.03 GiB), `eight-impaired-deferred` passed (`514bafac1a18`, 194 s), `soak-reset-2min` passed
(`416f099529bc`, 8 clients, no held transactions). The shaped heavy fixture
(`eng114-regional-1024m-soak-impaired-2min-deferred`) **failed once**: the run ended with the two
shaped clients 358 and 383 transactions behind and nobody disconnected. Cause: the server ends a
run once no regional client is owed anything and none has sent anything for `DRAIN_GRACE` (2 s);
a client whose repair request was deferred by the per-client patch queue cap repeats it only after
its backoff (up to 16 x 30 ticks = 8 s), so the server saw 2 s of quiet and ended the run while
the client was still waiting out the backoff. Two changes: the backoff now tops out at eight times
the base (240 ticks, 4 s; the test lists the waits 1, 2, 4, 8, 8 x base), and `DRAIN_GRACE` is 6 s,
documented as covering that backoff plus a slow link's round trip. Afterwards the shaped heavy
fixture passed twice (`c401bd2bc671`, `f79d8d380313`; 425 and 438 s; no client held anything;
nothing dropped), `eight-impaired-deferred` again (203 s) and `soak-light-impaired-2min` (220 s,
`5e608e1488df`).

Checks: `cargo test --workspace` 1,330 passed, 0 failed, 73 ignored; clippy clean.

### Reset memory: free the old world before building the new one (2026-10-10)

**The two symptoms.** The server's peak working set during a reset was 8.0 to 8.4 GiB against a
steady 5.5 GiB (above the 8 GiB target of the G4 text), and after a reset the steady state stayed
about 1 GiB higher.

**Cause.** `ResetWorld` built the new world while the old one was still fully resident (the old
one was only freed afterwards, since the earlier change moved that free to a background thread), so
the two coexisted: peak = old + the new one's allocations. The later +1 GiB is consistent with the
two worlds' allocations interleaving on the heap (Windows' default allocator, no custom one): it
disappeared once they no longer coexist, which is evidence, not a measurement of the allocator.

**Change.** The reset now captures the catalogue basis it needs from the old world, swaps in a
small placeholder world, **waits until the old world has been freed** (`Simulation::wait_for_retired`),
and only then builds the new world. The tick thread is blocked for the whole reset either way, so
nothing needs the old world. Two intermediate versions were measured and rejected: freeing on a
background thread *while* building (peak 5.4 GiB but resets 21.6 and 23.0 s with two clients and 27
to 28 s with eight, because the free and the build contend for the heap; freeing alone takes only
2.5 to 3.7 s when it has the machine to itself).

**Measured** (one run each):

| | before | now |
| --- | ---: | ---: |
| reset, 2 clients (`stress-deferred`) | 12.8 to 17.1 s, peak 8.03 GiB | **12.6 s and 15.0 s, peak 5.38 GiB** (free 2.6 and 2.7 s) |
| reset, 8 clients (30-minute heavy soak) | 14.0 to 15.8 s, peak 8.41 to 8.43 GiB | **16.2 s and 15.0 s, peak 5.78 GiB** (free 3.5 and 3.7 s) |
| steady state after a reset | about +1 GiB | back to the pre-reset level |

`eng114-regional-1024m-eight-impaired-deferred` 5.54 GiB peak, `stress-deferred` 5.42 GiB: the whole
run, resets included, now stays at the steady-state level. The 30-minute heavy soak
(`eng114-heavy30-j`, 1,974 s, 111,961 ticks, 2 resets, all eight clients on `f9604a985340`,
18,180 of 18,180 edits committed, 0 dropped, 0 held, commit p95 45 ms) peaks at 5.78 GiB (was
8.43). Its memory is a sawtooth: about 5.65 GiB right after a reset, rising about 0.25 GiB over the
following ten minutes as the edited terrain grows (each edit turns uniform bricks into dense ones,
about 35 KB per edit), and falling back at the next reset. That growth is edit data, not a leak
of freed memory; a long session without resets will keep growing with its edits.

**Harness.** The pipeline's "overloaded" refusals (`intent_stats.queue_full_rejections`, 111 in the
interim version whose resets were 28 s long) are retryable like the throttled ones and are now
excluded from the admission check as well (`admission_clean`; test extended).

**Also seen:** one 30-minute run (`eng114-heavy30-i`) is invalid: the computer went to sleep during it
(the clients' last event is 9.7 hours after its start) although the harness sets the Windows
keep-awake flag, and the harness then hit its deadline. It was rerun; the results above are the
rerun.

Checks: `cargo test --workspace` 1,330 passed, 0 failed, 73 ignored; clippy clean.
