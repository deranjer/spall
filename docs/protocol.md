# Multiplayer, persistence, and recovery contract

This is a proposed version-1 contract. T01 turns these records into tested DTOs before dependent tasks use them. User requirements: multiplayer and full-world destruction are essential.

## Authority and transport

The server owns world edits, support/fracture decisions, body identity, physics, and persistent state. Clients send input and action requests. Local play starts the same server with loopback transport; it does not bypass validation or transaction creation.

Use Quinn reliable streams and datagrams. One reliable ordered control/topology stream per client keeps initial event ordering simple. Separate bounded bulk streams carry baselines; bulk work must not starve control and motion. Limit concurrent bulk streams to four initially, with application priority and byte budgets. QUIC congestion control alone is not game traffic prioritization.

Datagrams carry player inputs and body/player motion. Each payload fits `min(1100, connection.max_datagram_size())` bytes after our envelope; recalculate when path limits change. Fail the connection setup clearly if required datagrams are unavailable. Larger records are split into independently useful snapshots, not IP-fragmented giant packets. Reliable application records use explicit length framing and checked decoding.

Handshake includes protocol version, exact content manifest hash, world ID, generator version, supported algorithms, server tick rate, cell-size codes, session identity, and negotiated limits. The sandbox combines its canonical material manifest hash with the optional versioned asset-manifest hash; clients serving custom content must provide the same asset manifest before connecting. Reject incompatibility before creating a player. For development use server certificate fingerprint pinning and a session join token; do not disable TLS verification globally. Dedicated Internet hosting is direct-address initially; QUIC does not provide NAT traversal, a relay, matchmaking, or account identity. Those need an explicit later platform choice.

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
| ProgressionRequest | nonzero request ID, recipe catalog version, expected inventory revision, and inspect/craft operation; authenticated control stream only |
| ProgressionResponse | request ID, current catalog/inventory revisions, outcome/rejection code, and complete sorted inventory snapshot |

Never serialize Rapier handles, ECS entities, pointers, platform-sized integers, or raw Rust structs. DTOs use explicit numeric sizes, tags, units, and bounds. Pre-release there are exactly two version numbers: `PROTOCOL_VERSION` (wire; every record header carries it as `WIRE_SCHEMA_VERSION`, which is the same value) and `STORE_SCHEMA_VERSION` (save). Client and server ship together, so a mismatch is a clear rejection, not speculative compatibility code. Per-record and per-family schema numbers return at the first public release, when real saves and third-party clients need protecting.

Progression requests are game-facing control records, not world edits. In
per-player credential mode, the server passes the authenticated `PlayerId` to
the game-owned catalog/inventory handler; legacy shared-token mode passes no
principal and remains suitable only for ephemeral per-slot progression. Crafting
uses optimistic catalog and inventory revisions; every reply contains the
authoritative full inventory for processed requests, including semantic
rejections, so clients can refresh stale state. A `RetryableCapacity`
admission reply has no authoritative inventory snapshot; clients retain their
current view and retry the identical request ID after backoff. The server
processes accepted requests on one bounded FIFO worker, preserving operation
order per player. Queue saturation never waits on the simulation tick. The
sandbox deduplicates by authenticated player ID when
available and falls back to the temporary connection slot only in legacy mode.
Durable inventory storage is a separate game-owned layer and remains open.

The sandbox per-player credential mode stores progression in its own
versioned SQLite database, separate from the authoritative world database.
Inventory revisions and item stacks use explicit stable fields. A craft
request, its complete response receipt, and the per-player request high-water
mark commit atomically in WAL mode with `synchronous=FULL`; recent receipts
are retained for replay and older unseen IDs are rejected. One bounded writer
thread owns this file. Craft requests are submitted through a bounded server
queue and their responses are sent only after the complete inventory and replay
receipt transaction commits. Harvest rewards in a saved world are first recorded in
the world's durable outbox in the same transaction as the world journal row.
The outbox delivery calls the progression writer, which deduplicates by
authenticated player ID and originating action request ID, then the world
writer acknowledges the event. Recovery replays unacknowledged events after a
crash. Live delivery runs on a separate bounded worker; the simulation thread
submits the world acknowledgement only after it receives the durable award
completion. Startup recovery and clean shutdown may wait for award completion.
Without world persistence, harvest remains ephemeral.

Limits start at 64 KiB per control record, 1 MiB per bulk part, 64 MiB per assembled transfer, and 256 KiB maximum decompressed data per material-only brick record (actual material payload is 64 KiB). Validate counts before allocation and decompress with output bounds. A multi-volume transaction can span staged bulk parts; its visible commit marker is small. Larger regions are split into multiple dependency-complete transfers. Each connection has bounded staging memory and a timeout.

## Water presentation and recovery (ENG-105)

Protocol version 4 uses ALPN `spall/4`. `WaterSnapshot` keeps record schema 3,
tag 15, on the ordered control stream: domain origin, fluid dimensions,
coarsening, server tick, sequence, chunk index/count, and zstd fractions in
X/Y/Z order (`255` full). Bounds are 4,194,304 cells, 96 chunks and 49,152
compressed bytes per chunk (4.5 MiB staged); nonfinal chunks must be full.
The assembler validates before staging and reads at most the declared cell
count plus one byte. Bad headers, corrupt streams and overlong output clear
staging without publishing a partial frame.

`WaterDelta` (tag 18) names the domain
origin, installed base sequence, new sequence/tick, ordinal/count, relative
fluid brick coordinate and zstd quantized fractions. Fluid bricks are 16 cells
per axis with edge clipping; at most 4096 changed bricks, 4160 compressed bytes
per brick, and bounded decoded output. Ordered unique coordinates, consistent
headers and the installed base are required. A complete candidate publishes
atomically. A missing base waits for the next full frame; malformed changes
leave the installed presentation unchanged. The sender chooses the smaller
valid full frame or delta group.

Each live session/domain gets a 128 KiB/s bucket with a 128 KiB initial burst,
at most 15 Hz updates and a full repair requested at least every 60 server ticks.
Large frames finish over multiple ticks before a successor is chosen; the byte
budget can delay repair completion. Up to eight domains give at most 1 MiB/s
and 1 MiB burst per client. Budget accounting includes encoded record bytes
plus 32 bytes per record; QUIC retransmissions and IP framing are outside this
application-byte measurement. Water uses bounded reliable FIFO backlog and
completes in order; overflow disconnects for re-baselining. Clients retain at
most eight domain assemblers and render received fractions only. There is no
water spatial interest filter in this bounded authored-domain integration.

World save schema 2 adds an explicit `checkpoint_water` row for every published
checkpoint, including empty water groups, with BLAKE3-16 integrity. Missing or
corrupt rows are recovery errors. Canonical `WaterState` schema 1 stores explicit
IEEE-754 bits for exact fractions, trapped cell volumes, configuration, fluid
time and cumulative source/drain/open-outflow volumes, plus source cells and
gate rate. ENG-126 adds canonical `WaterState` schema 2 with the same explicit
fields and a fine-domain ceiling of 67,108,864 voxel cells. Schema 1 retains
its 33,554,432-voxel ceiling and remains readable/writable for smaller domains;
older binaries reject schema 2 rather than reinterpret it. This is a canonical
save-record extension, not a change to the live presentation wire layout or
database tables. The group is limited to eight domains and 4,194,304 fluid cells;
each source list is bounded to 65,536 entries, with a 128 MiB checkpoint byte
ceiling. Terrain topology, poses and a canonical water group share the ordered
atomic journal batch. Recovery applies saved amounts to recovered committed
geometry, conserving displaced mass. Velocity, pressure, solver cache and sleep
reset to rest/awake; restarting is not a continuation of the pre-crash flow
trajectory. Presentation bytes never serve as canonical saves.

Old save formats are rejected. No save-copy migration command or compatibility decoder is retained.

## Column water presentation and recovery (ENG-167)

