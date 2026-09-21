# Large retained-world baselines — measurement and design (ENG-30 / T23) — 2026-09-21

Status: **measurement done, design proposed, nothing implemented.** ENG-30 stays open and T23
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

## 4. Design: segmented, dependency-complete baselines with bounded staging

`docs/protocol.md` already states the direction: *"Larger regions are split into multiple
dependency-complete transfers. Each connection has bounded staging memory and a timeout"*, and late-join
steps 3-5 (validate, install atomically, apply catch-up in order, bounded catch-up queue, retry limit).
Today a baseline is a single `BaselineWorld` blob; the design makes it a sequence of independently
bounded **segments**.

### 4.1 Wire format (baseline schema 2, negotiated via `BaselineBegin.world_version`)

- `BaselineBegin` (control, small) gains **counts only**, known from the snapshot before any byte is
  sent: `segment_count`, `total_raw_bytes`, `total_bricks`, `volume_count`. The client checks them against
  its own budgets (staging, resident memory, `max_dense_bytes`) **before allocating** and can refuse with
  an explicit reason instead of a mid-transfer failure. The existing `regions` manifest is unchanged.
- The bulk stream carries **segments**. A segment is one independent zstd frame of a postcard
  `BaselineSegment { index, raw_len, volumes: Vec<SegmentVolume> }` and is split into the existing
  1 MiB `BaselinePart`s (parts stay globally numbered; each `part_hash` is unchanged). Limits:
  `MAX_SEGMENT_DECOMPRESSED = 16 MiB` (raw, per segment), `MAX_ASSEMBLED_TRANSFER = 64 MiB` (unchanged;
  compressed total), `MAX_BASELINE_PARTS` unchanged, new `MAX_BASELINE_SEGMENTS = 1024` and a
  `MAX_BASELINE_TOTAL_RAW` tied to the client's declared budget rather than a fixed constant.
- `SegmentVolume { volume_id, header: Option<VolumeHeader>, brick_lo, brick_hi, bricks }`. The header
  (owner, cell-size code, bounds) rides in the volume's **first** segment. Volumes are packed
  first-fit up to the raw cap in ascending `VolumeId` order, so ordinary bodies (<= 80 bricks) never span
  segments.
- **Oversized single volume** (raw > segment cap, e.g. a 64-brick giant with dense bricks, or terrain of
  arbitrary size): the volume is emitted as consecutive **brick-range continuations** in canonical
  `(z, y, x)` order, each segment holding as many bricks as fit, with `header` only on the first and a
  `last: bool` on the final range. Each range states `brick_lo..brick_hi` (ordinals) so the client
  verifies contiguity. Terrain is always segmented this way; no volume size can exceed a single segment's
  bound by construction. `spall_voxel` bounds and the canonical hash are unchanged.
- `BaselineEnd.assembled_hash` becomes the BLAKE3 over the ordered `(segment_index, raw_len,
  segment_raw_hash)` list, so neither side re-encodes the whole world (today the client re-encodes the
  entire world just to check the hash). Per-segment raw hashes are verified as each segment decodes.
- A world that fits one segment is exactly one segment: small worlds (the existing tests and fixtures)
  cost the same as today.

### 4.2 Server: streamed capture from the COW snapshot

`snapshot_world` already takes a cheap copy-on-write view at a tick boundary. Replace
`transfer_from_snapshot`'s "expand everything to `Vec<u16>`, `encode`, `encode_compressed`" with a
**segment iterator** on the background worker: take bricks from the snapshot, expand only the current
segment (<= 16 MiB), encode, hash, compress, emit its parts, drop it. Peak additional heap becomes
`O(segment)` (~16 MiB raw + its compressed output), independent of world size, versus 2 GiB today. Emitted
parts are handed to the paced sender (`send_baseline_paced`, 1 MiB/s cap) as they are produced, with a
bounded in-flight window, so a slow client does not pin the whole encoded transfer. Concurrent joiners
keep the existing `captures active max 4` limit; total transient memory is `4 x segment`.
`reissue` (shared immutable geometry for a burst of joiners) keeps working: segments are `Arc`-shared
and retained only until every recipient has consumed them (bounded by the retry window).

### 4.3 Client: validated staging, atomic install

- Segments are decoded and **validated one at a time** (schema, dense-brick length, contiguity,
  per-segment hash, per-volume completeness, cell-size code, bounds) into a **staged world builder** that
  is not visible to prediction/rendering. Peak additional memory is one decoded segment plus the staged
  bricks themselves; the client never holds the raw whole-world blob or its canonical re-encode.
