# ENG-218 progress: ordered animal consumers

Date: 2026-10-17. ENG-218 remains in progress. This is measured partial
implementation evidence, not acceptance of animal replication or recovery.

Current accepted main protocol/wire is 9: inline lifecycle framing plus a
bounded typed bulk marker/codec. The internal inline authoritative producer is
integrated and validated, along with standalone typed attachment validation and
the baseline/reset runtime allocator. Shared geometry/lifecycle allocation,
live animal admission, bulk dispatch and production spawning remain closed. Earlier sections
below record the versions and test counts at their specific integration points.

## Integrated replay guard

Main commit `81b89f4` adds a bounded transaction payload fingerprint and
strengthens the existing client replay guard. The same transaction resent with
a new recipient control sequence remains a duplicate. A different logical
payload under the same transaction ID is rejected, including while the original
waits for a terrain catalogue, repair or bulk geometry. A held bulk transfer
cannot be rebound to another transaction. Conflicting repeats cannot advance
the replica clock or replace queue entries.

This reuses `ReplicaWorld.applied_tx` as an ID-to-fingerprint map; it creates no
actor ledger, journal or sequence. The fingerprint includes ordered operation
bytes and every logical transaction field while excluding the transport control
sequence. It checks record counts and serialized size before allocating the
fingerprint payload. Existing wire layouts and schema versions are unchanged.

Changed files:

- `crates/spall_protocol/src/records.rs`: `replay_identity_hash`.
- `crates/spall_protocol/tests/transaction_replay_identity.rs`: bounded identity,
  transport-sequence independence and operation-order tests.
- `crates/spall_client/src/replica.rs`: the existing guard, held transaction
  correlation and committed/catalogue/repair/bulk rejection regressions.

Checks on the main checkout:

- `cargo test -p spall_protocol --test transaction_replay_identity`: 2 passed.
- `cargo test -p spall_client --lib replica::tests`: 27 passed.
- `cargo test -p spall_protocol -p spall_client`: **261 passed, 0 failed,
  2 existing ignored** with local loopback access. An initial sandboxed run
  timed out during the QUIC baseline test's handshake; that isolated test and
  then the complete suite passed with loopback access. Logs:
  `.local/eng218-baseline-liveness-retry.log` and
  `.local/eng218-replay-integrated-tests-loopback.log`.
- `cargo clippy -p spall_protocol -p spall_client --lib --tests -- -D warnings`:
  passed; `.local/eng218-replay-clippy.log`.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Only filesystem incremental-cache hard-link warnings remained. These CPU and
loopback tests are not the combined 1024 m, GPU or population performance gate.

## Integrated lifecycle evidence and exact restore

Commits `a00102d`, `360cdd9` and `41112ff` add and independently verify the
lifecycle commit envelope. Its outer receipt binds the unchanged ENG-217 change,
authoritative death witness and source-fenced pre-motion overlay. Valid death
can catch up after missed motion datagrams, including a clip restart under a
newer epoch; stale source generations, static poses and forged evidence reject.
Topology and motion hashes are separate. This is still a portable reducer,
not an installed wire operation or a completed consumer slice.

Commits `88c1e41`, `1a83051` and `df9a74f` add exact corpse reconstruction and
an owned prepare/commit batch. All component shapes, source shards and mass
properties prepare before any solver insertion. Commit checks the creating
world identity, physics step, body-slot count and every source/owner reservation
before publishing the batch. Invalid later members, cross-member collisions,
stale preparations and preparations from a different world reject atomically.
Density is captured once per material across the batch.

Reconstruction retains original source coordinates and VolumeIds, independent
component poses/twists, a single surviving derived component, retired serial
high-water and empty lifetime source bindings. It does not rerun death or
fracture. This cold-restore milestone preceded the common mixed batch below.
Its safety caps (1024 owners, 16 Mi dense cells, 4 million solid cells and
4 million primitives across a batch) bound work; they are not measured
population or 1 km performance targets.

Additional changed files: `spall_protocol/src/creature_owner.rs`, protocol
`creature_commit_review.rs` and `creature_owner_review.rs`; physics
`src/articulated.rs`, `src/world.rs`, `src/lib.rs` and
`tests/corpse_restore_review.rs`; `docs/reports/ENG-218-design.md`.

## Integrated checks after cold-restore batching

Measured on main `963d1b7`:

- `cargo test -p spall_protocol -p spall_physics -p spall_sim -p spall_client`:
  **646 passed, 0 failed, 12 existing ignored**, across 67 test/doc-test suites,
  with local loopback access. Log: `.local/eng218-final-tests.log`.