The stacked-column water (`docs/water-agent-handoff.md`) is the production
model. The dense MAC `WaterState` above is gone: `grid_mac` was removed in
ENG-169 and its wire and storage types with it (`WaterSnapshot` and `WaterDelta`
records, tags 15 and 18 now reserved; the checkpoint `water` row; the
`WaterState` journal payload; the schema-1 migration). Older saves are rejected. The column-water checkpoint row is always
written (`None` for a dry world), so a missing row is corruption, never a
silently dry restore. Each replicated span also carries the quantized
horizontal water velocity (`ColumnWaterUpdate`: `flow_x`, `flow_z` as
`i8` in 1/16 m/s) so clients predict swimming against the same current as the
server. It adds three contracts and no change to the save schema version.

- **Canonical state.** `ColumnWaterState` (record schema 3) stores only wet
  spans: per region `(rx, rz)`, each span as `(column 0..1023, floor y,
  volume)` with volume an unsigned fixed-point integer in units of 2^-32 cell^3
  (never float bytes), plus the fluid time, spring and drain totals, the
  unplaced-volume ledger, source and sink cells and the gated rate. Span floors
  and ceilings are not stored: they are derived from terrain, and restore
  refuses a saved span that has no matching terrain span. Recovery is at rest.
  The checkpoint carries it in an additive `checkpoint_column_water` row
  (created if absent, BLAKE3-16/CRC integrity like the other rows; a save with
  no column water has no row). The journal appends `ColumnWaterDelta` (a new
  last variant, so older rows keep their discriminants): changed regions
  replace the previous state wholesale and the scalars are absolute. A delta is
  journaled in the same batch as any terrain commit that touched water, and
  every 20 ticks while water moves; resting water journals nothing.
- **Replication.** `ColumnWaterUpdate` is wire tag 23, record schema 1, on the
  ordered water stream. A region's 1024 columns are split into eight windows of
  128; an update carries one window as a baseline (`base_seq` 0, replaces the
  window) or a delta (changed spans only, depth 0 meaning dry, valid on a
  window at exactly `base_seq`). Payloads are zstd of 8-byte spans and are
  bounded before decompression. Depth is quantized to 1/128 cell. A delta on
  the wrong base is refused, not guessed. The server never publishes a span
  that quantizes to depth 0. Each session is byte-budgeted like the other water
  traffic, a late joiner starts from baselines of every wet region, and clients
  never run the fluid. The wire schema number is unchanged: peers that predate
  tag 23 reject it only when a server actually uses column water.
- **Not yet done.** There is no spatial interest filter; every wet region goes
  to every session. The client still adapts regions to dense frames for the old
  water look until the span-surface renderer (W4).

## Exact geometry, approximate motion

Use a hybrid representation for topology changes:

- Replicate compact deterministic local integer brush operations when the client's source revision matches exactly.
- Encode explicit cell/material runs for irregular changes and repairs.
- The server chooses split membership, child IDs, ownership transfer, and resulting transforms. A split uses canonical source-cell ranges or a baseline blob, not a client rerun of floating-point physics or support heuristics.
- Use the smaller valid encoding, bounded by staging limits. Do not send full modified chunks every frame as the default.

T10 implements this for the all-resident G1 case: a committed `TopologyTransaction` carries `[IntegerBrush, (SplitOff, child-fill CellRuns)*, source-removal CellRuns]` — the brush op plus, for each detached component, a `SplitOff` marker and the canonical `+X` `CellRun`s that fill the new child volume, then the runs that clear those cells from the source. A replica applies the ops in order into a candidate, checks the `before` revisions and every `result_hash`, and publishes atomically; a `before` gap raises a rate-limited `RepairRequest` and the server answers a brick repair with `CellRun`s for that brick's current state.

T17 increment 1 (ENG-64) adds the **baseline-blob split**: when the inline `CellRun` list would overflow one reliable control record (`MAX_CONTROL_RECORD`, not just the `MAX_TRANSACTION_OPS` count — ~1300 fragmented runs already blow the 64 KiB frame), the commit re-encodes the ops as `[IntegerBrush, SplitOffBaseline { source, child, child_entity, blob }*, SourcePatchBaseline { source, blob }]`. Each `blob` is a zstd-compressed postcard `spall_protocol::baseline::BaselineVolume` (the same per-brick `{coord, revision, edited, cells}` payload late-join uses) — one carries the whole child volume, one the source's post-cut affected bricks — bounded by `MAX_SPLIT_BASELINE_BLOB` (28 KiB compressed) so the record still fits the wire. `before` / `after` / `result_hashes` are unchanged and remain the replica's acceptance check; the blob rides inside `tx.ops`, so the durable journal and exact-replay reconstruct the split with no schema change.

