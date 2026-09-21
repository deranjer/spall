# Large retained-world baselines — measurement and design (ENG-30 / T23) — 2026-09-21

Status: **measurement done; design revised 2026-09-21; implemented and measured** (see G3.md increment 43). ENG-30 stays open and T23
unaccepted. The cap is **not** raised and the workload is **not** reduced.

## 1. The failure, precisely

A fresh client reconnecting to the retained v2-soak world is refused: the server's baseline capture
fails with `TooLarge { bytes: 395168264, cap: 268435456 }`. The number that exceeds the cap is the
**raw canonical (decompressed) payload**, not wire size. Wire size is tiny.

## 2. Measurements (retained save, copy of the v2 30-minute soak's `world.db`)

Reproduce: `SPALL_RETAINED_SAVE=<copy of world.db> cargo test -p spall_server --release --test
baseline_measure -- --ignored --nocapture` (`crates/spall_server/tests/baseline_measure.rs`; a counting
global allocator reports the peak *additional* heap of each stage).

| Item | Terrain | Body geometry | Metadata |
| --- | --- | --- | --- |
| Volumes | 1 | 11,842 | (ids, coords, revisions, flags) |
| Bricks | 84 (all dense) | 12,098 (11,968 dense, 130 uniform) | |
| Raw postcard bytes | 2.6 MiB | **374.0 MiB** cell payload | **0.2 MiB** |
| Largest single volume | 84 bricks | 80 bricks | |

- **Body geometry is 99.3% of the payload.** Metadata is 0.05%; terrain 0.7%.
- The dense bricks are almost empty: **885,399 of 394,919,936 cells (0.22%) are non-air**, 11,734 of the
  bricks are under 1% occupied, and **all 12,052 dense bricks hold at most two distinct values**.
  Postcard spends one byte per cell, so a 32 KiB-cell brick costs ~32 KiB raw for a few hundred solid cells.
- **Wire size is 0.2 MiB** (zstd), against a 64 MiB wire cap. The cap that trips is the 256 MiB
  decompressed bound, which exists to bound decompression output.
- Peak additional heap (release build):

| Stage (server unless noted) | Peak additional heap |
| --- | --- |
| Restore from save | 794 MiB (the live world) |
| `logical_world_baseline` (Vec<u16> copy of every dense brick) | 757 MiB |
| `BaselineWorld::encode` (single blob) | 512 MiB (Vec growth) on top of 377 MiB raw |
| `encode_compressed` | 512 MiB on top of the same raw |
| **Whole failing capture** (`logical_capture_transfer`) | **2,021 MiB, then fails** |
| Client: `decode_compressed` (same 256 MiB bound) | 512 MiB, then **refuses** (`BlobTooLarge`) |

So the failure is symmetric: even a working server could not deliver this world, because the receiver
enforces the same bound. The server burns ~2 GiB transiently on the way to failing, on the simulation
host, per late join. The *live* world already holds ~750 MiB of dense bricks; a transfer must not
multiply that.

## 3. Why not the obvious fixes

- **Raise the cap:** the transient cost scales with the raw size (2 GiB now); a bigger world or several
  concurrent joiners multiplies it. It also weakens the decompression-bomb bound. Rejected.
- **Reduce the world/workload:** the debris population is the required G4 workload. Rejected.
- **Compress harder:** the wire is already 0.2 MiB; the problem is the in-memory/decoded size.

## 4. Design (revised 2026-09-21): segmented, dependency-complete baselines

`docs/protocol.md` already states the direction: *"Larger regions are split into multiple
dependency-complete transfers. Each connection has bounded staging memory and a timeout"*, plus
late-join steps 3-5 (validate, install atomically, ordered catch-up, bounded catch-up queue, retry
limit). A baseline stays **one transfer** (one `BaselineBegin`, one bulk stream, one `BaselineEnd`,
one snapshot cursor `J`); what changes is that its payload is a sequence of independently bounded,
independently validated **segments** instead of one blob. It is not wire chunking: the segment is the
unit of allocation, hashing, validation and staging on both ends.