- `cargo check --workspace --all-targets --all-features`: passed.
  Log: `.local/eng218-final-workspace-check.log`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_client --all-targets -- -D warnings`:
  passed. Log: `.local/eng218-final-clippy.log`.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Release regression checks:

- `cargo build --release -p sandbox --example animal-collision-probe`: passed.
- `target/release/examples/animal-collision-probe.exe --corpse-bounded`:
  passed all 18 existing trout/crow/deer cases. This retains the original
  bounded corpse workload; it does not exercise a populated 1 km world.
  Logs: `.local/eng218-release-build.log` and `.local/eng218-corpse.json`.

Protocol/wire 7, baseline schema 1 and store schema 3 remain unchanged. No
production registration, real lifecycle TopologyOp, actor checkpoint or complete
baseline consumer is claimed by these prerequisite checks. GPU, two-client
animal recovery/crash, population performance and combined weather/1024 m gates
remain unrun.

## Common physics and geometry preparation

Main commits `8cb4f09` and `bcbaaaa` integrate the common articulated batch and
independent public-API checks. The batch prepares living kinematic births,
checked alive-to-corpse transfers and exact cold restoration together. Density
is sampled once per material; birth-bound density changes reject at death,
while legacy unbound kinematic creation retains its existing density semantics.
Death retains all lifetime source IDs. Cumulative primitive accounting includes
the actual collider primitives in transfers, not merely their collider count.

The opaque selected-body fence covers world identity, physics step/slots and
affected body pose/twist/mass and collider shape/configuration. It explicitly
tracks absent colliders. Batch validation and infallible publication are split
so unrelated prevalidated geometry can publish between them. Using a validated
token on another world or tick is a programmer error caught before mutation;
the owning simulation thread must validate all participants before any publish.

Main `3e3eed5` adds owned geometry preparation and final freshness validation
while keeping the public `commit` wrapper. It captures target geometry, registry,
world/physics identity, origin/epoch, affected terrain colliders and journal
cursor without cloning the whole physics world. Main `96ba47d` separates geometry
publication from journal append: the normal wrapper appends exactly once, and a
future combined transaction can apply both prepared participants before appending
the returned entry. Commit timing still covers prepare through journal append.
These are prerequisites, not yet
an actual ordered lifecycle transaction, client consumer or durable actor store.

New evidence covers the permitted interleave: prepare and validate geometry and
a two-part living birth, publish geometry first (adding a child body), publish
the validated articulated batch, verify exact source ray attribution and geometry
hashes, then append one journal entry. This fixture does not serialize the animal
into that geometry transaction and does not establish lifecycle durability.
The future combined candidate must reserve total geometry plus articulated body
capacity and shared allocator/journal/attachment bounds before either publishes.

Additional changed files: `spall_sim/src/{commit,world,commit_review}.rs` and
`spall_physics/tests/articulated_batch_review.rs`. The physics selected-body fence
also verifies absent collider state followed by collider reappearance; an empty
affected terrain-brick edit was not naturally present in the simulation fixture.

## Integrated checks after common preparation

Measured on main `96ba47d`:

- `cargo test -p spall_protocol -p spall_physics -p spall_sim -p spall_client`:
  **667 passed, 0 failed, 12 existing ignored**, across 68 test/doc-test suites
  with local loopback access. Log: `.local/eng218-composed-tests.log`.
- `cargo check --workspace --all-targets --all-features`: passed.
  Log: `.local/eng218-composed-workspace-check.log`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_client --all-targets -- -D warnings`:
  passed. Log: `.local/eng218-composed-clippy.log`.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo build --release -p sandbox --example animal-collision-probe` and
  `target/release/examples/animal-collision-probe.exe --corpse-bounded`: passed,
  all 18 original trout/crow/deer cases. Logs:
  `.local/eng218-composed-release-build.log`, `.local/eng218-composed-corpse.json`.

The independent public tests initially exposed a missing bound-density check in
death preparation; it was fixed before this integrated run. The test expectation
was also corrected to count the living aggregate root plus two part bodies.
No failed check was treated as a pass. Filesystem hard-link cache fallback
warnings remain. Versions and the unrun launch gates listed above are unchanged.

## Current-only protocol and save boundary

The user's 2026-10-17 instruction removes legacy compatibility from this work.
Main now uses protocol/wire 8. `TopologyOp::CreatureLifecycle` carries the outer
commit inside the existing ordered transaction; enclosing transaction ID,
operation ordinal and schema are structurally checked. The codec checks size
before allocating the encoded frame. Catalog/transition validation remains the
candidate consumer's responsibility. Clients and geometry-only recovery refuse
actor-bearing transactions before geometry, clocks, sequence gates or queues
change. Production spawning remains closed.

Main `4b8e090` exposes immutable prepared transfer evidence and logical body-slot
demand, plus a joint external geometry capacity check. Main `1e22e53` adds the
inline wire boundary; `7e6dc65` fixes the handshake version and verifies a valid
nonempty source-bearing Birth through codec and reducer. Main `6446151` adds
independent rejection tests with a validated actor payload after an ordinary
cell edit. Both replica and geometry-only recovery leave their state unchanged.

Main `fcf24fe`, `20c355d` and `25a818e` remove journal header rewriting, old
weather checkpoint/journal DTO fallbacks and conversions, missing-table
creation/backfills, vegetation v1-v3 DTO upgrades and old organism decoding.
Current store schema 3 requires its complete table/column layout. A nonempty
unversioned database is rejected; only a pristine database initializes. Current
weather/snow behavior and fallback to a prior complete valid checkpoint after
actual corruption remain. Baseline schema 1 is unchanged; actor-aware durable
recovery still requires the planned new schema.

Validation evidence for this boundary:

- Worker `cargo test -p spall_store`: 45 passed, 1 ignored after removal;
  protocol suite: 111 passed; ecology suite: 32 passed. Strict combined
  store/protocol/ecology all-target Clippy and diff checks passed.
- Coordinator focused nonempty Birth codec/reducer and handshake-v7 refusal:
  each passed. Client/recovery atomic rejection tests: 2 passed;
  `.local/eng218-lifecycle-rejection-review.log`.
- The coordinator seven-package integrated run stopped at the server snow test's
  obsolete expectation that wire-6 journals migrate. Earlier suites passed; this
  run is not recorded as a complete pass. Main `ca33d8f` replaces that expectation
  with old-version rejection while retaining the current snow restart/Q32 checks.
  `cargo test -p spall_server --test snow_persistence` then passed (1 test).
  Logs: `.local/eng218-current-only-tests.log` and
  `.local/eng218-snow-current-only-retry.log`.
- The complete coordinator retry, `cargo test -p spall_server -p spall_sim
  -p spall_store`, passed: 479 tests, 12 ignored. Workspace all-target/all-feature
  checking passed. Seven-package all-target Clippy with warnings denied first
  caught a redundant clone in the new client review fixture; removing it made
  the retry pass. Formatting and diff checks passed. Logs:
  `.local/eng218-current-only-remaining-tests.log`,
  `.local/eng218-current-only-workspace-check.log` and
  `.local/eng218-current-only-clippy-retry.log`. The build emitted a filesystem
  hard-link cache fallback warning; no GPU or launch performance result is implied.

Main `5c289eb` measures the full validated three-part sparse-source fixture:
**99,068 postcard body bytes**, above the **65,536-byte** control cap. Codec
rejection is expected and atomic; the fixture was not reduced or revoxelized.
Typed bulk transport is therefore a concrete prerequisite for this multi-part
representation. The later bulk marker/codec and server candidate changes remain
in the isolated worktree under implementation, not accepted in this report.
Log: `.local/eng218-three-part-wire-size.log`.

Next unblocked task: continue ENG-218 through real authoritative and replica
lifecycle consumers, bounded correlated bulk transport, complete baseline/segmented staging, and
checkpoint/journal recovery consumers. Production animal spawning remains
disabled. Source edit/fracture synchronization, species/current habitat and
integrated two-client restart/destruction/performance proof still follow.

## Typed bulk framing accepted after current-only cleanup

Worker `0d643bd` is integrated as main `bf6ba9a`, with the geometry-only replay
guard extended to refuse both actor forms. Protocol and handshake are now 9;
previous versions, including 8, reject without migration. The new marker carries
the operation ordinal, payload schema, outer hash and transfer ID. Its bounded
postcard/zstd payload checks correlation before nested decoding and enforces
raw, compressed and cumulative source/ownership work limits before hashing.
Geometry split groups retain a shared transfer ID across multiple children and
their source patch; lifecycle IDs cannot reuse geometry or other lifecycle IDs.
World-version tag 6 is defined for the payload but is not enabled in network
dispatch. This is transport framing, not a completed attachment consumer.

The unchanged validated three-part fixture measures **99,093 raw bulk-envelope
bytes, 397 compressed bytes**. The earlier **99,068-byte** measurement is its
inline topology body. Neither is a measurement of the shipped animal assets.

Worker checks passed: protocol unit tests 81; focused bulk tests 5; server unit
tests 88 with 1 ignored plus all server integrations; store unit tests 15 plus
persistence integrations 29; strict protocol library/test Clippy. The earlier
client run passed 129 tests with 2 ignored. Initial old expected-version
assertions were corrected. A compact Uniform overflow test also originally
expected the later bulk work check, while the existing bounded owner decoder
correctly rejected earlier; the test now preserves that earlier refusal.

Coordinator checks on integrated main passed:

- `cargo test -p spall_protocol --test creature_commit_review`: 11 passed.
- `cargo test -p spall_client lifecycle_rejection_review`: 2 passed, now covering
  inline and bulk forms, both catalogue states and repeat delivery. Both client
  and geometry-only recovery retain geometry, clocks, receipts and queues.
- `cargo check --workspace --all-targets --all-features`: passed after adding
  missing bulk labels to the existing G4 diagnostic and miniature test applier.
  The initial attempts failed those exhaustive matches; neither was hidden.
- Seven-package all-target Clippy with warnings denied passed; formatting and
  diff checks passed after formatting the new geometry-only guard.

Logs: `.local/eng218-bulk-integrated-protocol.log`,
`.local/eng218-bulk-integrated-rejection.log`,
`.local/eng218-bulk-integrated-workspace-retry2.log`,
`.local/eng218-bulk-integrated-clippy.log`, and
`.local/eng218-bulk-format-retry.log`. Cache hard-link fallback warnings remain.

The isolated producer's first ordered Birth/inline-overflow test passed. Review
then required final death-source freshness, installed birth-twist consistency,
incremental work bounds and real mixed/death scenarios. Those additions remain
unaccepted until their focused/full checks pass. Next unblocked work is still
ENG-218: finish that producer, shared transfer reservations and typed journal
attachments, then replica/baseline and durable recovery consumers. No launch,
GPU or population acceptance is claimed.

## Internal authoritative inline producer integrated

Worker `4a94a2c` is integrated as main `1d304da`. The crate-private producer
prepares a small actor-host candidate, shared registry IDs, original source
ownership, checked physical evidence and optional geometry under one transaction
and journal sequence. It validates all candidates before publishing geometry,
physics, sources/host and one journal entry. It does not clone the simulation or
physics world. Final checks include actual mapping entries, world identity,
physics step/affected owner state, host/allocator revision, journal cursor and
source identity/content/brick revisions/metadata. Death uses the prepared physical
transfer outcome. Birth rejects uninstalled nonzero initial twist. Incremental
source/physics/portable-data work limits run before expensive staging copies.

Tests cover ordered Birth -> applied part pose -> checked Death, stale pose-token
refusal, source replacement between prepare and validate, late invalid actor in a
mixed terrain edit, and successful mixed publication with one journal append.
The strengthened source test changes a sparse revision-1 brick while a resident
Uniform AIR brick keeps the maximum revision at 5. An initial two-Dense-brick
version exceeded the inline cap; the revised fixture retains the exact low-revision
invariant without falsely claiming multi-part bulk admission. Mixed hash assertions
use the terrain hash because the geometry-only whole-world hash deliberately
refuses actor-source worlds. No failing fixture was counted as passed.

Worker focused lifecycle tests: 9 passed. Coordinator
`cargo test -p spall_sim`: **252 passed, 9 ignored, 30 targets**, including unit,
integration and doc targets. Workspace all-target/all-feature check and strict
seven-package all-target Clippy passed. Logs:
`.local/eng218-producer-integrated-sim.log`,
`.local/eng218-producer-integrated-workspace.log`, and
`.local/eng218-producer-integrated-clippy.log`. The initial coordinator command was
not executed because automatic approval review reported a usage-service failure;
an account check showed usage available and the same approval path then accepted
the retry. No approval was bypassed.

Independent review found no concrete mismatch in candidate fences or death
pose/twist evidence. It confirmed remaining blockers: no production tick caller,
inline-only output, and pre-commit waking in the existing body edit schedule. A
coordinator guard now refuses dormant mixed targets before geometry preparation,
preventing a solver lookup on an absent body and retaining sleep, geometry,
host, registry and journal. The focused regression passed (1 test) and strict
Sim all-target Clippy passed; logs `.local/eng218-dormant-mixed-rejection.log`
and `.local/eng218-dormant-mixed-clippy.log`. Ordinary edit scheduling is unchanged;
real staged reactivation remains required, not accepted by this refusal test.

Three narrow dead-code allowances mark the closed intent, result and convenience
entry, pending a real server caller. No module-wide suppression or spawning
activation was used. Protocol/wire 9, baseline 1 and store 3 remain current.
The existing three-part record still rejects this inline-only producer.

Next ENG-218 prerequisites are in progress in the isolated checkout: a single
checked runtime transfer allocator for existing baseline/reset/repair paths and
bounded typed attachment sets. They are not yet integrated. The eventual tick
handoff must use owned reservation data before final hashing/journaling; all
geometry split and lifecycle callers must use that same allocator. Replica,
complete baseline/checkpoint, actor-aware journal recovery and live scheduling
still follow. No combined 1024 m/GPU/population/multiplayer acceptance is claimed.

## Transfer allocator and typed attachment prerequisites accepted

Allocator worker `aa4600c` is main `f4779af`; early-reset follow-up `4d7a005`
is main `a272e48`. A non-Clone runtime allocator uses one checked 61-bit low
sequence for all namespace reservations. Owned ranges have constant storage;
unused/dropped values remain burned. Zero, overflow and conflicting namespace
bits reject. Retained-ID high-water seeding is implemented/tested but not yet
connected to the server recovery source. LateJoin baseline/retry/repair/reset
callers use the allocator. Geometry split and lifecycle callers still await the
owned handoff, so common allocation across every path is not yet claimed.

Review caught the production reset caller replacing the simulation before the
LateJoin-only reservation check. The follow-up now reserves the complete sorted
session group immediately after policy checks and before catalogue capture or
world replacement. Allocation failure produces a rejected AdminStatus without
changing world hash, registry, journal, player position, cached/pending transfers,
join phase or retries. Publication consumes the owned group and rechecks sessions.

Protocol worker `6608530` and `1c197b6` are main `a8d790e` and `7ac6bc1`.
Standalone attachment schema 1 carries explicit geometry/lifecycle payload families,
with exact marker coverage, no missing/extra/duplicate IDs, lifecycle correlation,
and multiple child/source-patch geometry groups. The immutable decoded candidate
binds the existing parent replay identity, not only its transaction ID. Geometry
source-owner authenticity and material/catalog/world semantics belong to the
owning consumer. No second ledger or actor journal was introduced.

Nested geometry postcard lengths are scanned before vector allocation, using
conservative storage estimates and observed decompression capacity. Cumulative
compressed bytes remain 64 MiB; geometry decode/storage is bounded at 256 MiB and
lifecycle envelopes at 64 MiB. Lifecycle owner-set work is also summed before
canonical hash work: records, source bricks, dense/solid cells, masks, bytes,
overlay and witness arrays. Five compact Uniform payloads, each 983,040 solids,
now reject cumulatively above the existing 4,000,000-solid owner-set ceiling on
encode and decode. General geometry does not inherit animal-only cell caps;
its expansion/physics budget remains an owning-consumer gate. The tests also
round-trip mixed geometry plus the unchanged validated three-part Birth and
validate its source masks/poses through the reducer.

Coordinator checks:

- `cargo test -p spall_server` on integrated allocator: 196 passed, 2 ignored,
  23 targets (before the early-reset follow-up). Workspace all-target/all-feature
  check and strict server all-target Clippy passed.
- After both protocol commits and the reset follow-up,
  `cargo test -p spall_protocol`: 124 passed, 0 failed/ignored.
- `cargo test -p spall_server
  reset_id_exhaustion_preflight_preserves_world_and_late_join_state`: 1 passed.
- `cargo test -p spall_server --test snow_replication --test late_join_session`:
  2 passed, including real multiplayer reset behavior.
- `cargo check --workspace --all-targets --all-features`, strict seven-package
  all-target Clippy, workspace formatting and diff checks passed.

Logs: `.local/eng218-allocator-integrated-{tests,workspace,clippy}.log`,
`.local/eng218-attachments-integrated-{tests,workspace,clippy,format}.log`,
`.local/eng218-reset-integrated-tests.log` and
`.local/eng218-reset-integrated-scenarios.log`. Worker initial expected-ID/import
and validator-order compile/test issues were corrected before acceptance.
The worker's mistaken target-directory build was interrupted before any tests;
its generated debug directory/compiler metadata were verified and moved intact
inside ignored `.local/eng218-extra-target-allocator`. No results came from that
attempt. Existing user artifacts were preserved. Cache hard-link warnings remain.

Next ENG-218 work is the live geometry reservation handoff: expose a proven
bounded potential demand from pending/inflight pipeline state, reserve owned
Split IDs before the existing tick and consume them in the current sequential
commit/rebase loop. Unused IDs burn; idle demand is zero. This avoids a broad
resumable-tick rewrite and preserves once-only weather/physics updates. The bound
must cover carried inflight work, not just newly submitted jobs. The worker is
implementing this slice, unaccepted; startup seeding and fan-out must use the same
recorded IDs before removing the transaction-derived path. Lifecycle bulk binding,
typed journal replacement/current store version, actual replica/baseline/recovery
consumers and staged dormant-body waking still follow. Protocol/wire 9, baseline 1
and store 3 remain current; no legacy migration was added. Production spawning,
combined 1024 m weather/animals, GPU, population and multiplayer recovery/destruction
acceptance remain gated.

## Geometry reservation handoff and durable startup seeding (2026-10-18)

Main `65e0911` integrates worker `eb2650c`; `d3fa65f` integrates recovery
follow-up `2699fb5`. The server reserves geometry Split IDs from the same runtime
allocator used for baseline/reset/retry/repair, immediately before the existing
simulation tick. The conservative bound covers carried inflight work plus the
configured newly submitted edit limit across all rebase rounds. Offthread edit
work needs at most one; pending prewarm alone and idle ticks need zero. The
owned pool passes through the existing sequential commit/rebase path, preserving
once-only physics/weather work. Unused reservations burn. Bulk output retains
the exact ID in every child/source marker, committed result, fanout and durable
geometry payload. No transaction-derived fallback remains. Missing IDs reject
bulk preparation without publishing that geometry candidate.

Independent review caught startup scanning the fresh in-memory journal instead
of the recovered durable suffix. Startup now extracts IDs from every topology
record in that suffix, validates them using allocator namespace/range checks
before restore or checkpoint publication, and seeds the live allocator from
those IDs plus any current entries. This does not add a persisted allocator
counter or another journal. A real SQLite regression covers retained Split low
ID 401 and a CRC-valid invalid-ID record: the next reservation exceeds 401,
while the malformed record rejects startup with checkpoint count unchanged.

The new full-size noisy geometry split server regression uses actual QUIC
begin/parts/end and a replica. It compares the marker ID, every streamed ID,
durable attachment hash and recovered world hash. An injected final checkpoint
failure deliberately retains the suffix; restart must then issue a baseline
ID above the retained Split low sequence. The fixture was not reduced to fit
the inline path. Public simulation boundary tests additionally reject zero,
wrong/mixed namespace and duplicate IDs before changing tick, world, registry,
journal cursor or topology epoch; an idle tick needs no reservation.

The initial combined test run could not start a local-network test server in
the sandbox and stopped at the existing column-water replication test. The
same suite was retried with local-network access. Worker E2E lint findings
(single-match loops and borrow expressions) were corrected before acceptance.
The worker's accidental second build target was verified and moved intact to
ignored `.local/eng218-extra-target-handoff`; user artifacts were preserved.
An ignored Cargo wrapper now enforces the established workspace target path.

Changed implementation files: `spall_sim/src/{commit,schedule,sim}.rs`,
`spall_sim/tests/{acceptance,bulk_split,g4_workload,transfer_reservations}.rs`,
`spall_server/src/{persist,serve}.rs`, and the new
`spall_server/tests/geometry_split_transfer_id.rs`. Protocol/task documentation
and this report record the accepted boundary; dependency versions are unchanged.

Coordinator checks on the integrated main checkout:

- `cargo test -p spall_sim -p spall_server -p spall_protocol`: retry passed
  **578 tests, 0 failed, 11 ignored across 64 targets**, including both new
  simulation boundary tests and actual QUIC/SQLite restart coverage. The initial
  sandbox run stopped at the local-server startup timeout described above.
- `cargo check --workspace --all-targets --all-features`: passed.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store
  -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Logs: `.local/eng218-handoff-integrated-tests{,-retry}.log`,
`.local/eng218-handoff-{workspace,clippy,format}.log`, and the initial isolated
two-test boundary check `.local/eng218-transfer-boundary-tests.log` (2 passed).
Cargo commands used `.local/eng218-cargo.ps1` to pin the target directory;
cache hard-link warnings remain environmental. These are measured CPU/network
test results, not the unrun hardware/1 km acceptance targets below.

Next unblocked task remains ENG-218. Before durable animal production, implement
complete actor checkpoint and baseline capture/restore: portable source geometry,
ownership, hierarchy, frozen poses, physics evidence and allocator high-water,
including an empty host. Typed attachments then need explicit transport framing,
ordered replica/recovery consumers and one journal record/sequence, replacing
the current geometry-only payload under a new current store version. Lifecycle
bulk binding must happen before final transaction hashing/publication. No actor
journal, fake geometry baseline, legacy migration or live spawning is accepted
as a substitute. Current wire 9, baseline 1 and store 3 remain unchanged.
Combined 1024 m weather/animals, GPU, population and multiplayer animal
recovery/destruction acceptance are still unrun.

## Complete actor recovery prerequisite plan (2026-10-18)

The next coherent implementation boundary is exact reconstruction of a moved
Alive physics owner, before the Simulation snapshot API and checkpoint schema
change. `AliveBirth` initializes rest motion and tokens; using it followed by
fallible public mutation would not be an atomic restore. A value-only
`AliveArticulatedRestore` and prepared batch operation will carry the applied
source hierarchy, root pose/twist, topology/pose tokens, source generation and
intact serial high-water. The existing owner-key index will preserve stable
identity through immediate checked death and later fracture. Preparation must
share current source/grid/density/primitive budgets and final batch fences;
publication installs both root motion and each part body's measured twist before
exposing the owner. Independent review found that reconstructing part velocities
only from the rigid root would lose articulated animation motion. The runtime
restore input must therefore require exactly one keyed measured linear/angular
velocity for each part, with no missing/duplicate keys or synthesized fallback.
A bounded value-only getter must refuse capture while any pose is pending. No
runtime handle will enter portable DTOs.

The planned physics tests compare animated multi-part capture/restore, exact
source queries, tokens, immediate checked-death mass/momentum, invalid numeric input,
cross-owner/source collision and stale batch atomicity. This is a prerequisite,
not complete animal save/restore acceptance. The current geometry-only checkpoint
must reject any initialized actor host, including an empty host with consumed
allocator high-water, until a complete format can retain it.

Subsequent Simulation capture must join actual source volumes and original
metadata to full live physics ownership facts. A per-component fracture-planning
snapshot is insufficient: it omits original empty bindings, full component
partition, serial high-water and portable pose epoch. Capture/restore must retain
all those facts, use the same `CreatureOwnershipSetV1` plus allocator snapshot,
enforce cumulative decode/work limits, validate the injected catalog and publish
prepared source/host/physics candidates atomically. Complete baseline/checkpoint
consumers and typed journal/transport follow; production spawning stays closed.
The existing serialized ownership/motion records do not retain independent part
twists, so their explicit current schema must also change before arbitrary live
animation recovery can be claimed. A rigid-only workload is not a substitute.

Final integration review additionally found a first-step hazard in the initial
worker draft: Rapier requires velocity-based kinematic mode to install measured
twists, but leaving that temporary mode active without another authored frame
would freely integrate individual limbs while the stored hierarchy remained
unchanged. That draft was held out of main pending a reset before the next
integration and actual no-input/animation-resume regressions. Immediate snapshot
and death-witness checks alone do not prove correct next-step behavior.

## Animated Alive physics reconstruction prerequisite accepted

Worker `031374a` is main `decfd1a`; first-step correction `cea8c1c` is main
`f313fb2`. `AliveArticulatedRestore` and `AliveRestore` in the existing prepared
physics batch carry stable owner key, source generation, topology/pose tokens,
intact serial high-water, applied source hierarchy, root motion and a required
exact keyed set of measured part-body linear/angular velocities. Motion count
is checked before key-set allocation; duplicate/missing keys, nonfinite values,
zero source revision, identity collisions and stale candidates reject before
solver publication. The existing source/grid/density/primitive and body-slot
budgets remain shared. Birth still initializes its original rest state.

The applied-motion getter refuses pending root/local frames, corpses and
nonfinite twists. Owner, pose, source and motion reads must be made together on
the owning thread with no intervening mutation. It is not a cross-thread receipt.
The owner snapshot now exposes actual owner key, source generation and serial
high-water rather than requiring hard-coded reconstruction values.

Rapier ignores velocity setters for position-based kinematics, so reconstruction
temporarily uses velocity-based kinematics to retain the measured twists for an
immediate checked death. The corrected implementation queues those new owners
for conversion back to position-based control before their first integration.
An authored root/local frame consumes the temporary mode earlier; checked death
also clears it so the queued conversion cannot turn a corpse back kinematic.
No global owner scan, second physics engine or persisted runtime handle is added.

The three-part animation fixture actually moves/rotates local parts and proves
their measured twists differ from its stationary root. Original and rebuilt
owners produce matching immediate checked-death mass, linear momentum and angular
momentum (test tolerances: mass 1e-10 kg, momentum 1e-8 in their respective units).
The test also commits the transfer and checks installed mass/twist and retained
owner identity. Coordinator extensions step that corpse and verify it stays
dynamic, compare every part body's physical pose after a no-input step, and
resume an authored local frame before the first step, checking actual tip-body
position and velocity. Pending root/local capture, missing/nonfinite motion,
duplicate owner and stale-batch refusals are covered. These are bounded CPU
fixtures, not measurements of the shipped species or 1 km population.

Main `cf0e3bd` adds a separate current-checkpoint refusal for an initialized actor
host, even an empty host with allocator ID high-water 1 or 73 and epoch 4. Both
full and incremental capture must return `CreatureCheckpointUnsupported` while
retaining world hash, registry, journal cursor, tick and allocator snapshot.
This prevents silent loss in the current geometry-only format; it does not
implement animal saving. No live spawning or protocol/store version changed.

Review corrected initial missing-field/vector-cross compile errors. Worker
focused checks progressed from 4 to 5 passes; these include one existing birth
test. The coordinator's final focused `cargo test -p spall_physics --lib alive_`
passed 6 tests (0 failed/ignored), including the added local-resume scenario.
The first-step drift draft was not accepted on its immediate-witness result.
The worker's full physics run did not retain a final aggregate exit code; only
the coordinator's integrated results below count as the complete measured gate.

The first integrated run stopped at three `prediction_timeline` QUIC certificate
failures. Inspection confirmed its process-ID-only session directories were
created on 2026-10-15 and reused on 2026-10-18, allowing stale address/fingerprint
files to satisfy startup waits. The harness now uses a timestamp nonce and
exclusive directory creation; old diagnostic folders were preserved. No TLS
verification or workload was weakened. The complete suite was retried after
that concrete harness correction.

Coordinator final checks on integrated main:

- `cargo test -p spall_physics --lib alive_`: 6 passed, 0 failed/ignored.
- `cargo test -p spall_server --lib
  full_and_incremental_capture_refuse_to_drop_empty_host_high_water`: 1 passed.
- `cargo test -p spall_physics -p spall_sim -p spall_server`: corrected-harness
  retry **613 passed, 0 failed, 12 ignored across 71 targets**. The initial
  integrated attempt's three certificate failures remain recorded above.
- `cargo check --workspace --all-targets --all-features`: passed.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store
  -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.

Changed code: `crates/spall_physics/src/{articulated,lib,world}.rs`,
`crates/spall_server/src/persist.rs`, and
`crates/spall_server/tests/prediction_timeline.rs`. Updated design/protocol,
validation, task notes and this report record the exact acceptance boundary.
No dependency, wire 9, baseline 1 or store 3 version changed. Logs:
`.local/eng218-checkpoint-boundary-tests.log`,
`.local/eng218-alive-integrated-focused.log`,
`.local/eng218-alive-integrated-tests{,-retry}.log`, and
`.local/eng218-alive-{workspace,clippy,format}.log`. All Cargo commands used the
existing target-pinning wrapper. Cache hard-link warnings remain environmental.

Next remains ENG-218: bounded full-owner capture (including all corpse component
masks, twists, serial gaps and original empty source bindings), explicitly
versioned complete serialized ownership/motion plus allocator, then atomic
Simulation checkpoint/baseline capture/restore and typed ordered transaction
consumers. The current single-component fracture-planning snapshot is not a
portable full-owner export. Combined 1024 m weather/animals, GPU, population and
multiplayer animal restart/destruction acceptance remain unrun; spawning is closed.

## Whole-owner corpse capture plan (2026-10-18)

The next physics prerequisite is a bounded read-only capture returning the
existing `DynamicCorpseRestore` value, not the single-component fracture-planning
snapshot. The caller supplies canonical original source grids, IDs, revisions,
parents and current frozen hierarchy-local transforms through `SourcePart`.
Authored/rest transforms and sparse volume metadata remain separately owned by
Simulation; they must not be inferred from fractured shard grids. No second
frozen-pose array or competing restore layout is needed.

Capture must validate an expected owner key, source generation and portable
topology/pose tokens, reject pending frames, enumerate actual component IDs from
existing physics shard records, and prove that their exact source-cell masks
are disjoint and cover the supplied current source solids. Canonical source IDs
must match even for empty bindings. Every live component contributes its real
body pose/twist, and the actual serial high-water survives retired serial gaps
and a single remaining derived component. Global physics step is never a
substitute for pose epoch; zero tokens that the current restore DTO cannot
accept must reject rather than being synthesized.

For intact checked-death owners whose physics key has not yet been bound by
fracture/restore, stable identity is supplied by the owning caller, subject to
existing owner-index collision checks. A read may not bind or mutate that index.
An already bound key must match exactly. Original/shard/output work must be
bounded before map allocation and grid cloning, and material/pose/provenance
mismatches must refuse without mutation.

Planned acceptance: intact and multiple-component capture/restore, original
empty binding retention, one surviving derived component with a retired serial
gap, exact query/mass/motion and hierarchy, and atomic refusal of stale tokens,
wrong sources/materials/poses, incomplete partitions and excessive work. This
is not serialized checkpoint or full-owner Simulation export acceptance.

The runtime restore DTO selects its primary physical component from element
zero. Capture must preserve the actual primary body's component first, then
use stable ordering for the remainder; blindly sorting every component by serial
can change the reported owner root after a later fracture replaced its primary.
Portable wire canonical ordering is a separate future contract. Source and shard
budgets must each retain existing supported limits, with explicit cumulative
scan/output accounting rather than halving the accepted workload by counting
the two representations against one source-only ceiling.

Review found an existing limit mismatch: living/intact articulated geometry
accepts 4,000,000 dense cells, while cold corpse restore had reused the fracture
planner's 1,000,000-cell ceiling for derived grids. Applying that lower bound to
capture would refuse already-supported intact owners. The approved correction
aligns capture and cold-restore derived storage with the existing 4,000,000-cell
articulated owner ceiling, retaining the 16 Mi-cell batch bound and all solid,
primitive and geometry limits. Fracture planning keeps its separate 1,000,000-cell
work limit. A sparse greater-than-one-million-cell roundtrip must prove this
boundary; no performance or fracture-feasibility acceptance follows from it.

## Whole-owner corpse capture accepted (2026-10-18)

Integrated physics implementation `c3fb333` (worker `d239ce6`) adds
`PhysicsWorld::capture_dynamic_corpse_owner` and `DynamicCorpseOwnerFence`.
The read-only capture returns the existing owned restore DTO after validating
canonical source identity/hierarchy/frozen transforms, bounded source and shard
work, exact complete disjoint material/cell/revision provenance, pending-frame
absence, owner/source/topology/portable-pose fences and serial high-water.
It captures each actual body's pose/twist and keeps the actual primary first.
Caller keys for intact checked-death owners are checked without binding the index;
no registry-wide population scan, world clone or second actor ledger was added.

Review corrected composed packed-origin pose-range parity with cold restore,
primary selection after fracture, and source/shard budget accounting. Cold restore
now admits the existing articulated 4,000,000-dense-cell storage ceiling; fracture
planning remains at its existing 1,000,000-cell work ceiling. The sparse regression
has more than one million dense cells and only two solids, so it is a storage
compatibility check, not animal population or fracture performance evidence.

Coordinator public-API coverage in
`crates/spall_physics/tests/corpse_owner_capture.rs` adds two independent scenarios:
actual fracture of primary serial 5 into 6/7 while older serial 2 survives,
followed by capture/restore with exact per-component mass/pose/twist and hit-cell
checks; and a negative-coordinate air-padded source whose packed solid remains
correctly positioned. Original empty source binding and parent, pose epoch 17 at
physics step 0, and serial high-water 8 survive the fracture roundtrip.

Measured coordinator checks (all through `.local/eng218-cargo.ps1`):
- `cargo test -p spall_physics --test corpse_owner_capture`: 2 passed.
- `cargo test -p spall_physics -p spall_sim`: 420 passed, 0 failed,
  10 ignored across 48 test-result targets.
- `cargo check --workspace --all-targets --all-features`: passed.

The initial strict lint attempt found an unevenly grouped hexadecimal constant
in the new public test. Its spelling was corrected without changing its value;
final lint/format/diff results are recorded below. Existing incremental-cache
hard-link warnings are environmental; no runtime dependency or format changed.
Logs are `.local/eng218-owner-capture-{public-tests,full-tests,workspace,clippy,
clippy-retry,format}.log`.

Changed implementation files: `crates/spall_physics/src/{articulated,lib,world}.rs`;
coordinator test above; `docs/{protocol,validation,tasks}.md` and
`docs/reports/ENG-218-{design,progress}.md` clarify the accepted boundary.
Next unblocked work remains ENG-218: complete Simulation source/owner capture,
explicitly versioned independent Alive part motion and bounded ownership/allocator
snapshot, atomic checkpoint/baseline restore, and typed ordered journal/replica
consumers. This physics API cannot reconstruct original sparse metadata or authored
rest poses from shards. Wire 9, baseline 1 and store 3 remain unchanged; checkpoint
refusal remains in place and production spawning remains closed. Combined 1024 m
weather/animals, GPU, population and multiplayer animal restart/destruction gates
remain unrun.

Final coordinator checks: strict seven-package all-target Clippy retry passed
(cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store
-p spall_server -p spall_client -p sandbox --all-targets -- -D warnings);
cargo fmt --all -- --check and git diff --check passed.

## Ownership-set decode plan (2026-10-18)

Before complete serialized snapshot integration, add a bounded reader for the
existing `CreatureOwnershipSetV1` layout. The set is currently Serialize-only;
ordinary Vec<Record>::Deserialize would restart every per-record budget and
allocate aggregate contents before validation. Reuse seeded nested record parsing
with independent per-record ceilings plus cumulative set admission: 1,024 records,
64 MiB conservative decoded memory, 16 Mi dense slots, 4,000,000 actual source
solids (including compact Uniform), 4,096 source bricks, 8,192 mask bricks and
4,000,000 mask cells. Parts/components are bounded by record count times their
existing per-record ceiling. Account outer vector capacity before nested parsing,
reject oversized sequence hints immediately and refuse unknown-size excess
records without decoding their payload. Charge nested capacity/solid/scan costs
before allocation or expansion; do not divide a record's allowance by set size.

Unknown map keys in these ownership DTOs will reject immediately before reading
their values. Traversing arbitrary IgnoredAny extension values would bypass work
accounting. Optional None metadata behavior remains supported; required fields
remain required. This is an explicit current-schema strictness change, not a wire
layout or format-version change. Postcard structs cannot carry unknown map keys.

Acceptance: existing single-record tests, valid multi-owner sourceful postcard
and map roundtrips plus catalog validation; aggregate compact Uniform and air-brick
limits across individually admissible records; per-record caps inside sets;
hostile hints/no-hint excess payload probes and cumulative nested-resource tests.
This reader remains separate from semantic/catalog/identity validation and does
not supply allocator serialization, missing Alive part twists, Simulation capture,
checkpoint/baseline or journal consumer integration. No spawning or launch gate is
opened by this prerequisite.
The set reader also provides `CreatureOwnershipSetV1::decode(bytes, catalog)`:
check the raw 64 MiB frame ceiling before parsing, reject trailing bytes, perform
bounded nested decoding, then require ordinary catalog/semantic validation.
Generic Deserialize only establishes resource admission. The existing Serialize
implementation is retained; no new writer API promises equivalence between the
validator's conservative estimates and the reader's native decoded-capacity limits.
Unknown-size map sequences can consume bounded spare vector capacity; raw transport
frames and parser scratch space remain the caller's responsibility.

Review identified unbounded state-name strings underneath the record seed. Bound
species and clip names before ownership allocation to the existing 63/255-byte
limits, preserving their scalar serialization. Strict nested metadata covers asset
identity, source binding, local pose, mask cell and a same-layout local CreatureKey
reader; core identity serialization is unchanged. Map parser scratch space for
escaped strings is outside owned-value accounting, so outer frame bounds still
apply. Postcard borrowed strings are checked before an owned String is created.
## Ownership-set reader integrated (2026-10-18)

Integrated worker `57b8545` as main `a5ca1da`: existing V1 owner-set layout now
has seeded bounded deserialization and a frame-capped, catalog-validated decode
entry point. Per-record counters reset without losing cumulative set counters;
record capacity, fixed state allowance, dense slots, actual/Uniform solids,
bricks, masks, mask cells and work are charged before nested allocation/scanning.
Unknown-size excess bodies refuse before decoding; exact EOF does not reserve
unused output capacity. The validator's estimate and decoder's native-capacity
allowance remain distinct, documented limits; no new writer parity is promised.

Coordinator changes bound state-name ownership to 63/255 bytes, close nested map
extension traversal with strict fixed DTO/key readers, and refuse a 257th source
binding before reading its body. Serialized scalar order and all schema versions
stay unchanged. The initial coordinator public suite caught a real remaining
unknown-field path: MaskCell BudgetDecode's local Wire reader ignored the strict
public DTO attribute. It now delegates to that DTO's strict Deserialize; no
acceptance assertion was relaxed. The public failure/retry logs retain evidence.
Worker review also corrected record-header work charging before local reset and
unknown-size output growth at EOF. Early worker draft fixture exceeded its own
per-record decoded memory bound; its dense aggregate fixture was corrected to
respect individual record admission rather than raising the bound.

Measured coordinator checks (target-pinned `.local/eng218-cargo.ps1`):
- `cargo test -p spall_protocol --test creature_owner_review`: initial 21 passed,
  1 failed (MaskCell map hole); corrected retry 22 passed, 0 failed.
- `cargo test -p spall_protocol -p spall_sim`: 393 passed, 0 failed, 9 ignored
  across 40 test-result targets.

Changed code: `crates/spall_protocol/src/{creature,creature_owner,
creature_owner_decode}.rs`; public review scenarios in
`crates/spall_protocol/tests/creature_owner_review.rs`; protocol/validation/task
notes and this report. Public population scenarios cover real catalog-validated
two-owner sourceful roundtrips and byte stability, duplicate identity and trailing
bytes, compact Uniform admission at 3,997,696 solids versus 4,030,464 refusal,
retained one-owner ceiling, 4,096 air-brick admission versus 4,097 refusal,
63/255-byte names including multibyte text, unknown nested fields and excess
source-binding body refusal. Resource-only fixtures deliberately omit semantic
masks; they are not claimed as valid live populations. Internal decoder probes
exercise cumulative dense/mask/byte/work limits and hostile sequence admissions.

Next unblocked task remains ENG-218. Resolve and implement explicitly versioned
measured Alive part-motion ownership/overlay hashing and checked-death evidence;
corpse motion remains per-component, not per original source part. Then join
canonical Simulation sparse sources/rest poses/allocator with physics captures,
and implement atomic complete checkpoint/baseline plus typed ordered journal and
replica consumers. Any later layout change must reject superseded formats without
migration. This pass did not change wire 9, baseline 1 or store 3. No serialized
complete animal recovery, spawning, combined 1024 m weather/animals, population,
GPU or multiplayer animal restart/destruction gate is accepted.
Final coordinator checks passed:
- `cargo check --workspace --all-targets --all-features`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`.
- `cargo fmt --all -- --check` and `git diff --check`.
All Cargo calls used the existing target-pinned wrapper. Logs:
`.local/eng218-set-reader-{public-tests,public-tests-retry,integrated-tests,
workspace,clippy,format}.log`. Existing cache hard-link warnings remain environmental.
Unrelated main `artifacts/` were preserved. The prior worker's untracked server
test was retained in its ignored `.local/geometry_split_transfer_id.preserved-before-set-decode.rs`
before checking out the already-committed version, and its reviewed history remains
reachable at `codex/eng218-reviewed-worker-history`.
## Animated ownership V2 integrated (2026-10-18)

