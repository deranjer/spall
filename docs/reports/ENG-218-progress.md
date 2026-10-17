# ENG-218 progress: ordered animal consumers

Date: 2026-10-17. ENG-218 remains in progress. This is measured partial
implementation evidence, not acceptance of animal replication or recovery.

Current accepted main protocol/wire is 9: inline lifecycle framing plus a
bounded typed bulk marker/codec. Dispatch and production spawning remain closed;
the authoritative producer draft is isolated pending validation. Earlier sections
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