T17 increment 2 (ENG-64) handles the split whose compressed geometry is *still* over `MAX_SPLIT_BASELINE_BLOB` — the G1 64-brick collapse. The commit emits marker-only ops `[IntegerBrush, SplitOffBulkBaseline { source, child, child_entity, transfer_id }*, SourcePatchBulkBaseline { source, transfer_id }]` plus **one out-of-band `BaselineWorld`** (every child volume + the source's post-cut affected bricks). `transfer_id` is reserved from the common runtime transfer allocator before final transaction hashing/journaling, using the split namespace; it is preserved across wire and durable attachments. That world travels to replicas as a `BaselineTransfer` on a bulk stream (`BaselineBegin` control + `BaselinePart` bulk + `BaselineEnd` control, keyed by `transfer_id`), broadcast to every live client right after the marker transaction; a joining client gets both queued. The control stream is FIFO and single-reader, so a replica always processes the marker `TopologyTransaction` first, **holds** it (`ApplyOutcome::AwaitingBulkSplit`), then assembles the blob from the bulk stream and retries. To disk the pair uses current store5 `JournalPayload::TopologyAttachments`, with one Geometry entry in the existing bounded `TransactionAttachmentSetV1`. Exact replay and durable recovery validate the complete typed set against its parent before feeding the geometry into `replay_transaction`; older saves reject without migration. A split whose assembled `BaselineWorld` exceeds `MAX_ASSEMBLED_TRANSFER` (64 MiB) or `MAX_BASELINE_PARTS` is still refused (per-region splitting remains T18).

Physics is not lockstep. Do not promise identical trajectories from running the same inputs on every machine. Rapier's determinism guarantees have conditions, and whole-engine determinism requires more than selecting a physics feature. [Rapier determinism documentation](https://rapier.rs/docs/user_guides/rust/determinism/).

Canonical topology hashes include sorted volume IDs, cell-size codes, brick coordinates, authoritative layer bytes, ownership, and revisions. Exclude motion, render caches, and library allocation order. Specify little-endian encodings and stable sorting in T01; BLAKE3 over the canonical bytes defines the result. Hashes detect divergence; baselines repair it.

## Transaction application

1. Server constructs a complete transaction against specific input revisions. All source changes, children, mass/pose metadata, and required server collision updates are ready before commit.
2. Server commits once at a tick boundary and journals the complete authoritative result. Once an action has passed authoritative validation and entered admission, repeated ActionRequest IDs return its stored `Queued`, `Committed`, or deterministic staging/commit `Rejected` status and cannot perform another cut. The replay lookup occurs before re-validating the wire claim, so a reconnect can recover a committed result even after that edit changed the ray target. Pre-admission validation and rate-limit refusals are not admitted actions: they do not reserve a request ID, and a corrected retry remains a new request for admission.
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

T18 makes interest eviction revision-safe. The shared cache treats server and
client brick count/dense bytes as hard ceilings and retains an outer hysteresis
band after a brick leaves the enter radius. Server eviction requires an exact
durable revision acknowledgement first; a dirty brick is never removed merely
because a write was queued. Modified-air is carried by the existing
`StoredBrick.edited` field and restored before generation can run. Structural
queries return pending while any streamed dependency is unavailable. Collision
entry likewise returns a bounded missing-brick set and defers movement instead
of sampling unknown space as air. A dynamic body's intersected partitions are
references to one stable entity/volume pair; dependency baselines include the
complete body, never a partition-owned fragment.

## Persistence

Use SQLite transactions through one I/O writer. Store metadata, versioned compressed brick payloads, volume/body records, spatial references, checkpoints, and an ordered authoritative journal. Configure and verify `journal_mode=WAL` and `synchronous=FULL` on the writer in T16. Group pending journal records into a database transaction, and acknowledge durability only after its successful commit. Keep the database on local storage. SQLite permits one WAL writer at a time and distinguishes commit durability from checkpointing. [SQLite WAL documentation](https://sqlite.org/wal.html).

An engine checkpoint is a coherent saved simulation snapshot. A SQLite WAL checkpoint transfers database WAL pages; these are different operations. Bound reader transaction lifetimes and schedule database checkpoint work off the simulation thread. Include WAL size and flush/checkpoint latency in persistence metrics. T00 must select a released SQLite build with applicable upstream WAL fixes, including when using rusqlite's bundled library.

Required world metadata: schema version, WorldId, seed, generator version, material manifest/hash, cell-size codes, next-ID counters, checkpoint tick/cursor, and structural algorithm versions. Body records store stable IDs, voxel geometry references, pose/velocity, sleep state, damage/bond state, and mass inputs. No physics library internals are the primary save format.

Initially checkpoint the bounded scene from one immutable tick snapshot every 30 seconds and on clean shutdown; journal topology transactions between checkpoints. Journal periodic body pose batches at 20 Hz. Each topology journal transaction also contains the participant body states required at that transaction, so geometry edits to moving bodies recover with the correct ownership/frame even if earlier motion batches are missing.

Group disk flushes with a target interval <=100 ms, then emit DurableThrough. A simulation commit may precede durability. A crash can lose the unflushed suffix and rewind motion to the latest durable pose batch; this is the explicit initial durability model. Clean shutdown waits for a final checkpoint/flush. Never tell an automation that a save succeeded before durable acknowledgement. If the storage queue exceeds its limit or disk writes fail, stop accepting persistent edits and return an error; do not silently continue an unsavable world.

Checkpoint publication records all brick/body data and its journal cursor in one DB transaction. Recovery loads the latest complete checkpoint and replays the durable ordered journal suffix. Ignore no interior corrupt record: report corruption and offer the previous valid checkpoint as an explicit recovery choice. A crash must not leave the terrain removed without the corresponding child body.

Game progression that grants rewards for a committed world edit uses the
world database's durable outbox. The world journal row and opaque, explicitly
versioned game event are inserted in one SQLite transaction. The game applies
the event idempotently in its own database using a stable event/request ID, then
the world writer acknowledges (deletes) the outbox row. A crash before the
world transaction leaves neither; a crash after it leaves a replayable event;
a crash after the game transaction but before acknowledgement safely replays
the same ID. Events remain pending until the game database reports success.
This guarantee applies when world persistence is enabled; an ephemeral run has
no crash-recovery promise.

Retire journal records only after a newer durable checkpoint covers them and no snapshot transfer needs them. Cap retention; lagging joins get a fresh baseline. Persist modified-air bricks explicitly so procedural regeneration cannot restore mined blocks. Initial baselines transfer authoritative terrain rather than trusting client-side generation to be bit-identical.

Use explicit versioned DTOs and fixture worlds when changing save formats. Unsupported schemas are rejected without modifying the database; no save-format migration paths are retained. An older save fails to open with an "incompatible save, created by an older pre-release build" error (`StoreError::SchemaTooOld`, `PersistError::SchemaMismatch`); there is no migration before the first public release.

## Network budget and overload behavior

Provisional per-client steady-state egress target: <=256 KiB/s, measured including topology and motion; baseline transfer has a separate capped 1 MiB/s budget. At eight clients this is already about 16.8 Mbit/s server steady egress before overhead. Track both directions and actual transport bytes.

Budget example: 200 relevant bodies x 48 encoded bytes x 20 Hz = 192,000 bytes/s before framing, topology, player records, and retransmission. Therefore not all bodies can receive 20 Hz updates continuously. Prioritize players, imminent contacts, nearby moving bodies, and recently changed structures; send distant/sleeping bodies less frequently with periodic keyframes.

Never discard committed topology to meet the motion budget. Drop superseded unsent motion snapshots; cap cosmetic events; throttle expensive actions; repair or disconnect a client whose reliable backlog exceeds the bounded window. Record backlog bytes and age. The stress gate must show that the queue drains after a blast and a new player can join while the server continues running.

## T20 interest and bandwidth scheduling (partial — increment 1)

`spall_server::serve` gains an opt-in `ServeConfig::motion_interest`
(`MotionInterest { near_radius_m, far_radius_m, far_interval,
per_client_budget_bytes, static_anchor_m }`, CLI `--motion-interest` plus
`--motion-near-m` / `--motion-far-m` / `--motion-far-interval` /
`--motion-client-budget-bytes` / `--motion-static-anchor`). Unset, the host
behaves exactly as before: one 20 Hz `MotionSnapshot` batch broadcast
unfiltered to every client.

Set, each **live** client (a client still pulling a late-join baseline keeps
getting its catch-up keyframe, not the live feed) gets its own filtered batch:

- **Relevance by bounds.** `crates/spall_server/src/replication.rs` classifies
  every body/player against the client's `InterestSet` — anchored at the
  client's player capsule on a player scene, else at `static_anchor_m`, else
  `Global`. A body is `Near` (every batch) within `near_radius_m` of its
  bounding-sphere *near face* (so a large body is relevant when its bounds
  reach in, per "Large bodies are relevant if their bounds intersect
  interest"), `Far` (reduced cadence) out to `far_radius_m`, and `Excluded`
  beyond that plus a 1.25x outward hysteresis band. A player is never
  `Excluded` — only slowed to the `Far` cadence.
- **Motion-frequency tiers.** `Far`-tier snapshots are sent only every
  `far_interval`-th batch; the batches between are deferred (superseded, so no
  geometry is lost).
- **Per-client byte budget.** `per_client_budget_bytes` caps one client's
  motion per batch, accounted at a fixed 48 B/snapshot. Over budget, the
  lowest-priority motion is deferred first: far, then near-sleeping, then
  near-awake, then other players, then the client's own player.
- **Egress accounting.** `ServeSummary` (now `version: 2`) reports
  `motion_snapshots_sent` / `_interest_culled` / `_budget_deferred`,
  `max_client_motion_batch_bytes`, and both `app_egress_bytes` (the
  `spall_net` application-byte counter) and `transport_egress_bytes` (Quinn's
  `udp_tx.bytes` — actual UDP payload including QUIC framing and
  retransmission), summed over every client connection.

Committed `TopologyTransaction`s are never interest-filtered or dropped here —
the ENG-48 reliable-backlog bound remains the only thing that sheds them, and
only by disconnecting the client so it re-baselines. Not yet in this
increment: the eight-client separated-interest bandwidth scenario and its
xtask acceptance assertions (needs a world larger than the current fixtures),
per-body motion-frequency priority by velocity, and cross-region dependency
baselines for an interest-crossing split.

## T01 wire encoding decisions (implemented)

`spall_core` and `spall_protocol` implement the record families above. The
following are now fixed and must be reused, not re-derived, by dependent tasks.
`spall_protocol` record headers carry `WIRE_SCHEMA_VERSION`, which is defined as
`PROTOCOL_VERSION` (currently 15): one wire version for the handshake and every record, bumped on
any layout change. The save has the single `STORE_SCHEMA_VERSION` (currently 7, defined in
`spall_core` and re-exported by `spall_store`). The per-record schema constants that used to
exist (creature owner/change/snapshot/sync/motion/state, column water, weather, snow,
transaction attachments, lifecycle bulk, `BASELINE_WORLD_SCHEMA`) are now that same value.

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

**The canonical topology hash (`spall.topology.v2`).** Sort orders: volumes ascending by
`VolumeId`; bricks ascending by `(z, y, x)`; authoritative layers ascending by numeric layer
`kind`. Owner is encoded as `0,u64=0` for terrain or `1,u64=EntityId` for a body. Reordering the
inputs does not change the digest. All integers are little-endian; a "blob" is a `u32` length
followed by the bytes, and a "seq" is a `u32` count; BLAKE3 is the hash, and every hash starts
with its domain tag as a blob.

*Chunks.* Bricks are grouped into chunks of 8 x 8 x 8 (`HASH_CHUNK_SHIFT = 3`): the chunk of a
brick is its `(x, y, z)` shifted right by 3 on each axis, so negative coordinates floor. Chunks
are ordered by `(z, y, x)` of the chunk key, like bricks.

*Chunk digest* (domain `spall.topology.chunk.v2`): the chunk key as three `i64`; `seq` of the
chunk's bricks; then each brick in `(z, y, x)` order as its `x, y, z` (`i64`), revision (`u64`),
`seq` of layers and, per layer in `kind` order, `kind` (`u16`) and the layer bytes as a blob. A
chunk with no brick has no digest and is omitted. For the material layer the layer bytes are the
brick's 32-byte representation-independent content hash, and a volume's bricks are its **logical**
set: resident bricks plus the digests of evicted ones, each coordinate once.

*Topology hash* (domain `spall.topology.v2`): `seq` of volumes; per volume in id order, its id
(`u64`), cell-size code (`u8`), owner, `seq` of its non-empty chunks, and per chunk in order the
chunk key as three `i64` and the chunk digest as a 32-byte blob. BLAKE3 over those bytes is the
result.

*Why chunks.* A change to one brick alters only its chunk's digest, so an implementation can
cache chunk digests and recompute only the chunks whose contents changed (a commit then hashes a
few chunks rather than every brick of the world). The reference implementation validates a cached
digest by the stamps of the storage chunks it was computed from (`spall_voxel`'s
`Volume::chunk_stamps` and `EvictedBricks::chunk_stamps`), which are replaced by every mutation
and shared by clones; a different cache is equally conformant as long as the digests are equal.

*Versioning.* v1 hashed every brick in one stream under the tag `spall.topology.v1`. v2 changes
every hash value, so it is not interchangeable: the wire version was bumped
(`WIRE_SCHEMA_VERSION`, stamped in every record header, so a peer on the old version cannot decode a single
record) and saved worlds record `topology_hash_version = 2`. **A save written under v1 is
refused on recovery** with an algorithm-version mismatch; its recorded checkpoint and journal
hashes cannot be verified under v2 and the engine does not carry the v1 algorithm to migrate
them. The content manifest hash is unchanged (`spall.manifest.v1`).

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
no simulation, storage, or rendering. ALPN is `spall/4`; TLS is 1.3-only.

**Sessions.** Clients pin the server certificate fingerprint (BLAKE3 of the
certificate DER, carried out of band). Legacy development mode accepts one
per-run 32-byte shared join token and assigns no stable player principal.
Player-owned state uses per-player credential mode: the server provisions
unique 32-byte bearer tokens bound to stable 128-bit `PlayerId`s. `ClientHello`
contains only the token and compatibility handshake; the server checks the
token and returns the assigned optional `PlayerId` in `ServerAccept`. The ID is
never client-asserted. Credential files must be access-controlled and tokens
provisioned to the corresponding client through a protected channel. A wrong
certificate fails the QUIC/TLS handshake; an unknown token or incompatible
`Handshake` gets a distinct `AuthReject` before teardown. This local
`ClientHello` / `ServerAuthReply` exchange precedes the normal control stream.
When `--player-credentials-file` is set, the server checks that file every
500 ms. A valid atomic replacement rotates or revokes tokens while preserving
the stable PlayerId. Registry changes close active sessions, so reconnects
must use a currently registered token; malformed or unreadable updates revoke
all credentials until a valid file is installed. Follow
[`credential-operations.md`](credential-operations.md) for protected
provisioning, rotation, and revocation steps.

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
for `idle_timeout` is closed. Defaults: 500 ms / 30 s (2 s QUIC keep-alive). The
30 s bound gives a dependency-complete baseline capture (a bounded, CPU-heavy
snapshot before its first bulk byte) headroom to finish under legitimate
server-side load while still bounding a genuinely stalled peer.

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

## T17 late join, catch-up, and reconnect (implemented)

ENG-126 runtime extension: a client advertises
`segment::baseline_cap_streamed()` in its sentinel ack hash. The server may
then send `BaselineBegin.world_version = 3`, reusing segment schema 2: a
manifest followed by individually hashed/compressed frames. The cumulative
compressed ceiling is 256 MiB; each bulk part remains <=1 MiB, each decoded
segment <=4 MiB by default, and the manifest's decoded total <=16 GiB.
Emitted world versions today: 1 (single blob: repairs and split blobs), 3 (streamed), 4 (regional) and 5
(deferred regional). Version 2 (non-streamed segmented) is no longer produced by the server
(its constant is still referenced by `serve.rs` and the client accepts it); the former
version-9 creature-lifecycle constant was unused and has been deleted.
Version 1 and version 2 retain their 64 MiB compressed ceilings; the
single-blob decompression bound remains 256 MiB. Unsupported clients receive
the single-blob format, never an unnegotiated larger transfer. Repairs and bulk
split blobs keep the existing format.

The server expands immutable brick snapshots one segment at a time. The client
feeds `BulkRecv::next_part` into `SegmentedReceiver`, checks its explicit
staging budget before decoding segments, verifies part counts/bytes, transfer
id, chained hash and cursor, then installs the staged world atomically.
Default admission is 4 GiB; the portable 1024 m launcher explicitly selects
8 GiB. This is a staging admission allowance, not a raised resident-memory target.
Initial joins have a whole-session deadline. Existing replica bytes
are included when admitting a reset replacement. One queued version-3 baseline
has a separate <=256 MiB slot in the outbound FIFO; the writer may own one
while one waits. Ordinary reliable backlog still has its 8 MiB/2048-record
bounds. Oversized captures and client budget failures disconnect explicitly.

During replacement, the client retains an immutable COW snapshot of its old
volumes. A received dense brick may share that payload only after every
material has been compared exactly. Its incoming revision and edited flag are
restored independently; a changed edited flag invalidates the content-hash
cache. Different cells allocate a new payload. All wire, sequence, hash-chain,
cursor and admission checks still run; conservative admission still charges
both complete worlds even when payloads can be shared.

Material brick storage may internally use an exact u8 index into at most 256
full-width material IDs. Higher-diversity bricks retain u16 cells, and edits
expand before mutation and repack on collapse. Canonical material bytes, brick
hashes, wire `Dense(u16)` and durable encoding are unchanged. Replacement
admission counts actual old payload bytes plus metadata; incoming manifest
costs retain their full-width worst case. Unknown/air, revisions, ownership
and edited-air tombstones are unchanged.

At server shutdown, stop new connection admission after durable simulation
completion, enqueue shutdown after accepted reliable records, and keep control
readers and heartbeats alive while writers drain. A segmented baseline waits
for acknowledgement of its bulk bytes and FIN before sending `BaselineEnd`.
The existing transport idle timeout bounds shutdown drain; exceeding it fails
the server summary explicitly. Shutdown adds no simulation ticks.

ENG-114 transfer profiling retains CUBIC as the ordinary QUIC controller.
`TransportConfig::congestion = Bbr` and sandbox `--experimental-bbr` explicitly
select Quinn's experimental BBR implementation for diagnostics. This changes
the local congestion policy only: authentication, wire negotiation, stream
counts, allocation/admission limits, control liveness, deadlines and exact
baseline validation are unchanged. The distinct BBR impaired fixture must not
be reported as proof that the default CUBIC impaired workload passes, or as a
G3/G4 fairness/congestion acceptance result.

The single-blob baseline transfer body (world version 1: repairs and split blobs) is
`spall_protocol::baseline::BaselineWorld` (its `schema` is `WIRE_SCHEMA_VERSION`): per volume the `VolumeId`, cell-size code, owner (terrain / body
entity), optional brick bounds, and every resident brick as
`{coord, revision, edited, cells}` where `cells` is `Uniform(u16)` or a
`Dense` vector of exactly `CELLS_PER_BRICK` raw material ids. It carries
**authoritative geometry only** — the replica reaches the exact canonical
topology hash from it, so a late join needs no edit replay from world
creation; current body pose/velocity/sleep arrive as a `MotionSnapshot`
keyframe once the catch-up barrier is reached. The postcard bytes are split
into `BaselinePart`s (`part_index` ascending, `part_hash` = BLAKE3 of the
chunk) under the existing 1 MiB bulk-part / 64 MiB assembled-transfer
ceilings; reassembly is concatenation in index order and `BaselineEnd`
carries the BLAKE3 of the whole payload.

A late-join replica signals intent by sending a `BaselineAck` whose
`transfer_id` is the reserved sentinel `0` (a real transfer id is always
`>= 1`); no frozen wire record was added. The server then, for that client
only: captures a `BaselineWorld` at the current tick and journal cursor `J`,
sends `BaselineBegin` (control) + parts (one bulk stream) + `BaselineEnd`
(control), and buffers every subsequently committed `TopologyTransaction` in a
bounded per-client catch-up queue rather than broadcasting it. On the client's
real `BaselineAck { transfer_id = the assigned id }` the queue is flushed in
commit order, a motion keyframe follows, and the client rejoins the normal
broadcast. Other clients are never paused. A catch-up queue past its cap
cancels the transfer and re-captures a fresher baseline; a bounded number of
restarts then an explicit disconnect for that client only. A brick
`RepairRequest` is answered with a one-brick `BaselineWorld` patch delivered
over the same transfer mechanism — it restores the brick's exact revision as
well as its cells (the T10 `CellRun` reply could not), and the client merges
it into the live replica instead of replacing it. Reconnect reuses the
`spall_net` session slot with an incremented generation; the server tracks the
highest generation seen per slot and rejects any `ActionRequest` /
`RepairRequest` stamped with a lower one.


### ENG-118 vegetation records

Wire tag 19 is version-1 VegetationSnapshot under envelope schema 3: tick, ordered chunk index/count, BLAKE3 digest, up to 48 KiB payload, 4 MiB complete assembly bound. One-second self-contained presentation keyframes also repair late joiners. Replacing baselines clear plants and discard older frame ticks. Matched new client/server builds are required to display tag 19.

Store schema 2 gains a checksummed checkpoint_vegetation auxiliary row in the same publication transaction. First-time table creation backfills old checkpoints with empty vegetation; recovery never reseeds an existing save. JournalPayload appends VegetationState and VegetationClock without changing previous discriminants. Full state follows committed topology; small time_ms/credit_ms records preserve the exact interval clock without writing a forest at 60 Hz. Pending uncommitted growth requests regenerate from acknowledged progress after recovery. DTO fields and limits: docs/reports/ENG-118-vegetation.md.

### ENG-189 vegetation v2 and ENG-188 water (worldgen v4)

`VegetationState` / `VisualFrame` version 2 added `cover` (stateless cover species), `field` (the run-length-coded 4 m climate raster: biome, moisture, temperature, water level), `cleared` (destroyed-cover records, sorted `(x, z, at_s)`, at most 65,536) and `preset`. Version-1 state still decodes: carpet species become cover species, class masks widen to biome masks, and only the dropped carpet organisms are lost. Server and client must both be this build to show tag 19 (unchanged tag, schema bump). `ColumnWaterState` version 4 stores per-spring discharges (fixed point, 2^-16 cells^3/s); older versions decode with none. `AdminCommand::TeleportSelf { feet_mm }` moves the issuing player's body to the given feet position (admin commands must be enabled; non-finite positions are rejected; velocity is cleared and the movement epoch bumped so prediction resyncs; the client uses it when leaving fly mode so the body appears where the camera was). No new record tags. Field layout and limits: `docs/worldgen.md`.

Version 3 (ENG-190): the saved `LivingState` stores each organism without its skeleton (`wood`, `parents`), only the removed-cell indices, and regrows trees on load; versions 1 and 2 decode and upgrade. `VisualFrame` carries the same version number. A keyframe to a regional client holds only the plants and destroyed-cover records inside its window; the `field` raster is empty except on a session's first keyframe and every tenth, and a client reuses the last non-empty one (it clears it on a world reset). Whole-world clients receive every plant as before. The frame stays one `VegetationSnapshot` stream per client (chunk digest, 4 MiB assembly bound unchanged).

### ENG-114 transport diagnostics (2026-10-06)

TransportConfig optionally requests a kernel UDP receive buffer in 64 KiB..=4 MiB,
validated before binding. None retains Quinn/platform defaults; production remains
CUBIC. Actual granted capacity is debug-logged and may differ by OS. This does
not change any frame, streamed-baseline, staging or timeout admission limit.
The encrypted UDP test relay drains at most 64 already-due packets per wake,
in deadline/insertion order, without forwarding future packets early. Existing
rate scheduling, fault probabilities and bounded queue admission remain intact.

### ENG-130 regional startup baseline

Opt-in regional clients advertise a radius-specific capability hash in the existing baseline request
sentinel: `spall.baseline.capability.regional.v4.radius.{radius}`, radius 1..=16 bricks. Generated-world
play requests radius 10. Servers that do not understand the capability retain their full-baseline
fallback. World version 4 uses segment schema 3 and the version-3 streamed transport limits.
Versions 1..3 and segment schema 2 retain complete geometry. Durable world/split formats do not change.

Every logical terrain brick appears exactly once: real geometry in the spawn-centered Chebyshev box,
otherwise `BaselineCells::Digest` with canonical content hash, solid-cell count, revision, and edited
flag. This is a catalogue of the whole logical world, not a claim of resident distant geometry.
Detached bodies always contain complete geometry and stable ownership. Digests are forbidden in
legacy baseline blobs, repair geometry, authoritative replay, and durable split blobs. Receiver
validation rejects unknown schema/version combinations, body digests, out-of-bounds coordinates,
duplicates, geometry/digest overlaps, invalid counts, and incomplete manifests before atomic install.

Movement uses existing bounded, revision-validated BrickBaselineRequest repair traffic to fetch
nearby evicted bricks. Distant transactions retain existing gap/repair rules and exact logical hash
convergence. A reset replaces the geometry and digest namespaces together and selects the same
negotiated format independently for each client. A regional capture is never reused across players.

### ENG-114 deferred terrain catalogue and control-stream flow control (2026-10-07)

Additive wire records under envelope schema 3; matched client and server builds are required.

* **Deferred regional baseline.** A client that advertises
  `baseline_cap_regional_deferred(radius)` (instead of `baseline_cap_regional`) receives
  `BaselineBegin.world_version == 5` (`BASELINE_DEFERRED_REGIONAL_WORLD_VERSION`). The baseline
  carries every body and only the terrain within `DEFERRED_CORE_RADIUS_BRICKS` (2) of the player,
  with no digests, so the client is ready as soon as it arrives. A world with nothing to defer is
  sent as an ordinary complete baseline (world version 3), never as version 5.
* **`CatalogueChunk` (tag 20).** The distant terrain digests follow as an ordinary segmented
  payload (manifest, then segments, each hashed and bounded exactly like a baseline) split into
  in-order chunks of at most 32 KiB on the reliable control stream. Chunks carry the baseline's
  `transfer_id`, `index`/`count`, the payload's chained hash, `delta` and `expected_world_hash`.
  The server writes a chunk only when nothing else is queued, so no record waits behind the whole
  catalogue. The client feeds chunks to the same bounded `SegmentedReceiver`, verifies the chained
  hash, and merges digests for bricks it does not already hold (a resident brick wins).
* **Replica rules while a catalogue is pending.** Transactions are held in arrival order (bounded
  by `max_catalogue_deferred_txns`; past it the replica rejects loudly rather than dropping) and
  replay when the catalogue completes. `world_hash` is not comparable with the server's until then.
  A chunk for a replaced baseline is ignored; a refused or unmergeable catalogue ends the client
  with `baseline_transfer_failures`.
* **`CatalogueAck` (tag 21).** The client acknowledges each chunk it has processed. The server
  keeps at most `CATALOGUE_WINDOW_CHUNKS` (4) unacknowledged, so the transport's send buffer cannot
  hide minutes of catalogue ahead of the next baseline.
* **Delta catalogue for world resets.** When a client holds the complete catalogue of the world
  being replaced (the server tracks delivery per connection), the reset's catalogue lists only the
  bricks whose digest differs from the old world's and sets `delta`. The client merges them onto the
  catalogue it held and must reproduce `expected_world_hash` (the new world's canonical hash at the
  baseline tick) before any held transaction replays; otherwise the catalogue is refused. A reset
  to a world lacking a brick the client holds uses the full catalogue. Held-for-repair
  transactions do not block this: the bricks they would change already differ from the new world
  and are overwritten by the delta.
* **Resuming an interrupted catalogue.** A reset that arrives while the client is still receiving
  its catalogue does not discard what it has verified. A catalogue is a segmented payload and a
  segment is checked against the manifest and merged as soon as its last byte arrives, so the
  client holds exactly the segments that end within the chunks it has processed. Catalogues use
  small segments (`CATALOGUE_SEGMENT_DECODED_BYTES`, 128 KiB decoded, about 1,000 digests, one or
  two chunks) so the unit of progress is small; the baseline's 4 MiB segments would make a large
  world's catalogue two segments. The server records each catalogue's layout (the digests it
  carries, where each segment ends, and the basis it was itself built on) and counts the chunks
  its writer has written. Those are certain to have been processed by the client before the
  reset's baseline, because the control stream is ordered, so the count is a lower bound that can
  never overstate what the client holds. The reset's catalogue is a delta against the basis
  *plus every segment that ends within those chunks*, and declares the count in
  `CatalogueChunk.basis_chunks`. The client keeps the interrupted receiver's per-segment digests
  and selects the same prefix (ignoring anything it received beyond the count), merges the delta
  onto it, and must still reproduce `expected_world_hash`. A client that processed fewer chunks
  than `basis_chunks`, or has no interrupted catalogue to resume, refuses the catalogue (a loud
  failure, never a guess). An interrupted *delta* resumes from its own basis plus its completed
  segments, so resets can chain. A client that has processed no chunk, or a world that lacks a
  brick the client holds, gets a full catalogue (`delta = false`, `basis_chunks = None`).
  `basis_chunks` may be set only on a delta.
* **`ControlProbe` (tag 22).** The writer sends a probe whenever it wakes (queue activity, an echo,
  or its 250 ms idle timer), but never two within 25 ms; the client echoes it in
  order. Because the control stream is ordered, an echo confirms every byte written before the
  probe. Droppable traffic (water, vegetation, catalogue chunks) is held back while the oldest
  unanswered probe is older than 1.5 s or the bytes written past the last confirmed byte exceed
  1.5 s of the measured delivery rate (clamped to 48 KiB..1 MiB). Only the newest 16 probes are
  tracked, so a client that cannot echo yet (installing a baseline) never leaves the stream
  congested permanently.
* **Ordering and backpressure.** A batch's motion datagrams are sent first and again after every
  reliable message, so a run of bulk repairs cannot delay them. A client with unsent water
  outstanding (`MAX_UNSENT_WATER_MESSAGES`, 4), a streaming catalogue, or a congested control
  stream is not given further water or vegetation frames; the publisher's state is untouched, so the
  newest frame goes out later.

Single-brick repair requests no longer fill `current_hash`/`expected_hash` (the server never read
them), and a transaction whose result hash covers a volume with an earlier transaction still held
for a repair or bulk blob waits behind it.

### ENG-196/199 - Weather state and authorized overrides

Wire schema/protocol 6 adds WeatherSnapshot tag 24 and appended
AdminCommand::SetWeather { kind: Option<WeatherKind> }; None restores automatic.
WeatherState schema 2 carries the canonical override along with explicit clock,
epoch/revision, quantized precipitation/wind and stable lightning event fields.
Matching client/server versions are required. Old weather checkpoint/journal
layouts are rejected; no runtime enum bytes are persisted.
Existing ecology chronological clock and VegetationClock journal semantics stay
unchanged. A persisted fallback tick clock serves worlds without vegetation.
Weather overrides require the existing admin authorization and persist through
checkpoint/restart; world reset clears them. Per-weather stream fences allow
weather older than motion while refusing stale weather/reset records. See
reports/ENG-196-weather-foundation.md and reports/ENG-199-weather-controls.md.

### ENG-206/208 - Ground snow and footprints

Wire schema/protocol 7 originally added `SnowSurfaceFrame` (tag 25), versioned
separately as snow schema 1. Frames replace one 4 m tile/Y layer, carrying
disjoint exposed-face cohorts, terrain revisions, quantized depth and bounded
transient footprints. Empty frames remove old layers. Epoch, sequence and
effective-tick fences prevent stale snow after reset; client cache residency is
bounded. Server-authoritative grounded travel creates tracks; clients never
submit imprint positions. See weather.md for rates, caps and exposure rules.

Canonical checkpoints retain exact fractional snow depth and climate cursors;
geometry journal entries preserve exact cohort changes. Footprints clear on
restart. Live and saved topology/motion records now require protocol/wire 12. Saved
wire headers 5, 6, 7, 8 and 9 are rejected without migration or header rewriting.

### ENG-218 - Ordered creature lifecycle boundary

Protocol/wire 12 carries a typed inline `TopologyOp::CreatureLifecycle`
with the current `CreatureLifecycleCommitV4` and ownership/change schemas 4. The
operation must use its enclosing transaction ID and exact operation ordinal.
Multiple operations use the existing transaction count limit. The codec checks
the 64 KiB control limit before allocating an encoded frame. Catalog, source and
transition validation still belong to the owning candidate consumer.

Current protocol/wire 12 includes `TopologyOp::CreatureLifecycleBulk`, binding its
exact operation ordinal, payload schema, outer commit hash and shared transfer ID.
`CreatureLifecycleBulkPayloadV4` uses payload schema 4 and world-version tag 9 and bounded postcard/zstd
framing. The decoder checks the marker correlation prefix before nested owner
decoding and hashing. Raw commit, cumulative source/ownership work and compressed
assembly limits are enforced. Geometry split children and their source patch may
share a geometry transfer ID; lifecycle IDs may not reuse each other or geometry
IDs. The codec and marker are implemented; network dispatch for tag 7 remains
closed until atomic typed attachment consumers and the common allocator are ready.

The current geometry-only replica and SimWorld replay paths reject actor-bearing
transactions before geometry, clocks, sequence gates or replay receipts change.
This boundary does not enable spawning or claim complete baseline or durable
animal journal recovery. Baseline schema 1 remains guarded; current store schema
4 adds the complete checkpoint section described below. Unsupported old formats
are rejected; legacy save upgrades and missing-table backfills have been removed
under the user's explicit compatibility decision. Crash recovery from a valid
complete checkpoint remains a separate requirement.

The standalone schema-1 `TransactionAttachmentSetV1` groups typed geometry and
lifecycle payloads for one transaction. Its decoder checks exact transfer-ID
coverage and families, geometry child identities, and lifecycle marker correlation.
A private validated set also binds the existing parent replay identity, so a
different transaction body cannot reuse it merely by keeping its transaction ID.
Source-patch owner authenticity and material/catalog/world validation remain the
owning consumer's responsibility.

The set retains the 64 MiB cumulative compressed limit, a conservative 256 MiB
geometry decode/storage budget and 64 MiB lifecycle envelope budget. Geometry
postcard lengths are scanned before nested vector allocation. Lifecycle work is
also cumulative across the set, including the existing 1,024-owner, 16 Mi dense-cell
and 4,000,000-solid-cell limits; compact Uniform encodings cannot bypass them.
General geometry retains its own bounds rather than inheriting animal-only caps.
The consuming world must still budget geometry expansion and physics work.

The server allocator uses one checked 61-bit low sequence for baseline, retry,
repair, reset and geometry split reservations, with namespace bits preserved
for reset/split and reserved for lifecycle. Dropped reservations burn their range.
Reset reserves the connected-session group before replacing the world. Geometry
reserves a bounded owned pool before the existing tick: the bound includes
carried inflight work and newly admitted edits across all rebase rounds. Idle
ticks reserve nothing. Bulk markers, streamed payloads and durable geometry
records retain the same reserved ID; transaction IDs no longer generate it.
Startup validates IDs from every topology record in the durable recovery suffix
before restoring or publishing another checkpoint, then seeds the live allocator
above them. The fresh in-memory journal is not the recovery source. Lifecycle
reservations and typed runtime consumers remain closed pending complete actor
checkpoint/baseline restore and ordered attachment transport/replay.

The catalog-free checkpoint capture refuses any initialized creature host,
including an empty host whose allocator high-water must survive restart. It cannot emit a
geometry-only checkpoint that silently omits that state. Complete live-animal
recovery also requires independent applied part-body twists: root motion and
frozen part poses alone cannot reproduce an animated limb's immediate checked
death transfer. The physics reconstruction prerequisite carries those measured
values explicitly, and the current owner/motion records now retain them. Complete
Simulation checkpoint and baseline integration remains required before enabling
animal checkpointing.

Whole-owner corpse physics capture now returns the existing runtime
`DynamicCorpseRestore` DTO. The owning thread supplies canonical original source
bindings and frozen hierarchy-local transforms; physics validates their complete,
disjoint cell coverage against every live component and captures real component
poses/twists. Its expected owner/source/topology/portable-pose/serial fence must
match, and pending frames refuse. The primary physical component remains element
zero, followed by stable component ordering. A caller-supplied key for an intact
checked-death owner is checked without binding or mutating the owner index.
This runtime DTO is not a complete serialized ownership snapshot: original sparse
volumes/material catalogs, authored transforms and allocator state still require
Simulation capture and explicitly versioned complete checkpoint/baseline schemas.
Ownership-set decoding reuses the existing seeded record reader with per-record
limits and shared population limits; a fresh per-record Deserialize cannot reset
aggregate admission. It reads the current V4 ownership layout. The safe
`CreatureOwnershipSetV4::decode(bytes, catalog)` entry point caps the input frame,
rejects trailing bytes, and performs bounded parsing followed by catalog/semantic
validation. Generic Deserialize admits resources only. Unknown ownership map fields
(including nested fixed metadata) reject before their values; missing optional
None metadata retains its existing map behavior. State-name byte limits apply
before creating owned postcard strings. The population reader does not install a checkpoint or introduce a production
consumer; its resource budgets also apply inside the snapshot envelope below.

Current animated ownership uses V4 owner/set/change/commit and bulk payload
types, retaining unchanged motion overlay V2 and part velocity V2 DTOs. Superseded
ownership layouts reject without aliases or migration. Every Alive owner carries a strictly
ordered `part_velocities` entry for each original source part, with finite bounded
installed linear/angular velocities. Corpse lists are empty; its independently
moving component frames remain authoritative. Source-content and topology-only
hashes exclude measured motion; full/motion and enclosing commit hashes bind it.
The pre-motion overlay applies the complete measured list before validating the
expected before hash, including when a client received no motion datagram.

The owning simulation thread samples pending-safe physical part motion with its
existing source/pose/owner capture and prepared checked-death freshness fence.
Zero part velocities are authored only for the existing stationary AliveBirth
installation. The unchanged checked death witness attests the resulting installed
root transfer; the overlay and complete before hash bind its input motion.
Replica/recovery do not rerun that physical calculation or keep another ledger.
Nested velocity vectors are charged against local/population/attachment work and
owned capacity before allocation or hashing. Wire 11 and earlier, owner/change/commit/bulk
schemas 1/2/3, and retired lifecycle kinds 6/7/8 have no compatibility path. Geometry
baseline 1 and store 3 layouts are unchanged; embedded old wire journals reject.
Complete actor checkpoints, baselines and typed runtime consumers remain gated.


Complete capture prerequisite (2026-10-18): ownership V4 records explicitly retain
`primary_component_serial`. None names the sole intact component; derived corpses
name an existing nonzero serial below their retained serial high-water. Arrays
remain sorted by serial; root pose and both twists must match the named primary,
which need not be the first component. Full and topology hashes bind that identity.
Empty source parts retain their source identity, hierarchy, bounds and content
hash with zero resident bricks; every complete animal still needs nonempty exact
component masks covering all original solid cells.

`CreatureSnapshotV2` schema 2 contains the existing creature allocator snapshot,
the actual shared `next_volume_id`, and one bounded `CreatureOwnershipSetV4`.
Even an empty set retains world identity, epoch, next creature ID and retired ID
gaps. Its full hash binds all those values. Validation requires owner world/epoch
agreement, creature IDs below the allocator high-water and source volume IDs below
the shared volume counter. The decoder caps the frame at 64 MiB + 128 bytes, uses
the existing cumulative ownership reader and strict envelope/allocator maps, then
validates catalog/identity and refuses trailing data. Raw Deserialize enforces
resource admission only. Installation must validate source uniqueness against
geometry and match the accompanying checkpoint's shared counter; this envelope
never replaces that counter or allocates another ID stream.

The Simulation now provides guarded read-only capture into this envelope,
including actual applied motion and complete corpse ownership. It does not install
actors or change baseline 1/store 3 layouts. Source revision-allocator gaps are
now retained explicitly in source V2 records. Joint prepared
host/source/physics restore, actor checkpoint/baseline sections and ordered runtime
replay remain required. Geometry-only checkpoint refusal and closed production
spawning remain in force.


Simulation capture (2026-10-18) preserves a distinction between host logical
`pose_epoch` and physics' runtime authored `pose_token`. Birth can have runtime
token zero, and Death advances the logical epoch independently. The owner-thread
adapter reads both in one immutable window: raw runtime tokens fence physical
capture, while the authoritative logical epoch is written into the portable
record and cold-restore bundle. A fresh physics instance initializes its token
from that portable epoch. No raw runtime token or counter is added to the wire.
`capture_dynamic_corpse_owner_at_epoch` makes this mapping explicit; the existing
direct physics capture keeps its existing token-as-portable-epoch contract.

Capture validates exact host/source/metadata/reverse-index/physics-owner bijection,
source freshness and shared volume namespace, then the actual installed Alive
source cells/materials/revisions/hierarchy/cell scale. Corpse capture validates the
complete physical partition. Original sparse bricks, revisions, edited flags,
bounds and authored transforms are retained separately from current/frozen poses.
Before output construction, two-pass population admission bounds metadata,
nonuniform source scans, dense source payload, solids, reconstruction grids,
components, mask count and capacity (including geometric growth and map overhead).
Conservative mask bounds may refuse a population that would fit after exact
partition counting; they do not raise existing limits. Physics reconstruction
scratch retains its separate bounded per-owner grid/work ceilings.

Current source records use `CreatureSourcePartV2`, which requires the actual
`next_brick_revision` counter, positive and strictly above every resident brick
revision. The source-content v2, full owner v4 and topology v4 hashes bind it;
motion-only hashes exclude it. Static-source lifecycle checks preserve it across
Death. Current-only owner/set/change/operation/commit/bulk V4 schemas/domains4,
snapshot V2/schema2/domain2, wire/handshake12 and lifecycle kind9 reject superseded
layouts without aliases or migration. Unchanged overlays and part velocities stay
V2; source masks/local poses/components keep V1. Baseline1 remains guarded;
store5 retains the complete checkpoint section introduced in store4, described below.

Simulation capture now records every actual volume.next_revision, including
retired gaps, initially empty/emptied sources and u64::MAX exhaustion. The previous
SourceRevision representability refusal is removed. The pure
`reconstruct_creature_sources(snapshot, catalog)` validates the whole population
before copying cells, returns unpublished saved-ID source volumes with original
bricks/revisions/edited flags/cell size/bounds, then resumes each counter exactly.
`Volume::resume_revision_counter` refuses zero or rollback without mutation and
never derives a replacement from resident maxima. The u64::MAX counter is retained
as exhausted; subsequent edits must refuse rather than reuse a revision. This
source adapter neither installs ownership/physics nor advances any world IDs.
Joint prepared Simulation restoration is available as described below. Complete
checkpoint/baseline sections and ordered runtime consumers remain prerequisites
for animal saving, spawning and 1 km acceptance.


Joint cold animal installation (ENG-218, 2026-10-18) uses the current snapshotV2
without changing wire12/baseline1/store3 layouts. After geometry recovery,
`Simulation::prepare_creature_restore(expected_world, snapshot, catalog)` requires
an uninitialized host, all five source indexes empty, exact expected WorldId and
shared next-volume counter, and disjoint geometry/source IDs. It reconstructs the
original volumes and their actual revision counters, admits the complete physical
grid/mask work under existing per-owner/population ceilings, and prepares one
AliveRestore/ColdRestore batch plus a detached host and source store. Saved world
positions must roundtrip through the recovered physics origin exactly; otherwise
installation refuses instead of changing authoritative positions.

Applied local poses and measured Alive part velocities are retained. Corpse
components keep masks, independent motion, source generation, serial high-water
and named primary; portable serial order is converted to primary-first runtime
order only during reconstruction. The host keeps the existing saved creature
allocator and record IDs without consuming IDs. Empty populations install their
saved allocator with no physical operations.

`commit_prepared_creature_restore` revalidates world identity, geometry/physics
fences, origin, registry counters, journal cursor, bounded catalog fingerprint,
empty target indexes and geometry namespace, then validates physics before any
publication. It publishes physics, fills preallocated owner-handle slots, swaps
the complete source store and installs the complete host. No fallible checks or
source/host map allocations follow physics publication. It changes neither shared
registry counters nor journal entries. A stale/foreign/invalid candidate leaves
all participants unchanged. The API never replaces even an initialized empty
host. This does not enable actor checkpoints, baseline consumers or live spawning;
the existing incomplete-format guard remains until their own atomic integration.


Complete animal checkpoints (ENG-218, 2026-10-19) advance current-only store schema
from3 to4, with no migration. Baseline1 and wire12 do not change. Every checkpoint
has a required checkpoint_creatures row containing a bounded strict Option of
StoredCreaturesV1, a matching has_host flag and BLAKE3-16 checksum. None explicitly
means uninitialized, while Some(empty) retains the saved allocator epoch/high-water.
The section carries the unchanged SnapshotV2, its full canonical hash and finite
physics origin. Length is queried before SQLite copies a BLOB. Missing/empty/
oversized/bad-CRC/flag-mismatched or invalid bounded sections invalidate that
checkpoint; they are never converted into an absent population.

Publication preflights bounded population encoding, snapshot/hash/schema, unique
creature IDs even across generations, exact world and shared next-volume counter,
all source cell-size codes, source content hashes and disjoint geometry/source IDs.
The actor row joins existing body/brick/environment rows, world metadata, cursor
and complete marker in the same SQLite transaction and checkpoint crash cuts.
Inside BEGIN IMMEDIATE, the writer checks the prior required actor row, its bounds,
CRC/framing/flag and bounded hash. It cannot drop an initialized host. Until typed
animal replay is implemented, actor checkpoint publication refuses a cursor below
the durable high-water from both journal rows and retained checkpoint cursors;
further journal appends after an initialized-host checkpoint also refuse. Refusals
roll back and poison the writer under the existing failure policy. Actor-free
checkpoint/journal suffix behavior is retained.

Snapshot topology hashing has an explicit v1 domain and binds allocator, source
counter and owner topology hashes, excluding sampled motion. Checkpoint topology
composition is version3: initialized hosts compose this with the unchanged geometry
v2 hash in a separate domain; uninitialized actor-free worlds return the original
geometry hash exactly. Complete SnapshotV2 canonical hashes separately bind all
sampled motion. SimWorld::geometry_topology_hash is explicitly partial; ordinary
world_hash refuses source owners in production as well as tests.

Server persist::capture_with_catalog and restore_with_catalog require game-owned
references. Capture retains all actual sources/motion through the guarded Simulation
adapter. Restore verifies the section and catalog, refuses every nonempty actor
journal suffix, rebuilds geometry detached in the persisted physics frame, verifies
composed topology, jointly installs the saved actor candidate, then verifies exact
complete recapture hash before returning the Simulation. Catalog-free callers
continue to refuse initialized hosts/actor checkpoints. Structural load failures
use the existing corruption/fallback policy and require explicit acceptance;
semantic catalog/physics/hash failures return an error without silently selecting
an older checkpoint. RecoverBase::AtOrBefore(tick) lets an explicit prior-checkpoint
choice reload the correct suffix from that checkpoint's own cursor. Never reuse a
previous checkpoint with a suffix loaded relative to the latest checkpoint.

These entrypoints establish complete checkpoint restoration only. Live animal scheduling,
actor journal/attachment/replica and baseline consumers, bulk production and
source-edit/fracture synchronization remain closed. Production
spawning and combined 1024m animal/weather launch acceptance remain gated.

The catalog-aware full-capture adapter requires all logical geometry resident.
It refuses world.has_evicted() before copying any terrain or actor payload, so a
checkpoint never combines a complete logical hash with resident-only rows. The
incremental ResidencyPass::capture_checkpoint_with_catalog supplies the complete
logical terrain set from its existing verified backing/cache path to the same
animal capture builder. Its runtime-only world-instance token prevents reuse
across replacement worlds; no token is serialized. Missing/digest-mismatched
backing, invalid catalog or incomplete ordinary bodies fail without publishing
cache/counter changes. Catalog-free capture still refuses initialized hosts.
Store4/wire12/baseline1 layouts are unchanged by this adapter. Live serve catalog
handoff and typed animal consumers remain required before 1km acceptance.


Typed durable transaction attachments (ENG-218, 2026-10-19): current-only store5
replaces store4 without migration. JournalPayload::TopologyAttachments replaces
the geometry-only bulk journal variant. It retains current-wire parent and body
participant controls plus existing TransactionAttachmentSetV1 bytes. The envelope
validates every required bulk marker exactly once by family and transfer ID,
parent transaction/operation ordinal/hash and existing cumulative decoded-work
budgets. The geometry producer adapter supplies its existing allocator-reserved
ID; startup retained-ID seeding decodes the same typed record. Wire12, baseline1
and animal owner/snapshot layouts are unchanged. No second journal or ID ledger.

Typed parent controls cap at64KiB; participant count caps at4096, each control
at64KiB, total participant bytes at64MiB; attachment bytes cap at64MiB before copy.
Recovery borrows SQLite row blobs and bounded postcard fields rather than copying
an unchecked whole journal blob. Full journal row decoding refuses trailing data.
Nested geometry/lifecycle decoding keeps the existing family-specific cumulative
budgets. Typed rows must match parent server_tick; invalid records poison the
writer before any prefix/outbox publishes. Recovery reports the invalid record
and retains only the validated durable prefix under the existing corruption policy.

The store can retain bounded lifecycle and mixed attachment data, but Simulation
replay explicitly refuses lifecycle attachments before applying the transaction.
Existing initialized-host journal suffix guards remain. The in-memory journal
now carries the same typed attachment set from preparation
through publication and durable conversion. Bounded bulk lifecycle preparation
accepts already reserved IDs; the live server intent/reservation caller, atomic
Simulation replay, client baseline/replica and live catalog handoff remain ENG-218
work. Storage retention is not live actor replay acceptance.


Bounded authoritative bulk production (ENG-218, 2026-10-19): the internal
Simulation preparation path accepts caller-owned geometry/lifecycle transfer
reservations. The common lifecycle namespace value (bit61) is now exported by
protocol and shared with the existing server allocator; no ID values change.
Intents and supplied ID counts are bounded before traversal. IDs require nonzero
low values, the exact namespace and unique lifecycle entries. Simulation creates
no transfer allocator or persistent transfer counter. Global reservation/retained
ID uniqueness remains the existing server allocator's responsibility.

Inline-fit transactions retain the inline lifecycle form. Oversized transactions
with sufficient reserved IDs convert all actor ops to V4 bulk markers, preserving
operation ordinals and exact already-reduced/sampled commits, including checked
Death witness and measured part motion. Final marker framing must fit64KiB, and
the complete typed set (geometry plus actors) must pass existing correlation,
cumulative work and actual encoding before validation/publication. Missing IDs,
invalid sources/counters/catalogs or stale fences return without mutation.

JournalEntry.attachments replaces its geometry-specific bulk field. The geometry
producer prepares its typed set before publication as well; persist::journal_records
encodes that same set, with no geometry-specific reconstruction after commit.
Mixed publication emits one journal entry after both physical owners. A geometry-
only borrow adapter refuses any actor transaction, including inline actors next
to a sole geometry attachment. Store5/wire12/baseline1 layouts are unchanged.
This internal producer does not enable live scheduling, baseline/replica or
ordered animal replay; existing recovery refusals and spawning gate remain.