This milestone supersedes earlier directions to keep the ENG-217 inner ownership
layout unchanged. Current-only V2 record/set, lifecycle operation/change, overlay,
commit and bulk payload types replace their changed V1 counterparts without
aliases or migration. Owner/change/commit/bulk schemas and their changed hash
domains are 2. Protocol/handshake/wire is 10; lifecycle bulk kind is 7, replacing
6. Geometry baseline 1 and store 3 layouts remain unchanged: their journal fields
contain opaque current wire records, and saved wire 9 now rejects without header
rewriting. Complete actor checkpoint/baseline sections are still absent/guarded.

`CreaturePartVelocityV2` retains actual keyed installed linear/angular velocities.
Alive records require exactly one sorted entry per source part, including empty
bindings; Corpse requires an empty list and retains independent component motion.
Source descriptors and existing frozen-pose records keep their layout. Full/motion
and outer commit hashes bind the new list; source-content and topology-only hashes
exclude it. Canonical optional overlay encoding puts its discriminator before the
variable list. Bounded readers count velocity entries, native vector capacity and
work at both owner and population levels; lifecycle attachment budgets include
owner and overlay lists. Existing limits were not raised. Unknown new DTO fields
and excess no-hint vector bodies refuse before reading their contents.

The authoritative death producer samples pending-safe installed part velocities
on the owning thread alongside owner/pose/source captures. The existing prepared
batch fences the physics step and body/owner state before publication. Its complete
before hash and outer commit bind that measured overlay; the unchanged checked
physical witness attests resulting installed root motion. No duplicate input-motion
witness list, secondary ledger or root-rigid inference was introduced. Explicit
zero lists are limited to the existing stationary AliveBirth installation and
fixtures declaring zero motion. Death clears the live-only list. The bridge now
roundtrips three genuinely animated parts through the portable codec before a real
checked transfer, with unequal measured velocities; ordered single-part production
retains its actual physics sample in the emitted overlay.

