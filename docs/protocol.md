# Multiplayer, persistence, and recovery contract

This is a proposed version-1 contract. T01 turns these records into tested DTOs before dependent tasks use them. User requirements: multiplayer and full-world destruction are essential.

## Authority and transport

The server owns world edits, support/fracture decisions, body identity, physics, and persistent state. Clients send input and action requests. Local play starts the same server with loopback transport; it does not bypass validation or transaction creation.

Use Quinn reliable streams and datagrams. One reliable ordered control/topology stream per client keeps initial event ordering simple. Separate bounded bulk streams carry baselines; bulk work must not starve control and motion. Limit concurrent bulk streams to four initially, with application priority and byte budgets. QUIC congestion control alone is not game traffic prioritization.

Datagrams carry player inputs and body/player motion. Each payload fits `min(1100, connection.max_datagram_size())` bytes after our envelope; recalculate when path limits change. Fail the connection setup clearly if required datagrams are unavailable. Larger records are split into independently useful snapshots, not IP-fragmented giant packets. Reliable application records use explicit length framing and checked decoding.

Handshake includes protocol version, exact content manifest hash, world ID, generator version, supported algorithms, server tick rate, cell-size codes, session identity, and negotiated limits. Reject incompatibility before creating a player. For development use server certificate fingerprint pinning and a session join token; do not disable TLS verification globally. Dedicated Internet hosting is direct-address initially; QUIC does not provide NAT traversal, a relay, matchmaking, or account identity. Those need an explicit later platform choice.

## Minimal record families

| Record | Required fields and meaning |
| --- | --- |
| InputFrame | session/player, input sequence, intended server tick estimate, movement axes, view direction, held buttons; include up to three recent frames redundantly |
| ActionRequest | unique request ID, input sequence, action/tool type, aim, claimed target; server validates all claims |
| ActionStatus | request ID, queued/rejected/committed status, reason or TransactionId; queued is not committed |
| TopologyTransaction | TransactionId, server tick, connection stream sequence, algorithm version, dependencies, before/after revisions, ordered operations, canonical result hashes |
| MotionSnapshot | server tick, snapshot sequence, acknowledged input sequence, body/player ID, topology revision, pose, velocity, angular velocity, sleeping flag |
| BaselineBegin/Part/End | transfer ID, interest epoch, checkpoint tick, journal cursor, world/content versions, byte counts, part hashes, region/volume manifest |
| BaselineAck | transfer ID, verified manifest hash and installed cursor |
| RepairRequest | object/brick key, expected/current revision and hash; rate-limited |
| DurableThrough | highest contiguous journal sequence flushed to durable storage; distinct from simulation commit |

Never serialize Rapier handles, ECS entities, pointers, platform-sized integers, or raw Rust structs. DTOs use explicit numeric sizes, tags, units, and bounds. Version the wire schema independently of the world save schema. Protocol mismatch is initially a clear rejection, not speculative compatibility code.

Limits start at 64 KiB per control record, 1 MiB per bulk part, 64 MiB per assembled transfer, and 256 KiB maximum decompressed data per material-only brick record (actual material payload is 64 KiB). Validate counts before allocation and decompress with output bounds. A multi-volume transaction can span staged bulk parts; its visible commit marker is small. Larger regions are split into multiple dependency-complete transfers. Each connection has bounded staging memory and a timeout.

## Exact geometry, approximate motion

Use a hybrid representation for topology changes:

- Replicate compact deterministic local integer brush operations when the client's source revision matches exactly.
- Encode explicit cell/material runs for irregular changes and repairs.
- The server chooses split membership, child IDs, ownership transfer, and resulting transforms. A split uses canonical source-cell ranges or a baseline blob, not a client rerun of floating-point physics or support heuristics.
- Use the smaller valid encoding, bounded by staging limits. Do not send full modified chunks every frame as the default.

