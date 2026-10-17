# ENG-218 progress: ordered animal consumers

Date: 2026-10-17. ENG-218 remains in progress. This is measured partial
implementation evidence, not acceptance of animal replication or recovery.

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

## Work under review

The dedicated animal-consumers checkout contains the protocol commit wrapper
and exact physics reconstruction work. Neither is accepted as a completed
consumer slice. Review requires an outer receipt binding the unchanged ENG-217
change, authoritative death evidence and source-fenced pre-motion snapshot.
Missed motion datagrams must not prevent death; stale source topology must still
reject. Separate topology and motion hashes must explicitly bind lifecycle and
preserve actor-free legacy hashes.

Cold physics restoration must preserve exact original source identities and
independently moving corpse components, including a single surviving derived
component and retired serial high-water. Empty lifetime source bindings must
remain reserved; all shapes and mass properties must prepare before publication.
Restoration must not rerun death/fracture or manufacture a one-volume body.

Next unblocked task: continue ENG-218 through the real typed ordered operation,
bounded correlated bulk transport, complete baseline/segmented staging, and
checkpoint/journal recovery consumers. Production animal spawning remains
disabled. Source edit/fracture synchronization, species/current habitat and
integrated two-client restart/destruction/performance proof still follow.