A two-part ordered producer scenario hit the existing 64 KiB inline ceiling. It
is now an explicit atomic-refusal regression with unchanged journal, registry,
physics count and host revision. The earlier one-part ordered acceptance remains
one-part; no reduced workload is reported as two-part success. Typed bulk
publication remains a launch prerequisite. The current three-part protocol fixture
is measured separately; it is not a shipped animal/population performance result.

Measured coordinator checks, all using `.local/eng218-cargo.ps1`:
- `cargo test -p spall_sim --lib creature`: 32 passed, 0 failed.
- `cargo test -p spall_protocol -p spall_sim -p spall_store -p spall_server`:
  final permitted run 640 passed, 0 failed, 12 ignored across 68 result targets.
- `cargo test -p spall_client --lib lifecycle_rejection_review`: corrected retry
  2 passed, 0 failed; validated V2 inline/bulk actor inputs still refuse before
  replica or geometry-only replay mutation.
- `cargo check --workspace --all-targets --all-features`: passed.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `cargo test -p spall_protocol --test creature_commit_review -- --nocapture`:
  12 passed; separate current V2 fixture byte measurements recorded below.

Earlier attempts are retained in `.local/eng218-motion-*` logs: intermediate build
errors/fixtures were updated with required explicit motion and current schema;
wire golden/old-version expectations were updated, never relaxed. An excess-body
probe initially expected the wrong diagnostic label, corrected to the actual
pre-body count refusal. Initial client fixtures omitted required Alive motion;
adding their declared zero entry preserved their original valid-input/rejection
scenario. Superseded bulk schema now returns Schema before marker correlation.
An initial store crash run failed on PermissionDenied at the child marker rename;
its failure output previously hid the child error. The test now includes that
bounded child log in a panic. The unchanged crash workload passes in the final
permitted run. No crash boundary, TLS check or control ceiling was relaxed.
Existing incremental-cache hard-link warnings are environmental.