T10 implements this for the all-resident G1 case: a committed `TopologyTransaction` carries `[IntegerBrush, (SplitOff, child-fill CellRuns)*, source-removal CellRuns]` — the brush op plus, for each detached component, a `SplitOff` marker and the canonical `+X` `CellRun`s that fill the new child volume, then the runs that clear those cells from the source. A replica applies the ops in order into a candidate, checks the `before` revisions and every `result_hash`, and publishes atomically; a `before` gap raises a rate-limited `RepairRequest` and the server answers a brick repair with `CellRun`s for that brick's current state. A split whose runs would exceed `MAX_TRANSACTION_OPS` is refused at commit — the baseline-blob alternative for a giant split is T17.

Physics is not lockstep. Do not promise identical trajectories from running the same inputs on every machine. Rapier's determinism guarantees have conditions, and whole-engine determinism requires more than selecting a physics feature. [Rapier determinism documentation](https://rapier.rs/docs/user_guides/rust/determinism/).

Canonical topology hashes include sorted volume IDs, cell-size codes, brick coordinates, authoritative layer bytes, ownership, and revisions. Exclude motion, render caches, and library allocation order. Specify little-endian encodings and stable sorting in T01; BLAKE3 over the canonical bytes defines the result. Hashes detect divergence; baselines repair it.

## Transaction application

1. Server constructs a complete transaction against specific input revisions. All source changes, children, mass/pose metadata, and required server collision updates are ready before commit.
2. Server commits once at a tick boundary and journals the complete authoritative result. Repeated ActionRequest IDs return the existing status and cannot perform another cut.
3. Client stages all parts and checks source revisions, IDs, length limits, and hashes. It applies operations in the encoded order into a candidate replica.
4. Publish the candidate only when the whole transaction validates. Retain the previous consistent replica while dependencies or derived client collision data are pending. Local visual feedback may show a pending action but must not change authoritative replica geometry.
5. Duplicate transactions are ignored using session/stream sequencing and IDs. Unexpected source revisions trigger bounded repair, not guessed replay. A failed candidate cannot partly replace live state.

A motion snapshot may arrive before its body's creation or refer to a newer topology revision. Keep at most one newest pending snapshot per unknown body for a short bounded window; apply only after the required topology arrives. Ignore snapshots older than the installed revision or last applied tick. Destruction/tombstone messages prevent late packets from resurrecting bodies. IDs are not reused.

Filtered replication uses per-connection stream sequences; gaps in global TransactionIds are normal. If a cross-region transaction affects a subscribed object and an unsubscribed dependency, first supply a complete dependency baseline or an authoritative post-transaction replacement for the subscribed view. Never ask a client to replay a brush over geometry it has not received. Large bodies are relevant if their bounds intersect interest, even if their centres are far away.

## Prediction and interpolation

Server consumes at most one validated input frame per player/tick. Reject invalid values, excessive look/movement rates, excessive lead/lag, and duplicates. Reuse recent held movement briefly on loss; clear it after a 250 ms silence timeout. Edge-triggered tool actions use reliable deduplicated ActionRequests.

The local client keeps a bounded input/state history, predicts capsule movement, and replays unacknowledged inputs after an authoritative correction. Collision topology changes invalidate affected history: restore the authoritative player state and rebuild prediction against a known revision. Do not replay movement blindly through geometry that no longer exists.

Render remote motion initially about 100 ms behind the latest estimated server time. Interpolate known states; extrapolate for at most 100 ms and then hold, with visible-lag metrics. Never derive authoritative destruction from that extrapolated pose. The server owns pushing/crushing outcomes. Cosmetic particles can be predicted and discarded freely.

No rewind of destructible world history for competitive lag compensation in v1. Server-time hit validation is an explicit latency tradeoff. PvP-grade historical voxel/body hit testing is a separate feature, not an assumed property of player prediction.

## Late join, interest changes, and reconnect

1. Capture an immutable, dependency-complete regional snapshot at tick T and journal cursor J. Include relevant terrain, modified-air tombstones, body geometry/poses, authoritative material/bond layers, and required manifest entries.
2. Continue simulation. Retain subsequent relevant transactions in a bounded catch-up queue while sending compressed, hashed baseline parts on bulk streams.
3. Client validates and installs the baseline atomically, then applies subsequent topology records in order to a declared barrier cursor. Motion samples are held until their topology exists.
4. Server sends a current motion keyframe for those objects. Permit player interaction only when local collision neighborhoods and the barrier are ready.
5. If the client cannot catch up within memory/time limits, cancel the transfer and create a fresher snapshot. Apply a retry limit and clear diagnostic. Do not require replaying all edits since world creation or permanently disable late join after heavy destruction.