### 4.1 Memory accounting

Four different things are called "memory" in a transfer; they are budgeted separately and only the
first is bounded independently of world size.

| Class | What | Bound |
| --- | --- | --- |
| **A. Bounded temporary buffers** | Per active capture (server worker) or per receiving transfer (client): one segment's decoded cells, its canonical postcard bytes, the zstd scratch and output, one reassembly frame. | `O(segment budget S)`, independent of world size and of recipient count (server: `capture_workers x`, default `<= 4`). |
| **B. Shared retained segments** | The compressed, framed parts of a capture, shared by every joiner at that journal cursor (`Arc`, today's `BaselineTransfer.parts`) until the last recipient finishes or is cut, and by the one `cached_baseline`. | Compressed size of the transfer, `<= MAX_ASSEMBLED_TRANSFER` (64 MiB) by the existing wire cap, **not** world-sized in decoded terms. The decoded `BaselineWorld` is no longer retained (it existed only for a unit test). |
| **C. Snapshot lifetime** | The copy-on-write `BaselineSnapshot` (Arc'd bricks shared with the live world). | From capture on the tick thread to the end of *encoding* on the worker (not to the end of *sending*). While alive it pins the pre-edit version of every brick the world edits, at most one 64 KiB dense brick per edited brick. It is dropped before any part is sent; `cached_baseline` keeps only class B. |
| **D. World-sized staging / existing memory** | Server: the live world (unchanged, ~750 MiB dense for the retained save). Client: the **staged replica** built while segments arrive, then swapped in atomically; a client that already holds a world holds old + staged until the swap. | `O(world)` by nature; declared up front (`total_decoded_bytes` in the manifest) and admission-checked against the client's budget before any allocation. Reported, never called "bounded temporary". |

*Budgeting decoded allocations, not serialized bytes.* One dense brick is 32,768 cells: 64 KiB decoded
(`Vec<u16>`), 32-96 KiB as postcard (1-3 bytes per cell varint), and a further copy when the client
converts to `Brick::restored`. So the segment budget `S` is stated in **decoded bytes** (dense brick
= 65,536 + 64 bookkeeping, uniform brick = 64) and the planner fills a segment until the next brick
would exceed `S`. Derived checks, enforced on both ends: raw postcard `<= 2 S + 64 KiB`, compressed
frame `<=` raw cap + 1 KiB, decoded bytes actually counted after decoding `<= S`. The peak
temporary allocation of a stage is therefore modelled as `~ (1 decoded + 1.5 raw + 1 zstd out) x S`
plus one compressed frame, i.e. **about 3.5 S to 4 S**; with the default `S = 4 MiB` that is
~16 MiB, with `S = 16 MiB` it would be ~64 MiB. **The 64 MiB transient figure is a hypothesis, not a
result:** it is claimed only after the counting-allocator measurements in section 7 report it for
the retained save, for the server capture and the client receive separately, with a slow recipient
and concurrent joiners.

### 4.2 Framing

The bulk payload (the concatenation of the existing `BaselinePart`s, unchanged part size and part
hashes) is a sequence of frames, each `kind: u8` + `len: u32 LE` + body:

- `kind 0`, **manifest** (always the first frame): postcard `SegmentManifest { schema: u16,
  segment_count, volume_count, total_bricks, total_decoded_bytes, segment_decoded_cap }`. The client
  validates every field against its own limits and budget **before decoding anything else**.
- `kind 1`, **segment**: `raw_hash: Hash32` (BLAKE3 of the raw postcard) + a zstd frame of postcard
  `BaselineSegment { index, volumes: Vec<SegmentVolume> }`, `SegmentVolume { volume_id, header:
  Option<VolumeHeader>, first_ordinal, bricks: Vec<BaselineBrick>, last }`. `header` (cell-size code,
  owner, bounds) rides only in a volume's first segment. Volumes are packed in ascending `VolumeId`
  order; a volume larger than `S` continues across consecutive segments in canonical `(z, y, x)`
  brick order with `first_ordinal` proving contiguity and `last` closing it. `len` is checked
  against the raw cap **before** buffering the frame.
- `BaselineEnd.assembled_hash` (v2) = BLAKE3 over the manifest bytes followed by every segment's
  `raw_hash` in order. Neither side re-encodes the whole world to check it.

Decompression keeps today's protection: output is read through `take(cap + 1)` per segment with the
per-segment raw cap, so a bomb can never allocate past it; the v1 256 MiB whole-blob bound stays in
force for v1 transfers and for split/repair blobs (`BaselineVolume::decode_compressed`,
`BaselineWorld::decode_compressed` unchanged).

### 4.3 Version negotiation

- `BaselineBegin.world_version`: `1` = today's single blob, `2` = segmented. Nothing else in the
  record changes, so v1 stays byte-identical.
- A client advertises segmented support **backward-compatibly** in the baseline request it already
  sends: the sentinel `BaselineAck` (`transfer_id == BASELINE_REQUEST_SENTINEL`) carries
  `verified_manifest_hash = BASELINE_CAP_SEGMENTED` (a fixed well-known hash) in a field the sentinel
  otherwise leaves unused. An old server ignores it and serves v1; an old client sends the zero hash.
- The server sends v2 when the client advertised it **and** (the world does not fit a v1 blob, or the
  operator forced segmentation with a segment budget); otherwise v1. A client that did not advertise
  segmented support and whose world does not fit v1 is refused explicitly (the connection is closed
  with a reason naming the required capability), never sent a truncated or partial world.
- A client that receives a `world_version` it does not implement refuses the transfer explicitly
  (`join-failed`, reason names the version).

### 4.4 Server: streamed capture, paced transfer, shared retention

`snapshot_world` still takes the COW snapshot on the tick thread. On the capture worker a **segment
planner** walks the snapshot's volumes and bricks (cheap: no cell expansion) to lay out segments by
decoded cost, then a **segment encoder** expands one segment at a time (class A), hashes it,
compresses it, frames it and appends the frame's parts to the shared `parts` (class B), dropping every
temporary before the next segment. The snapshot is released when the last segment is encoded
(class C). `send_baseline_paced` is unchanged: it sends the parts in order, paced at the existing
per-client rate, so a slow recipient holds only its cursor into the shared `Arc` parts (it never
causes a copy), and the transfer's existing timeout / catch-up cap cancels it. Concurrent joiners at
one cursor share the same `parts` (as today); the per-recipient additional memory is the QUIC send
window, not a segment copy. Retention while a slow recipient lags is `<= compressed transfer size`
per distinct capture, and distinct captures are limited by `capture_workers` and the single cached
baseline.

### 4.5 Client: streaming validation, staged build, atomic install

A frame reader consumes parts as they arrive and yields whole frames; it never holds more than one
frame. Each segment is decoded under the per-segment bounds, its `raw_hash` and structure are
verified, and it is folded into a **`StagedBaseline`** (class D) that checks: manifest order and
counts, `index` monotonic with no gap/duplicate/reorder, per-volume `first_ordinal` contiguity,
header present exactly once and first, `last` closes each volume, no volume opened twice, dense-brick
length, cell-size code and bounds valid, brick coordinates inside declared bounds, total decoded bytes
within the manifest and within the client budget. Nothing is visible to prediction or rendering until
`BaselineEnd`: the chained hash must match, every volume must be closed, terrain must exist, and only
then `install_staged` swaps the staged maps in (the same swap `install_baseline_world` performs
today, which is re-expressed as a one-segment staged build so there is one install path). Any failure
drops the staging and fails the join explicitly, naming the segment index; the replica keeps its
previous state untouched.

### 4.6 Snapshot cursor, catch-up ordering, failure behaviour

Unchanged contract. The cursor `J` and tick are fixed at snapshot time and carried in `BaselineBegin`;
transactions after `J` queue per joiner exactly as today and are applied after the install, in order;
a burst past the cap cancels and recaptures a fresher baseline within the existing retry limit; an
exhausted retry budget ends in the existing bounded explicit `join-failed`. A capture error (for
example over the total budget) is reported by name, as increment 41 did for `TooLarge`.

## 5. Complementary, separate option: compact brick encoding

All 12,052 dense bricks of the retained save hold one material plus air at 0.22% occupancy. A
`Masked { material, bitmask }` cell encoding would shrink the raw payload roughly 50x. It is a schema
change on both sides and a separate decision; segmenting is required regardless (arbitrarily large or
dense worlds) and does not depend on it.

## 6. Tests and acceptance

Fixtures are small and synthetic with a **configurable low segment budget** (a few dense bricks), so
oversized volumes, ranges and budget exhaustion are exercised in milliseconds:

1. Protocol: frame/manifest round trip; oversized volume spanning many segments; missing, duplicate
   and reordered segments and brick ranges refused; wrong `first_ordinal`; corrupt segment hash;
   corrupt/oversized `len`; decompression bomb refused at the per-segment cap; manifest over budget
   refused before any segment; chained-hash mismatch; volume left open at `BaselineEnd`.
2. Server: planner respects the decoded-cost cap for dense and uniform bricks; a single brick larger
   than the cap is a hard error, not a silent overflow; capture retains parts only (no decoded world);
   v1 chosen for small worlds, v2 when forced or oversized, explicit refusal for a non-advertising
   client with an oversized world.
3. Session (real QUIC): a low-budget synthetic world converges to the server hash with the client
   reaching ready; **edits committed during the transfer** arrive via catch-up and converge;
   **cancellation** (server or client dies mid-transfer) leaves the previous client state intact;
   **budget exhaustion** on the client fails the join explicitly; a slow recipient and eight
   concurrent joiners share one retained capture and the retained-memory bound holds (counting
   allocator).
4. Large-world acceptance (separate from the synthetic tests): the retained soak save through a real
   server + client reconnect: matching topology hash, required body state (count, ids, poses),
   measured server and client peak memory (capture, receive, staged world), readiness time, and the
   existing join/recovery regressions (`t23-g3`, `t23-g3-impaired-join`, `t23-g4-join-budget`,
   restart check).
5. Full `cargo xtask check` on the final integration revision.

## 7. Measurements (2026-09-21; see G3.md increment 43 for the table)

Retained save, release, counting allocator. Server encode peak additional heap 1.6 / 6.1 / 24.1 MiB and client receive temporary ~0.9 / 3.1 / 12.1 MiB at segment budgets 1 / 4 / 16 MiB (about 1.5 S and 0.75 S, better than the 3.5-4 S model above); retained after capture 0.2-0.3 MiB; 8 concurrent joiners add 1.5 KB; client staged world 769 MiB (class D). The 64 MiB transient target is met for budgets up to 16 MiB; the default is 4 MiB. Real reconnect: 192 segments, 244 KB, hash match, ready 13.8 s, server 987 MiB / client 799 MiB peak working set. Still unmeasured: a real slow QUIC recipient under the allocator.

## 8. Risks / open questions

- `SplitOffBulkBaseline` reuses `BaselineWorld` blobs (bounded by their own 64 MiB compressed cap);
  they stay v1 and are out of this pass.
- Per-segment zstd loses cross-segment redundancy; irrelevant at 0.2 MiB total, measured anyway.
- The client's staged world is real, world-sized memory (~750 MiB for this world) and must be
  reported against the 4 GiB client target; only the *transient* blow-up is removed.
- Interest-limited (regional) baselines remain the long-term way to avoid sending a whole world; a
  region is just a set of volumes/ranges and fits this framing.