Changed files: protocol `src/{creature_owner,creature_owner_decode,codec,handshake,
lib,lifecycle_bulk,records,segment,transaction_attachments}.rs` and its three owner,
commit and attachment review test files; simulation `src/{sim,world,
creature_owner_bridge}.rs`; client `src/lifecycle_rejection_review.rs`; store
`src/dto.rs` and `tests/abrupt_crash.rs`; current protocol/validation/design/progress
and ASCII-appended task notes. No game catalog, renderer, habitat or physics solver
changes were needed.

Next unblocked task is ENG-218: complete canonical Simulation source/owner capture
with allocator epoch/high-water (even empty), atomic complete actor checkpoint and
baseline restoration, typed ordered journal/attachment and replica consumers, then
bulk producer scheduling and source-edit/fracture/terminal synchronization. Current
checkpoint refusal and closed production spawning remain. Combined 1024 m
weather/animals, shipped species population, GPU and multiplayer animal
restart/destruction gates are unrun; this milestone is not a launch acceptance.
Current V2 three-part fixture measurements: 99,144 control-body bytes versus the
65,536-byte cap; 99,169 raw lifecycle-envelope bytes and 409 compressed bytes.
These fixture measurements establish the retained bulk requirement, not shipped
trout/crow/deer asset cost or population throughput. Measurement log:
`.local/eng218-motion-measurements.log`. Final integrated evidence is
`.local/eng218-motion-final-tests.log`; client corrected evidence is
`.local/eng218-motion-client-refusal-retry.log`; workspace/lint/format logs use
`.local/eng218-motion-{workspace,clippy,format}.log`. Unrelated `artifacts/` remains
untouched. Workers exhausted their available service allowance during design;
the coordinator completed implementation, review and validation locally.

## Primary identity V3 and bounded snapshot envelope (2026-10-18)

Complete-capture review exposed a missing serialized invariant: after actual
fracture, the current primary can have a higher serial than an older surviving
component. Physics captures primary first, whereas portable records sort by
serial. Ownership V3 now explicitly retains `primary_component_serial`, bound
by full/topology hashes. Root pose, rotation and both twists must match that
component. None selects the sole intact component; derived identity must name a
nonzero existing serial below the unchanged serial high-water. Empty source parts
now preserve their source ID, authored hierarchy, bounds and sparse content hash
without fabricated cells; the aggregate still requires complete nonempty masks.