Interest re-entry follows the same protocol. Unsubscribing evicts only the replica view, not the persistent entity. Reconnect uses a new session generation; old queued inputs and packets are invalid. Cached content may be reused only after matching world/manifest/revision hashes.

## Persistence

Use SQLite transactions through one I/O writer. Store metadata, versioned compressed brick payloads, volume/body records, spatial references, checkpoints, and an ordered authoritative journal. Configure and verify `journal_mode=WAL` and `synchronous=FULL` on the writer in T16. Group pending journal records into a database transaction, and acknowledge durability only after its successful commit. Keep the database on local storage. SQLite permits one WAL writer at a time and distinguishes commit durability from checkpointing. [SQLite WAL documentation](https://sqlite.org/wal.html).

An engine checkpoint is a coherent saved simulation snapshot. A SQLite WAL checkpoint transfers database WAL pages; these are different operations. Bound reader transaction lifetimes and schedule database checkpoint work off the simulation thread. Include WAL size and flush/checkpoint latency in persistence metrics. T00 must select a released SQLite build with applicable upstream WAL fixes, including when using rusqlite's bundled library.

Required world metadata: schema version, WorldId, seed, generator version, material manifest/hash, cell-size codes, next-ID counters, checkpoint tick/cursor, and structural algorithm versions. Body records store stable IDs, voxel geometry references, pose/velocity, sleep state, damage/bond state, and mass inputs. No physics library internals are the primary save format.

Initially checkpoint the bounded scene from one immutable tick snapshot every 30 seconds and on clean shutdown; journal topology transactions between checkpoints. Journal periodic body pose batches at 20 Hz. Each topology journal transaction also contains the participant body states required at that transaction, so geometry edits to moving bodies recover with the correct ownership/frame even if earlier motion batches are missing.

Group disk flushes with a target interval <=100 ms, then emit DurableThrough. A simulation commit may precede durability. A crash can lose the unflushed suffix and rewind motion to the latest durable pose batch; this is the explicit initial durability model. Clean shutdown waits for a final checkpoint/flush. Never tell an automation that a save succeeded before durable acknowledgement. If the storage queue exceeds its limit or disk writes fail, stop accepting persistent edits and return an error; do not silently continue an unsavable world.

Checkpoint publication records all brick/body data and its journal cursor in one DB transaction. Recovery loads the latest complete checkpoint and replays the durable ordered journal suffix. Ignore no interior corrupt record: report corruption and offer the previous valid checkpoint as an explicit recovery choice. A crash must not leave the terrain removed without the corresponding child body.

Retire journal records only after a newer durable checkpoint covers them and no snapshot transfer needs them. Cap retention; lagging joins get a fresh baseline. Persist modified-air bricks explicitly so procedural regeneration cannot restore mined blocks. Initial baselines transfer authoritative terrain rather than trusting client-side generation to be bit-identical.

Use versioned DTO migrations with fixture worlds before changing save formats. Unknown newer schemas are rejected without modifying the database. Back up/migrate into a separate database and validate before replacement.

## Network budget and overload behavior

Provisional per-client steady-state egress target: <=256 KiB/s, measured including topology and motion; baseline transfer has a separate capped 1 MiB/s budget. At eight clients this is already about 16.8 Mbit/s server steady egress before overhead. Track both directions and actual transport bytes.

Budget example: 200 relevant bodies x 48 encoded bytes x 20 Hz = 192,000 bytes/s before framing, topology, player records, and retransmission. Therefore not all bodies can receive 20 Hz updates continuously. Prioritize players, imminent contacts, nearby moving bodies, and recently changed structures; send distant/sleeping bodies less frequently with periodic keyframes.

Never discard committed topology to meet the motion budget. Drop superseded unsent motion snapshots; cap cosmetic events; throttle expensive actions; repair or disconnect a client whose reliable backlog exceeds the bounded window. Record backlog bytes and age. The stress gate must show that the queue drains after a blast and a new player can join while the server continues running.

## T01 wire encoding decisions (implemented)

`spall_core` and `spall_protocol` implement the record families above. The
following are now fixed and must be reused, not re-derived, by dependent tasks.
`spall_protocol` version-0 headers use `WIRE_SCHEMA_VERSION = 1` and
`PROTOCOL_VERSION = 1`.

**Endianness.** Every canonical hash pre-image and every frame header field is
little-endian, fixed width. Canonical sequences carry a `u32` LE length prefix;
blobs carry a `u32` LE byte-length prefix. `f32`/`f64` are encoded from their
IEEE-754 bit pattern after a finiteness check; `-0.0` is normalized to `0.0`.

**Framing.** A reliable/datagram record is `u16 LE schema version` `||`
`u16 LE wire tag` `||` postcard body. Decoding refuses input longer than the
channel limit before parsing, then requires the tag/schema to match the
expected record type and rejects trailing bytes. `Record::validate` then
enforces counts, ranges, and float finiteness; `TopologyTransaction` also has
`validate_against(&MaterialManifest)` for unknown-material rejection.

**Wire tags.** `InputFrame=1`, `ActionRequest=2`, `ActionStatus=3`,
`TopologyTransaction=4`, `MotionSnapshot=5`, `BaselineBegin=6`, `BaselinePart=7`,
`BaselineEnd=8`, `BaselineAck=9`, `RepairRequest=10`, `DurableThrough=11`,
`Handshake=12`. Discriminants are permanent; new families take new numbers.

**Sort orders for the canonical topology hash.** Volumes ascending by
`VolumeId`; bricks ascending by `(z, y, x)`; authoritative layers ascending by
numeric layer `kind`. Owner is encoded as `0,u64=0` for terrain or `1,u64=EntityId`
for a body. Domain-separated with the ASCII tag `spall.topology.v1`; the content
manifest hash uses `spall.manifest.v1`. BLAKE3 over the resulting bytes is the
result. Reordering the inputs does not change the digest.

**Brush fixed-point units.** Brush centre and radius are expressed in the
target volume's local cell space with 8 fractional bits: `1` unit = `1/256`
cell (`BRUSH_UNIT = 256`). A sphere removes a cell when the squared distance
from the brush centre to the cell centre is `<=` the squared radius, computed
in `i128` so no in-range input overflows. Maximum radius is 256 cells.

**Pose quantization.** Translation is transmitted as three IEEE-754 `f64`
metres and only finiteness is enforced (authority positions are `f64`; they are
not lossily quantized). Orientation is a unit quaternion quantized to four
`i16` components with scale `32767`; the decoder renormalizes and rejects a
zero-magnitude quaternion.

**Session and stream sequencing.** `SessionId` is a `u64`: high 32 bits are the
connection slot, low 32 bits a generation. `SessionRegistry::open` bumps the
generation on every (re)connect, so a reconnect produces a strictly newer
session and `accept` rejects any record stamped with an older generation.
`SequenceGate` accepts strictly increasing per-stream `u64` sequence numbers,
reports the size of any skipped gap, and rejects duplicates and regressions;
gaps in global `TransactionId`s on a filtered stream are expected.

**Size limits.** 64 KiB per control record (header included), 1 MiB per bulk
part, 64 MiB per assembled transfer, 256 KiB max decompressed material-only
brick record (64 KiB actual payload), 1100 B per datagram payload. Count
ceilings: 3 redundant inputs per frame, 4096 transaction ops / refs, 8192
baseline regions, 4096 baseline parts.

## T09 transport adapter (implemented)

`crates/spall_net` layers Quinn/QUIC on the T01 records. It owns transport only:
no simulation, storage, or rendering. ALPN is `spall/1`; TLS is 1.3-only.

**Sessions.** Development uses server-certificate **fingerprint pinning**
(BLAKE3 of the certificate DER, carried out of band) plus a per-run 32-byte
**join token** (constant-time compared). A wrong certificate fails the QUIC/TLS
handshake (`TransportError::Connect`); a wrong token or an incompatible
`Handshake` fails after TLS with a distinct `AuthReject::{BadToken,
Incompatible(field)}` sent back to the client before the connection is torn
down. Authentication is a `spall_net`-local `ClientHello { token, handshake }` /
`ServerAuthReply` exchange at the head of the control stream — not a frozen wire
record — after which the same stream carries `NetMessage`s.

**Channels.** One reliable ordered **control stream** per connection carries
`NetMessage` envelopes: `Heartbeat { seq }`, `Record { seq, <T01 frame> }`, or
`Bye`. The inner record keeps the exact `schema | tag | body` frame; the
envelope adds a per-stream `u64` sequence. **Bulk streams** are capped at four
per connection and carry length-framed `BaselinePart` records with the per-part
and assembled-transfer ceilings enforced during read. **Datagrams** carry
`InputFrame` / `MotionSnapshot` only, prefixed with a `u64` sequence, capped at
`min(1100, connection.max_datagram_size())`; a connection whose peer cannot
carry datagrams is rejected at setup.

**Liveness.** QUIC keep-alive plus an application `Heartbeat` on the control
stream every `heartbeat_interval`; a connection with no inbound control traffic
for `idle_timeout` is closed. Defaults: 500 ms / 10 s (2 s QUIC keep-alive).

**Deduplication.** Per-stream sequence gates (`spall_protocol::SequenceGate`):
the control stream is strict (any replay dropped); datagrams tolerate a bounded
reorder window so a late-but-new motion sample is still delivered while an exact
replay is dropped.

**Fault tooling.** Two independent mechanisms, both deterministic from a seed:
`FaultChannel` delays / drops / reorders *decoded application messages* on a
logical clock (for tests that must not depend on QUIC's own recovery); `UdpProxy`
drops / delays / reorders *opaque encrypted datagrams* between client and server
without parsing a QUIC header (for real retransmission / congestion behaviour).
`cargo xtask net-check` runs one server + N clients through per-client proxies
and writes `summary.json` / `net.jsonl` / `metrics.json`.
### T01 review clarifications

A `CellRun` addresses contiguous +X cells in the volume's cell coordinates with fixed Y/Z; `start.x + len - 1` must fit i64. Sphere and material-manifest deserialization uses the same validation as their checked constructors. Generic structural decoding does not know a world's material registry: world application must use `decode_topology(bytes, manifest)` or call `validate_against` before mutation. Baseline bulk payloads may use the full 1 MiB; `encode_bulk`/`decode_bulk` reserve additional bounded bytes for protocol and postcard metadata. Control records retain their independent 64 KiB cap. Negotiated limits must be nonzero and within protocol ceilings, and both simulation and snapshot rates must match.

### T09 review bounds and ownership

Authentication limits apply to QUIC establishment plus the application exchange. Callers may run concurrent `accept` operations, capped by `max_pending_authentications`; waiting for the first incoming connection is not an authentication timeout. Live server Connection objects hold bounded reusable slots (`max_connections`); dropping one releases its slot, and reuse increments its generation without wrapping. A client binds the unspecified address in the target's IP family for LAN reachability. The server echoes the accepted nonzero limits; both ends enforce them and the client validates the response session and compatibility.

The datagram cap includes the 8-byte transport sequence and the inner protocol frame. Bulk caps count payload bytes, with bounded metadata overhead added to framing. `collect_parts` handles one ordered transfer per stream, with indices starting at zero, one transfer ID, at most 4096 parts, and verified part hashes. Compression is not implemented at this stage; received payload bytes are never decompressed in T09.

Hosts must own and run one `Connection::run_liveness` future plus their receive pumps. It exits on connection close or stop-channel closure; stalled heartbeat writes close the connection. Canceling a partially completed stream read/write requires closing that connection, not retrying from a guessed frame boundary. The harness owns its child tasks and aborts them on early errors/timeouts. UDP proxy delays use a single owned queue (1024 packets and 2 MiB maximum); excess packets are dropped and queued sends cannot survive shutdown.