- Dependency-completeness: a volume is *complete* only after its `last` range; the install step refuses
  a staged world with an incomplete volume, a missing terrain, or an entity referenced by a body volume
  but absent (hard failure -> explicit bounded `join-failed`, never a partial world).
- **Atomic install** at the end swaps the staged world in (`install_baseline_world` semantics), then the
  existing catch-up barrier applies queued topology records in order from `journal_cursor`. Motion is
  still held until topology exists. Nothing in the live replica changes until the swap.
- Bounded staging: the client's declared budget (default derived from `max_dense_bytes`) is compared to
  `total_raw_bytes` from `BaselineBegin`; a transfer over budget is refused up front with a reason
  ("baseline needs X MiB of staging, budget Y").
- Timeouts and retries: unchanged in policy (per-transfer deadline, retry limit, cancel and recapture a
  fresher snapshot on catch-up exhaustion). New: a segment that fails validation cancels the transfer with
  the segment index in the diagnostic.

### 4.4 Catch-up ordering

Unchanged contract: the snapshot cursor `J` is fixed at capture; the server retains topology after `J`
in the bounded catch-up queue while segments stream. Segmentation lengthens nothing in that path; the
catch-up queue bound and the "cannot catch up -> cancel and recapture" rule already exist. Because
capture is streamed, the *snapshot* stays coherent (it is COW at tick T) while the encoding of segments
runs on the worker over that immutable view, so segments of one transfer are mutually consistent.

### 4.5 Complementary, separate option: compact brick encoding

The measurement shows all 12,052 dense bricks hold one material plus air at 0.22% occupancy. A
`BaselineCells::Masked { material, bitmask }` (or palette + RLE) would cut the raw payload from 377 MiB to
a few MiB and shrink every stage above ~50x, and would also make journal/checkpoint bytes smaller. It is a
**schema** change on both sides and a separate decision; segmenting is required regardless (arbitrarily
large worlds, dense terrain) and this design does not depend on it.

## 5. Regression cases and acceptance

1. **Retained save** (`retained_save_yields_a_bounded_baseline_transfer`, ignored, currently failing with
   `TooLarge`): must produce a transfer; peak additional heap must be `<= 64 MiB` (asserted); client
   staging peak must be `<= budget`.
2. Server + real client process on a copy of the retained save (`.local/runs/reconnect-repro/run.sh`): the
   fresh client reaches ready and the agreed hash (this is the failing v2-soak "reconnect" row).
3. Oversized single volume: a synthetic 4,096-brick volume (> 1 segment) round-trips; a volume with a
   gap/reordered/duplicate brick range is refused; a missing `last` segment is refused; corrupt segment
   hash/part is refused with the segment index.
4. Bounded staging: a `BaselineBegin` over the client's budget is refused before any allocation; a peak
   allocation test (counting allocator) shows client and server peaks independent of world size.
5. Ordering: an edit committed to a brick in an *already delivered* segment while later segments are
   streaming is applied after install via catch-up, hash-equal to the server.
6. Interop: schema-1 single-blob baselines keep decoding (or the version mismatch is an explicit refusal).
7. `t23-g3` (joins, impaired join, retry exhaustion) and `t23-g4-join-budget` scenarios unchanged.

## 6. Risks / open questions

- Schema bump touches `spall_protocol`, `spall_server::baseline`, `spall_client::net`/`replica`; the
  split-baseline (`SplitOffBulkBaseline`) path reuses `BaselineWorld` and needs the same segmenting or a
  documented exemption (its own 64 MiB bound).
- Per-segment zstd loses cross-segment redundancy (compressed total may grow a few percent; irrelevant at
  0.2 MiB).
- Client resident cost of the *staged world itself* (~750 MiB for this world) is real and must be
  reported against the 4 GiB client target; only the transient blow-up is removed.
- Not measured: client install peak for a world this size (the client refuses first). It will be measured
  once segments exist.
- Interest-limited (regional) baselines remain the long-term way to avoid sending a whole world; this
  design is compatible with them (a region is a set of volumes/ranges).

## 7. Implementation slices

1. `spall_protocol`: `BaselineSegment`, limits, `BaselineBegin` counts, hash chain; encode/decode tests.
2. Server segment iterator + paced send + bounded window; retained-save regression turns green.
3. Client staging builder + validation + atomic install; oversized-volume and corrupt-segment tests.
4. Scenario evidence: reconnect on the retained save, join-budget lanes, then a peak-memory report.