Changed owner/set/change/operation/commit/bulk types are V3, schemas/domains 3,
wire/handshake 11 and lifecycle bulk kind 8. Unchanged motion overlay V2, part
velocity V2 and source/component/pose V1 retain their layouts. There are no V2
owner aliases or migration. Geometry baseline 1 and store 3 retain their current
layouts; prior embedded wire 10 journals reject. This supersedes the previous
milestone's current-version numbers without claiming a complete checkpoint.

New `CreatureSnapshotV1` is a bounded portable envelope containing the existing
allocator snapshot, actual shared next-volume-ID counter and one current owner
set. Empty populations retain allocator world/epoch/high-water and retired gaps.
Full hash binds allocator and shared counter as well as ownership. Safe decoding
caps the input at 64 MiB + 128 bytes, reuses the cumulative ownership reader,
requires strict envelope/allocator maps, checks owner world/epoch and both ID
high-waters, validates catalog/partition semantics and refuses trailing bytes.
It introduces no extra allocator or stream. A future installer must also match the
geometry counter and enforce the shared geometry/source namespace atomically.
This pass implements the portable prerequisite, not canonical Simulation capture
or runtime installation. Those remain next; initialized-host geometry-only
checkpoint refusal and closed spawning are unchanged.

Meaningful regressions include derived serials 7/41 with actual primary 41 and
retired high-water 100, absent/invalid primary identities, each root motion field,
coincident component frames, empty bounded/unbounded source bindings and all-empty
aggregate refusal; snapshot empty/high-water/epoch/hash roundtrips, wrong world and
counter rejection, strict allocator metadata, trailing refusal and declared
population admission before invalid record bodies when a sequence hint exists.
Missing primary fields and superseded owner schemas refuse without migration.

Coordinator validation:
- `cargo test -p spall_protocol --test creature_owner_review`: final 28 passed.
- `cargo test -p spall_protocol -p spall_sim --lib --tests`: passed before the final
  additional schema/snapshot guard probes; the final integrated run below covers
  all those changes.
- `cargo test -p spall_protocol -p spall_sim -p spall_store -p spall_server`:
  permitted integrated run 644 passed, 0 failed, 12 existing ignored across 68
  result targets, including actual local multiplayer and abrupt-crash recovery.
- `cargo check --workspace --all-targets --all-features`: passed.

Initial new snapshot guard assertions failed twice because they assumed postcard
would preserve custom error text, then assumed it exposes a sequence size hint.
The final guard test uses an actual hint-exposing value decoder with invalid null
record bodies and requires the count-limit diagnostic before their decoding.
Postcard still uses the existing bounded incremental set reader; no implementation
limit or workload was weakened. Failed attempts remain in
`.local/eng218-primary-owner-tests{,-retry}.log`; corrected evidence is
`.local/eng218-primary-owner-tests-final.log`. Full integrated evidence:
`.local/eng218-primary-integrated-tests.log`. Existing incremental-cache hard-link
warnings remain environmental.

Changed files: protocol `src/{creature_snapshot,creature_owner,
creature_owner_decode,codec,handshake,lib,lifecycle_bulk,records,segment,
transaction_attachments}.rs`; protocol owner/commit/attachment review fixtures;
simulation `src/{sim,world,creature_owner_bridge}.rs`; client
`src/lifecycle_rejection_review.rs`; store `src/dto.rs`; current protocol,
validation, design/progress and ASCII-appended task notes. No game catalog,
renderer, solver, habitat or production spawn changes. Unrelated `artifacts/`
remains untouched.

Next unblocked task remains ENG-218: canonical read-only Simulation capture with
complete host/source/metadata/physics bijection, bounded population preflight and
actual applied Alive motion / whole-corpse masks. Stale source/fracture state must
refuse. Then implement jointly prepared host/source/physics/registry restoration,
current actor checkpoint/baseline sections and typed ordered journal/replica
consumers, bulk scheduling and source-edit/fracture/terminal synchronization.
Combined 1024 m weather/animals, shipped population/GPU and multiplayer animal
restart/destruction remain unrun and unaccepted.

Final checks also passed:
- `cargo test -p spall_client --lib lifecycle_rejection_review`: 2 passed.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed after correcting two
  formatting-only differences in the final test additions; initial format output
  remains `.local/eng218-primary-format.log`, final output is
  `.local/eng218-primary-format-final.log`.
- `cargo test -p spall_protocol --test creature_commit_review -- --nocapture`:
  12 passed; current V3 fixture 99,145 control-body bytes versus 65,536 cap,
  99,170 raw bulk-envelope bytes / 410 compressed. These are fixture measurements,
  not shipped species/population throughput. Logs use
  `.local/eng218-primary-{client-tests,clippy,measurements}.log`.
All Cargo commands used the established target-pinned wrapper and ran serially.


## Guarded complete-owner Simulation capture (2026-10-18)

Simulation now exposes read-only `capture_creature_snapshot(catalog)` returning
None only for a completely uninitialized host, or Some(snapshot), including an
empty initialized allocator with its world/epoch/retired high-water. It uses the
existing V1 envelope / V3 owners and actual shared volume counter, without a new
stream or allocator. This pass does not open geometry-only checkpoint paths or
production spawning.

Two-pass admission first bounds the whole population's record/source/brick and
nonuniform cell work before dense/paletted source scans. The next bounded pass
checks source metadata and complete resident revision fences, original sparse
content, source/geometry namespace, source grid work, solid counts, potential
component/mask counts, per-owner and cumulative bytes. Mask estimates include
geometric Vec capacity, BTreeMap node/key overhead and output component capacity;
Alive dense mask cells/outer masks reserve exact known counts. Conservative bounds
can refuse a population which exact mask counting might admit. These output
budgets do not replace the physics adapter's separately bounded reconstruction
scratch/grid ceilings; no measured total-heap or population-throughput claim.

Host/source/metadata/reverse-index/physics-owner maps must be a complete bijection,
including frozen-pose maps only for Corpse. Alive export checks original absolute
cell/material coverage, cell scale, revisions, volume/part identity and applied
hierarchy against the installed physics source parts. Corpse export uses actual
whole-owner capture for every disjoint component mask, frame/twist, owner/source
identity, component serial high-water and primary. The physical primary is retained
explicitly while output components remain canonically sorted. Original sparse
bricks, revisions, edited flags, bounds, authored transforms and empty source
bindings remain distinct from current/frozen physical poses. No lifecycle edits,
source synchronization or journal operations are synthesized by capture.

Read-only worker review found two important contract gaps. Host logical pose_epoch
and the raw physical authored pose_token are separate counters: initial physical
token zero is valid; Death increments only the host epoch. The explicit
`capture_dynamic_corpse_owner_at_epoch` adapter checks the actual raw physical
fence and emits the caller's authoritative logical epoch for cold reconstruction.
Raw tokens remain runtime-only; direct existing physics capture retains its
previous token-as-epoch behavior. Actual raw-zero birth and checked Death capture,
Alive/corpse reconstruction and subsequent mapped token admission pass.

Volume.next_revision is not generally reconstructible from resident bricks after
retirement/eviction. Current source DTOs omit it. This pass explicitly refuses any
such source with SourceRevision rather than write a lossy snapshot or invent a
max+1 counter. A current-only source high-water DTO plus exact restore seam is the
next ENG-218 prerequisite before general animal save acceptance. Sources whose
actual counter is representable are captured, including a genuinely empty source
with its initial counter. This limitation is deliberate and visible, not complete
Simulation checkpoint/baseline acceptance.

The reviewer initially found missing Alive physics/source equality and incomplete
mask-vector capacity accounting. Both were corrected before acceptance; a follow-up
read-only audit found no remaining concrete blocker within this guarded scope.
Tests exercise altered source content with valid refreshed metadata against an
unchanged physical rig, exact negative masks, pending refusal and mutation-free
failure; scalar metadata limits before stale hashing; independent native capacity
refusal at the admitted 8 Mi dense-slot ceiling; empty allocator capture; actual
three-part applied motion and checked transfer; source revision-gap refusal; and
multi-component primary 41 with older serial 7/high-water 100 plus an empty child.
The last Simulation case starts from a cold-restore fixture; actual fracture
creation/roundtrip remains independently covered by the existing physics tests.

Measured checks (all Cargo calls serial through `.local/eng218-cargo.ps1`):
- `cargo check -p spall_sim --lib`: final passed; initial compilation needed an
  explicit mask Vec type and removal of an unused import (no gate relaxed).
- `cargo test -p spall_sim --lib creature_`: final focused run 32 passed, 0 failed.
- `cargo test -p spall_voxel -p spall_physics -p spall_protocol -p spall_sim`:
  661 passed, 0 failed, 11 existing ignored across 60 result targets, including
  final Alive mapped-epoch reconstruction additions.
Logs: `.local/eng218-capture-{check,check-retry,check-source,focused,
focused-retry,focused-final,integrated}.log`. Existing cache hard-link warnings are
environmental. No GPU or generated-world launch/population measurement was run.

Changed files: new simulation `src/{creature_capture,creature_capture_tests}.rs`;
simulation `src/{creature_owner_bridge,creatures,lib,sim,world}.rs`; physics
`src/world.rs`; voxel `src/brick.rs`; protocol/validation/task notes and ENG-218
design/progress reports. Engine dependencies and wire11/baseline1/store3 layouts
are unchanged. Unrelated artifacts/ is preserved.

Next unblocked ID: ENG-218 source revision high-water format and exact reconstruction,
then joint prepared Simulation host/source/physics/registry restore, complete actor
checkpoint/baseline sections and typed ordered journal/replica consumers, bulk
publication and source-edit/fracture/terminal synchronization. Combined 1024 m
weather/animals, shipped population, GPU and multiplayer animal restart/destruction
remain unrun/unaccepted; production spawning remains closed.

Final coordinator checks passed:
- `cargo check --workspace --all-targets --all-features`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`.
- `cargo fmt --all -- --check` and `git diff --check`.
Logs: `.local/eng218-capture-{workspace,clippy,format}.log`.
No server/network regression rerun was needed for this pass: wire/store layouts
and server publication paths are unchanged; workspace compilation and strict lint
cover their callers. The current production checkpoint refusal remains unchanged.


## Exact source revision counters and reconstruction (2026-10-18)

This supersedes the previous guarded capture's SourceRevision representability
refusal. Current-only CreatureSourcePartV2 carries required next_brick_revision;
it is positive and strictly greater than every resident brick revision. Capture
writes the actual Volume counter. Source-content v2, full ownership v4 and topology
v4 hashes bind it; motion-only hashes exclude it. Death's validated static-source
comparison preserves the counter through the canonical source hash. Missing
fields and zero/brick-equal counters reject; no old-field default or migration.

Containing owner/set/operation/change/commit/bulk types and schemas/domains are V4,
source descriptors V2, snapshot envelope V2/schema2/domain2, wire/handshake12 and
lifecycle bulk kind9. Unchanged motion overlay/velocity V2 and local pose/component/
mask V1 keep their layouts. Geometry baseline1/store3 remain unchanged guarded
containers; superseded embedded wire11 frames reject. Actor-free terrain/body hash
contracts and existing topology enum discriminants remain unchanged.

`Volume::resume_revision_counter` is a checked monotonic resume primitive. Zero or
rollback leaves the counter, resident data and state stamp unchanged. A no-change
resume retains the stamp; advancing past retired revisions invalidates it. The
u64::MAX value is preserved as exhausted; editing cannot reuse or wrap it.

New pure `reconstruct_creature_sources(snapshot,catalog)` first validates the whole
bounded population, then builds unpublished saved-ID source volumes with original
bounds, cell sizes, resident brick contents/revisions/edited flags and finally the
exact recorded counter. Uniform bricks require no dense scratch expansion;
nonuniform reconstruction uses existing bounded per-brick scratch. It creates no
IDs, journal operations, owner registry or physical bodies and installs nothing in
a live Simulation. Joint host/source/physics/registry installation remains next.

Tests now cover actual solid edit -> modified-air tombstone -> retirement, capture
and codec roundtrip, source reconstruction and a next edit consuming the saved
counter instead of the old resident maximum. An emptied source child retains its
actual consumed revisions and uses the correct next value after reconstruction;
the physical two-component fixture still retains primary41/older7/highwater100.
An actual live source advanced to u64::MAX captures/reconstructs exactly and refuses
the next edit without mutation/reuse. Invalid reconstruction candidates leave live
sources and shared counters untouched. Protocol probes cover required fields,
source/full/topology-versus-motion hashes, and a fully rehashed Death payload that
illegally changes only the source counter. The Volume public test covers atomic
zero/rollback refusal, idempotent resume and subsequent revision50 consumption.

Read-only review verified capture -> field -> hashes/validation -> exact resume,
including transitive static-source binding, and reported no remaining concrete
blocker. Initial simulation compilation still referenced the removed SourceRevision
error in its old refusal test; that test was replaced with successful actual-gap
reconstruction coverage. No failing scenario, workload or size ceiling was reduced.

Focused measured checks, all via `.local/eng218-cargo.ps1`, serialized:
- `cargo test -p spall_protocol --lib --tests`: passed, including new source-counter
  hash/required-field and Death-tamper assertions.
- `cargo test -p spall_sim --lib creature_`: corrected focused run34 passed0failed;
  final integrated run also includes strengthened actual exhausted/empty counters.
- `cargo test -p spall_voxel --test revision_resume`: 1 passed, 0 failed.
Logs `.local/eng218-source-counter-{protocol,protocol-final,sim,sim-retry,volume}.log`.
Final wider checks/results are recorded below. Existing cache hard-link warnings
remain environmental.

Changed files: protocol src/{creature_owner,creature_owner_decode,creature_snapshot,
codec,handshake,lib,lifecycle_bulk,records,segment,transaction_attachments}.rs and
owner/commit/attachment tests; simulation src/{creature_capture,creature_capture_tests,
creature_owner_bridge,creature_source_restore,lib,sim,world}.rs; client lifecycle
rejection fixtures; store src/dto.rs; voxel src/{lib,volume}.rs and new
 tests/revision_resume.rs; protocol/validation/tasks and ENG-218 design/progress.
No engine dependency, species, renderer or launch configuration changed. Unrelated
artifacts/ remains untouched.

Next unblocked task ID remains ENG-218: jointly prepared atomic Simulation restore
of host/source/physics/registry state using the existing complete snapshot, then
actor checkpoint/baseline sections and typed ordered journal/replica consumers,
bulk producer and source-edit/fracture/terminal synchronization. Production spawn
and geometry-only checkpoint guards remain. Combined1024m weather/animals, GPU,
shipped population and multiplayer animal restart/destruction remain unrun and
unaccepted. Source-only reconstruction is not complete animal save installation.

Final integrated evidence for the source-counter pass:
- `cargo test -p spall_voxel -p spall_physics -p spall_protocol -p spall_sim -p spall_store -p spall_server`:
  909 passed, 0 failed, 14 existing ignored across 89 result targets, including
  real local multiplayer/restart and abrupt-crash recovery at the existing workload.
  This is separate from unrun multiplayer animal installation/recovery acceptance.
- Integrated log: `.local/eng218-source-counter-integrated.log`; all current counter
  capture/reconstruction tests and actual empty/exhausted source strengthening are
  included. No physics/TLS/crash gate or workload was reduced.

Other final checks passed:
- `cargo test -p spall_client --lib lifecycle_rejection_review`: 2 passed0failed;
  validated V4 actor input still refuses atomically at the closed replica path.
- `cargo check --workspace --all-targets --all-features`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`.
- `cargo fmt --all -- --check` and `git diff --check`.
- `cargo test -p spall_protocol --test creature_commit_review -- --nocapture`:
  12 passed; current V4 three-part fixture 99,148 control-body bytes versus
  65,536 cap; 99,173 raw bulk envelope / 412 compressed bytes. This retains the
  real bulk-producer requirement and is not a shipped species/population cost.
Logs `.local/eng218-source-counter-{client,workspace,clippy,format,measurements}.log`.
All Cargo commands used the established target-pinned wrapper and ran serially.


## 2026-10-18 joint cold Simulation restoration

Implemented the next ENG-218 prerequisite in `spall_sim`: public opaque
`prepare_creature_restore` / `commit_prepared_creature_restore`, restoring the
existing complete CreatureSnapshotV2 after geometry recovery. This is a cold
installer, not a checkpoint/baseline/replay consumer. It requires an uninitialized
host and all five source indexes empty, explicit expected WorldId, exact geometry
next-volume counter and disjoint geometry/source IDs. The existing geometry
commit fence binds world identity, physics step/slots/body state, terrain revision,
topology, origin, every registry counter and journal cursor. Commit repeats the
source namespace and bounded catalog fingerprint checks before physics validation.

The complete source population is reconstructed with saved IDs, sparse original
bricks/bounds/cell size/revisions/edited flags and next revision counters. Physical
source spans/solids are admitted for the entire population before grids or mask
cell vectors are allocated, under the existing 4M owner / 16Mi batch dense-cell,
1M owner / 4M batch solid and 1024-owner ceilings. Original empty sources remain
empty bindings. Saved world positions must roundtrip exactly through the recovered
physics frame; no silent authoritative position rounding is accepted.

Living owners restore applied local poses, independent measured part twists,
root motion, stable owner key, source generation, topology and logical pose epoch.
Corpse components restore independent masks/poses/twists, generation and serial
high-water, including primary41/older7/high-water100. Only the runtime DTO is
primary-first; portable components remain serial sorted. The complete CreatureHost
uses the saved allocator and copies existing stable records below its high-water,
without allocating IDs or deriving replacement counters.

All source/provenance/reverse-owner/frozen-pose maps and physics-owner slots are
staged before publication. After every participant passes validation, physics
publishes, existing owner-handle slots are filled, the complete source store is
swapped and the complete host is installed, with no later Result path or source/
host map-node allocation. No shared counter changes or journal append. Empty
snapshots use the same fences and retain retired allocator epoch/high-water with
no physics batch; initialized empty targets refuse replacement.

Read-only review found no concrete blocker, including a separate follow-up of
exact origin conversion. Tags remain strictly 1..N in the same order as staged
keys; runtime physics handles remain unpublished/private and are never serialized.

Focused final check through the target-pinned wrapper:
`cargo test -p spall_sim --lib creature -- --nocapture`: initial corrected run47
passed; final strengthened run48 passed, 0 failed.
It includes codec -> complete joint restore -> recapture equality/hash for mixed
Alive/Corpse, actual three-part animation and checked Death, fragmented cold
fixture with nonlowest primary/empty child and serial gaps, empty allocator,
and subsequent logical pose-token admission. The fragmented Simulation fixture
starts from ColdRestore; actual fracture remains separately covered by physics.
Refusal cases: stale step, different Simulation, changed registry/terrain/catalog,
initialized host, wrong world, geometry source collision, huge sparse source span,
invalid second physical owner and occupied physical owner key. Existing physical
motion and source/host emptiness, counters and journal remain unchanged on refusal.
Alternate exact origins roundtrip; changed origins invalidate prepared candidates
and lossy origins refuse during prepare. Final strengthening also roundtrips actual
retired revision gaps and u64::MAX through the joint installed store, and refuses
oversized unused catalog entries/clip names on the empty-population path.

Initial compile attempts corrected an Option-to-Result conversion and test-only
terrain access/BodyState comparisons. One initial focused run had 46 passes and
one failure because its new test expected an existing physical owner-key conflict
to survive prepare and fail at commit; physics already rejects it earlier. The
assertion now requires that stronger refusal and unchanged state. No gate, workload,
TLS/crash condition or size limit changed. Retained initial result log:
`.local/eng218-joint-restore-animals-attempt1.log`; final focused log:
`.local/eng218-joint-restore-animals.log` and strengthened final
`.local/eng218-joint-restore-animals-final.log`.

Changed implementation/test files: simulation src/{creature_restore,
creature_restore_tests,creatures,world,sim,lib,creature_capture,
creature_capture_tests,creature_owner_bridge}.rs, plus the source reconstruction
module's now-current joint-installer comment. Updated architecture/protocol/
validation/tasks and this progress/design evidence. No wire12, baseline1, store3,
dependency, species or renderer layout change. Unrelated artifacts/ preserved.

Next unblocked task ID remains ENG-218: complete actor checkpoint/baseline
sections with explicit format versions, atomic durable recovery and typed ordered
journal/attachment/replica consumers, then bulk production and source-edit/fracture/
terminal synchronization. Production spawning and initialized-host geometry-only
checkpoint refusal remain. Combined1024m weather/animals, shipped population/GPU
and multiplayer animal restart/destruction acceptance remain unrun and gated.


Wider measured checks (all Cargo commands via `.local/eng218-cargo.ps1`, serialized):
- `cargo test -p spall_voxel -p spall_physics -p spall_protocol -p spall_sim -p spall_store -p spall_server`:
  916 passed, 0 failed, 14 existing ignored across89 result targets. Actual local
  multiplayer/restart and abrupt-crash suites retained their existing workloads.
  Log `.local/eng218-joint-restore-full.log`. This run preceded the final test-only
  revision-gap/MAX assertions and unused-catalog case; final focused48 and full
  simulation recheck below cover those additions. No implementation changed after
  this full suite other than formatting and a module comment.
- `cargo check --workspace --all-targets --all-features`: passed; log
  `.local/eng218-joint-restore-workspace.log`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`:
  passed; log `.local/eng218-joint-restore-clippy.log`.
Existing incremental-cache hard-link warnings remain environmental. Complete
animal checkpoint crash cuts and two-client animal installation/recovery remain
separate unrun gates; current geometry-only recovery tests do not establish them.

- Final `cargo test -p spall_sim`: 273 passed, 0 failed, 9 existing ignored
  across31 result targets, including the final test-only strengthening; log
  `.local/eng218-joint-restore-sim-final.log`.

- `cargo test -p spall_client --lib lifecycle_rejection_review`: 2 passed,
  0 failed; closed replica/recovery consumers still refuse actor-bearing input
  before geometry, clocks or queues mutate. Log `.local/eng218-joint-restore-client.log`.
- `cargo fmt --all -- --check` and `git diff --check`: passed; format log
  `.local/eng218-joint-restore-format.log`. No GPU or launch process was started.


## 2026-10-19 complete animal checkpoint section

Implemented current-only store schema4, replacing3 without migration. Every
checkpoint transaction now includes a required CRC-protected animal row and
explicit host-presence flag, including a strict None row for uninitialized worlds.
Some(StoredCreaturesV1) holds the existing bounded SnapshotV2, full snapshot hash
and finite physics origin. Geometry/environment/cursor/complete marker and the
new row publish in the same transaction and existing checkpoint failure sites.
Original source volumes stay in the existing animal snapshot, never fake Bodies.

Store publication admits bounded source/population encoding and validates schema,
full hash, saved world/counter agreement, source cell-size coverage, original
source content hashes and geometry/source namespace disjointness. Length is queried
before materializing SQLite BLOBs; strict trailing framing and the seeded nested
reader apply. Missing/empty/oversized/bad-CRC/flag-mismatched or inconsistent sections
are corruption, not an absent population. BEGIN IMMEDIATE serializes checkpoint and
journal guards. Both writes validate the prior required section's bounds, checksum,
framing, presence and bounded hash. Actor checkpoints cannot drop an initialized
host or rewind the durable high-water (including retained checkpoint cursors after
journal pruning). Until typed actor replay exists, no actor checkpoint with a suffix
or subsequent journal append is accepted; actor-free suffix behavior is unchanged.

Catalog-aware persist::capture_with_catalog / restore_with_catalog connect the
existing complete Simulation capture and prepared joint installer to actual files.
Catalog-free APIs continue to refuse animal state. Restore validates game references
before reconstructing, builds geometry detached in the saved origin, checks composed
topology, installs the joint candidate and verifies exact complete recapture hash
before returning the Simulation. Any actor-bearing checkpoint with a nonempty suffix
refuses; no journal row is skipped or reinterpreted.

Snapshot topology now has a separate v1 domain binding allocator/high-water and
owner topology, excluding sampled motion. Checkpoint composition version3 combines
it with geometry v2 only for initialized hosts; uninitialized geometry vectors stay
unchanged. Full SnapshotV2 hashes bind motion separately. An explicit partial
geometry hash supports composition; source-bearing ordinary world_hash now refuses
in production as well as tests. No wire12/baseline1 or ownership DTO layout change.

Catalog-independent population admission was factored from existing validation,
keeping the same aggregate ceilings before encoding/hashing. A concrete prerequisite
bug was found: set uniqueness used complete generational keys but CreatureHost maps
by CreatureId, so two generations of the same ID could collapse in a restored host.
Snapshot validation now rejects duplicate CreatureId before materialization; a
fully rehashed storage case proves refusal without a checkpoint write. Catalog
validation reuses admitted aggregate work through a crate-private helper.

Recovery preserves the existing explicit corruption policy. Structural section
failures are fallback-eligible at candidate loading, with a corruption report and
explicit RecoveryChoice requirement. Semantic catalog/physics/hash failures return
an error and do not silently rewind. RecoverBase::AtOrBefore(tick) reloads the chosen
prior checkpoint and its own suffix; never combine previous_checkpoint with a suffix
loaded relative to the newer checkpoint. The SQL selection filters its tick bound.

Read-only review identified missing-row writer guard and pruned journal high-water
holes; both are corrected, with bounded prior-row CRC/framing checks and dedicated
regressions. Final follow-up found no concrete blocker in the stated checkpoint
scope. No worker edited files or ran Cargo during the review.

Focused measured check (target-pinned wrapper):
`cargo test -p spall_server --test animal_checkpoint -- --nocapture`: 12 passed,
0 failed. Log `.local/eng218-checkpoint-animals-final.log`. Covers mixed Alive and
fragmented Corpse, initialized-empty allocator, exact independent motion/original
sources/primary/serial/counters, saved origin, all checkpoint fault points, row
corruption/fallback, metadata/catalog/hash and generation identity refusal, suffix
closure, prior-row corruption, retained/pruned cursor and explicit older selection.
The physical fragmented fixture begins from ColdRestore; actual fracture remains
separately exercised in the physics suite. FaultPlan checkpoint cuts model controlled
failures; real-process actor crash/restart is not claimed from those tests.

Initial compile-only fixture names AnchorPlane::at_y / set_fault_plan were corrected
to the existing at / set_faults APIs. Two focused attempts had6pass1fail because the
new whole-checkpoint recapture equality included SnowState's existing dirty flag,
which from_canonical_bytes intentionally sets for surface republication. Animal
snapshot, geometry, metadata, weather and other fields already matched exactly.
The test now explicitly compares those fields and all canonical snow state after
that existing reload normalization; no animal comparison, workload, size limit or
physics condition was reduced. Initial logs `.local/eng218-checkpoint-animals-compile.log`
and `...-animals-attempt{1,2}.log` retained.

First full six-package run:471 passed,1 failed,2 existing ignored across39 completed
result targets before Cargo stopped. Failure was only the prior hash-version test
asserting current version2 after the explicit checkpoint composition advanced to3.
It now requires version3 and rejects both older versions1/2, with no migration or
hash substitution. Log `.local/eng218-checkpoint-full-attempt1.log`; corrected full
regression results follow below.

Changed files: protocol src/{creature_owner,creature_snapshot}.rs; simulation
src/world.rs; store src/{creature_checkpoint,db,dto,lib,recover,schema}.rs and
 tests/{durability,abrupt_crash}.rs; server src/{persist,persist_pipeline}.rs and
 tests/{animal_checkpoint,persist_pipeline,persistence}.rs; protocol/architecture/
validation/tasks and ENG-218 design/progress. Unrelated artifacts/ preserved.

Next unblocked task ID remains ENG-218: typed actor journal/attachment and replica/
baseline consumers with atomic ordered suffix replay, then bulk production and
source-edit/fracture/terminal synchronization. Live animal scheduling/spawning
remain closed. Combined1024m weather/animals, shipped population/GPU, actual process
animal crash/restart and two-client destruction/recovery remain unrun/unaccepted.


Final measured checks (all Cargo commands through target-pinned
`.local/eng218-cargo.ps1`, serialized in G:\Programming\voxel_engine\target):
- `cargo test -p spall_voxel -p spall_physics -p spall_protocol -p spall_sim -p spall_store -p spall_server`:
  corrected run929 passed,0 failed,14 existing ignored across90 result targets.
  Includes all12 new animal checkpoint scenarios plus existing actual local
  multiplayer/restart/abrupt-crash workloads. Those existing process tests are
  actor-free and do not establish real-process animal suffix recovery.
  Log `.local/eng218-checkpoint-full.log`.
- `cargo check --workspace --all-targets --all-features`: passed;
  `.local/eng218-checkpoint-workspace.log`.
- `cargo clippy -p spall_protocol -p spall_physics -p spall_sim -p spall_store -p spall_server -p spall_client -p sandbox --all-targets -- -D warnings`:
  passed; `.local/eng218-checkpoint-clippy.log`.
- `cargo test -p spall_client --lib lifecycle_rejection_review`:2 passed,0 failed;
  `.local/eng218-checkpoint-client.log`. Closed consumers still reject valid
  actor payloads before geometry/clocks/queues change.
Existing incremental-cache hard-link warnings are environmental. No engine
runtime dependency changed and no GPU/game launch process was started. Exact
final format/diff checks follow. ENG-218 remains in_progress.


Final inspection caught a new full-capture risk in streamed worlds: stored_bricks
copies residents while geometry_topology_hash includes retained evicted digests.
The new catalog-aware full-capture API now refuses world.has_evicted() before
copying geometry/actors. A regression actually evicts a terrain brick, retains the
same logical geometry hash, requires capture refusal and checks the complete
actor snapshot is unchanged. Read-only review found the world-wide (terrain and
body) guard sound. The existing incremental ResidencyPass capture still has no
actor/catalog participant and stays guarded; live catalog/residency/backing handoff
is an explicit next ENG-218 prerequisite before streamed1km animal saves. The
catalog-free capture error now correctly describes missing catalog participation
rather than claiming store4 lacks a complete section.

The six-package929-pass run and initial workspace/strict lint results above precede
this final small guard and its thirteenth checkpoint test. A complete server rerun
and refreshed workspace/lint/format checks validate the final state below; no
other production behavior or test workload changed.


Final state after the evicted-geometry guard:
- `cargo test -p spall_server`:212 passed,0 failed,2 existing ignored across25
  result targets, including all13 animal checkpoint tests and actual unchanged
  server multiplayer/recovery scenarios. Log `.local/eng218-checkpoint-server-final.log`.
- Refreshed `cargo check --workspace --all-targets --all-features`:passed;
  `.local/eng218-checkpoint-workspace-final.log`.
- Refreshed strict seven-package `cargo clippy` command above:passed;
  `.local/eng218-checkpoint-clippy-final.log`.
- `cargo fmt --all -- --check` and `git diff --check`:passed;
  `.local/eng218-checkpoint-format.log`.
All checks use the same established target-pinned wrapper. The six-package929-pass
run predates only this final small refusal guard/test and error-text clarification;
full final server and refreshed workspace/lint checks include them. No crash/TLS/
workload or size gate was relaxed. No 1km/GPU/population or live animal consumer
acceptance follows. Next remains ENG-218 live catalog/residency checkpoint handoff,
typed ordered actor journal/attachment and baseline/replica consumers, then bulk
and source-edit/fracture/terminal synchronization.
