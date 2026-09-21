//! Late-join baseline capture and bulk chunking (T17).
//!
//! `docs/protocol.md` "Late join": the server captures "an immutable,
//! dependency-complete regional snapshot at tick T and journal cursor J",
//! continues simulating, and streams "compressed, hashed baseline parts on bulk
//! streams" while retaining "subsequent relevant transactions in a bounded
//! catch-up queue". This module owns the first half: turning the live
//! [`SimWorld`] into a [`spall_protocol::BaselineWorld`] blob and the
//! `BaselineBegin` / `BaselinePart` / `BaselineEnd` records that carry it.
//!
//! It carries **authoritative geometry only** — every resident brick with its
//! real revision, so the replica reaches the exact canonical topology hash with
//! no follow-up repair. Body motion is delivered afterwards as a
//! [`spall_protocol::MotionSnapshot`] keyframe per body (`docs/protocol.md`
//! step 4).

use spall_core::{BrickCoord, CELLS_PER_BRICK, JournalSeq, LocalCell, Revision, Tick, VolumeId};
use spall_protocol::segment::{
    self, BaselineSegment, SegmentManifest, SegmentVolume, VolumeHeader,
};
use spall_protocol::{
    BaselineBegin, BaselineBrick, BaselineCells, BaselineEnd, BaselineOwner, BaselinePart,
    BaselineRegion, BaselineVolume, BaselineWorld, Hash32, InterestEpoch, RepairKey, RepairRequest,
    TransferId, limits,
};
use spall_sim::{Body, BrickBacking, Simulation};
use spall_voxel::{Brick, BrickSnapshot, DigestError, EvictedBricks};
use std::sync::Arc;

/// World/content schema versions stamped into a `BaselineBegin`. These mirror
/// the T10 bridge session's fixed values; real negotiation is a later task.
pub const BASELINE_WORLD_VERSION: u32 = 1;
pub const BASELINE_CONTENT_VERSION: u32 = 1;

/// A complete baseline ready to push to one client: the control-stream
/// `BaselineBegin` / `BaselineEnd` markers and the ordered bulk parts.
#[derive(Debug, Clone)]
pub struct BaselineTransfer {
    pub begin: BaselineBegin,
    pub parts: Arc<[BaselinePart]>,
    pub end: BaselineEnd,
    /// What a segmented (`world_version == 2`) transfer is made of; `None` for a single blob.
    /// The decoded world is deliberately **not** retained: only the compressed, framed parts are
    /// shared (`docs/reports/large-world-baseline-design.md`, class B).
    pub segments: Option<SegmentedStats>,
}

/// Shape of a segmented transfer, for telemetry and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentedStats {
    pub segments: u32,
    pub volumes: u32,
    pub bricks: u64,
    /// Sum of the decoded cost of every brick (what a receiver's staging must hold).
    pub decoded_bytes: u64,
    /// The per-segment decoded budget the sender used.
    pub segment_cap: u32,
    /// The largest decoded cost any one segment reached.
    pub max_segment_decoded: u32,
}

/// Immutable, copy-on-write geometry handed from the authoritative tick to a
/// background baseline encoder. It deliberately contains no runtime physics or
/// ECS handles.
#[derive(Debug, Clone)]
pub struct BaselineSnapshot {
    pub checkpoint_tick: u64,
    pub journal_cursor: JournalSeq,
    volumes: Vec<BaselineSnapshotVolume>,
}

#[derive(Debug, Clone)]
struct BaselineSnapshotVolume {
    volume_id: spall_core::VolumeId,
    cell_size_code: u8,
    owner: BaselineOwner,
    bounds: Option<[[i64; 3]; 2]>,
    bricks: Vec<(BrickCoord, BrickSnapshot)>,
}

impl BaselineTransfer {
    /// Total bulk payload bytes (excludes the small control markers).
    pub fn payload_bytes(&self) -> usize {
        self.parts.iter().map(|p| p.payload.len()).sum()
    }

    /// Decodes a **single-blob** (v1) transfer's world from its parts (tests and diagnostics).
    pub fn decode_v1_world(&self) -> Result<BaselineWorld, spall_protocol::BaselineDecodeError> {
        assemble(&self.parts)
    }

    /// Reissues immutable baseline geometry under a fresh transfer id. The
    /// snapshot's cursor remains its honest capture cursor; the receiver drains
    /// all later topology records before it is promoted to live replication.
    /// This prevents a burst of simultaneous joiners from repeatedly blocking
    /// the authoritative tick thread serializing identical topology.
    pub fn reissue(&self, transfer_id: TransferId) -> Self {
        let mut begin = self.begin.clone();
        begin.transfer_id = transfer_id;
        let mut end = self.end;
        end.transfer_id = transfer_id;
        // The compressed parts are shared, not copied: a joiner's transfer id lives in `begin` /
        // `end` and is stamped on each part transiently when it is sent (`send_baseline_paced`).
        Self {
            begin,
            parts: Arc::clone(&self.parts),
            end,
            segments: self.segments,
        }
    }
}

/// Why a baseline could not be captured for transfer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    #[error(
        "baseline payload is {bytes} bytes; the assembled-transfer cap is {cap} (a bigger world \
         needs region splitting, T18)"
    )]
    TooLarge { bytes: usize, cap: usize },
    #[error("baseline needs {parts} parts; the ceiling is {cap}")]
    TooManyParts { parts: usize, cap: usize },
    #[error("segmented baseline: {0}")]
    Segmented(String),
}

/// Snapshots `sim`'s live world into an immutable [`BaselineWorld`] coherent
/// with the current tick. Equivalent to [`logical_world_baseline`] with no
/// backing — valid only while nothing is evicted.
pub fn world_baseline(sim: &Simulation) -> BaselineWorld {
    logical_world_baseline(sim, None)
}

/// Captures a stable, cheap copy-on-write view at a tick boundary. Expanding
/// snapshots into protocol cell vectors and compressing them is intentionally
/// deferred to [`transfer_from_snapshot`], which may run on a worker.
///
/// T23 / G3 row 7 follow-up: over the **logical** brick set, exactly like
/// [`logical_world_baseline`] — a resident brick is snapshotted directly, an
/// evicted one is read from `backing` and wrapped as an equivalent
/// [`BrickSnapshot`]. Before this, a currently-evicted terrain brick made this
/// panic (`background snapshots require resident geometry`): the periodic
/// checkpoint path used to paper over it by reloading every evicted brick
/// back into the live world before every checkpoint, which incidentally also
/// made most background baseline captures land on a fully-resident tick; once
/// that reload was replaced with bounded capture (this same follow-up),
/// evictions persist for the whole run and this path panicked for real.
/// Panics if a volume has evicted bricks and `backing` is `None` or cannot
/// supply one — a partial baseline is never emitted, same contract as
/// `baseline_volume`.
pub fn snapshot_world(sim: &Simulation, backing: Option<&dyn BrickBacking>) -> BaselineSnapshot {
    let world = sim.world();
    let mut volumes = Vec::with_capacity(world.body_count() + 1);
    let mut push = |body: &Body, owner| {
        let volume = &body.volume;
        let mut bricks = spall_voxel::logical_bricks(volume, world.evicted(volume.id()))
            .expect("baseline snapshot logical invariant")
            .into_iter()
            .map(|logical| {
                let coord = logical.coord;
                if let Ok(Some(snap)) = volume.snapshot_brick(coord) {
                    return (coord, snap);
                }
                // Evicted: its cells come from the durable backing (same
                // fallback `baseline_volume` uses for the synchronous path).
                let backing =
                    backing.expect("a background baseline snapshot over evicted geometry needs a durable backing");
                let brick = verified_backing_brick(
                    world.evicted(volume.id()),
                    volume.id(),
                    coord,
                    backing,
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "background baseline snapshot: durable brick {coord:?} of volume {} {error}",
                        volume.id()
                    )
                });
                (coord, brick.snapshot())
            })
            .collect::<Vec<_>>();
        bricks.sort_by_key(|(coord, _)| (coord.z, coord.y, coord.x));
        volumes.push(BaselineSnapshotVolume {
            volume_id: volume.id(),
            cell_size_code: volume.cell_size().to_u8(),
            owner,
            bounds: volume
                .bounds()
                .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
            bricks,
        });
    };
    push(world.terrain(), BaselineOwner::Terrain);
    for body in world.bodies() {
        push(body, BaselineOwner::Body(body.entity.expect("body entity")));
    }
    volumes.sort_by_key(|volume| volume.volume_id.get());
    BaselineSnapshot {
        checkpoint_tick: sim.current_tick().get(),
        journal_cursor: JournalSeq(sim.journal_cursor()),
        volumes,
    }
}

/// Expands an immutable tick-boundary snapshot and packages it for one client.
pub fn transfer_from_snapshot(
    snapshot: BaselineSnapshot,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
) -> Result<BaselineTransfer, BaselineError> {
    let world = BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: snapshot.checkpoint_tick,
        volumes: snapshot
            .volumes
            .into_iter()
            .map(|volume| BaselineVolume {
                volume_id: volume.volume_id,
                cell_size_code: volume.cell_size_code,
                owner: volume.owner,
                bounds: volume.bounds,
                bricks: volume
                    .bricks
                    .into_iter()
                    .map(|(coord, snap)| BaselineBrick {
                        coord: [coord.x, coord.y, coord.z],
                        revision: snap.revision().get(),
                        edited: snap.is_edited(),
                        cells: cells_of(&snap),
                    })
                    .collect(),
            })
            .collect(),
    };
    transfer_from_world(world, transfer_id, interest_epoch, snapshot.journal_cursor)
}

/// [`world_baseline`] over the **logical** brick set: every resident brick plus,
/// for each brick this server has evicted, its durable geometry from `backing`.
/// A late joiner therefore receives a complete world regardless of the server's
/// cache contents. Panics if a volume has evicted bricks and `backing` is
/// `None` or cannot supply one — a partial baseline is never emitted
/// (`docs/reports/G3-residency-hash.md` lifecycle).
pub fn logical_world_baseline(
    sim: &Simulation,
    backing: Option<&dyn BrickBacking>,
) -> BaselineWorld {
    let world = sim.world();
    let mut volumes = vec![baseline_volume(
        world.terrain(),
        BaselineOwner::Terrain,
        world.evicted(world.terrain().volume_id),
        backing,
    )];
    for body in world.bodies() {
        let entity = body.entity.expect("a detached body carries an entity id");
        volumes.push(baseline_volume(
            body,
            BaselineOwner::Body(entity),
            world.evicted(body.volume_id),
            backing,
        ));
    }
    volumes.sort_by_key(|v| v.volume_id.get());
    BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: sim.current_tick().get(),
        volumes,
    }
}

/// Captures a baseline and packages it for one client: `transfer_id` and
/// `interest_epoch` identify the transfer, `journal_cursor` is the highest
/// committed journal sequence the snapshot already includes (the replica's
/// catch-up barrier starts strictly after it).
pub fn capture_transfer(
    sim: &Simulation,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    journal_cursor: JournalSeq,
) -> Result<BaselineTransfer, BaselineError> {
    logical_capture_transfer(sim, None, transfer_id, interest_epoch, journal_cursor)
}

/// [`capture_transfer`] over the logical brick set — pulls evicted bricks from
/// `backing` so the transfer is complete even when this server has evictions.
pub fn logical_capture_transfer(
    sim: &Simulation,
    backing: Option<&dyn BrickBacking>,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    journal_cursor: JournalSeq,
) -> Result<BaselineTransfer, BaselineError> {
    transfer_from_world(
        logical_world_baseline(sim, backing),
        transfer_id,
        interest_epoch,
        journal_cursor,
    )
}

/// Packages an already-built [`BaselineWorld`] (a full snapshot or a one-brick
/// repair patch) into a [`BaselineTransfer`].
pub fn transfer_from_world(
    world: BaselineWorld,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    journal_cursor: JournalSeq,
) -> Result<BaselineTransfer, BaselineError> {
    // `docs/protocol.md` late-join step 2: "sending compressed, hashed
    // baseline parts on bulk streams". The size cap applies to the
    // *decompressed* canonical payload (the same ceiling
    // `BaselineWorld::decode_compressed` enforces on the receiving end), so a
    // bigger world still needs region splitting (T18) regardless of how well
    // it compresses.
    let raw = world.encode();
    if raw.len() > limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED {
        return Err(BaselineError::TooLarge {
            bytes: raw.len(),
            cap: limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED,
        });
    }
    let assembled_hash = Hash32::of(&raw);
    // `BaselineWorld::encode_compressed` re-encodes internally rather than
    // reusing `raw` — an acceptable one-time cost for a per-join capture, and
    // it keeps the zstd dependency centralized in `spall_protocol` alongside
    // `decode_compressed`.
    let payload = world.encode_compressed();

    let parts: Arc<[BaselinePart]> = chunk_payload(&payload, transfer_id).into();
    if parts.len() > limits::MAX_BASELINE_PARTS {
        return Err(BaselineError::TooManyParts {
            parts: parts.len(),
            cap: limits::MAX_BASELINE_PARTS,
        });
    }
    // At least one part always: an empty world is not a valid late-join target.
    debug_assert!(!parts.is_empty());

    let regions: Vec<BaselineRegion> = world.volumes.iter().filter_map(region_of).collect();

    let begin = BaselineBegin {
        transfer_id,
        interest_epoch,
        checkpoint_tick: Tick(world.checkpoint_tick),
        journal_cursor,
        world_version: BASELINE_WORLD_VERSION,
        content_version: BASELINE_CONTENT_VERSION,
        total_bytes: payload.len() as u64,
        part_count: parts.len() as u32,
        regions,
    };
    let end = BaselineEnd {
        transfer_id,
        assembled_hash,
        journal_cursor,
    };
    Ok(BaselineTransfer {
        begin,
        parts,
        end,
        segments: None,
    })
}

impl BaselineSnapshot {
    /// Conservative decoded cost of the whole snapshot (a dense-stored brick counts as dense even
    /// if its cells turn out uniform), computed without expanding any cells.
    pub fn estimated_decoded_bytes(&self) -> u64 {
        self.volumes
            .iter()
            .flat_map(|v| &v.bricks)
            .map(|(_, snap)| {
                if snap.is_dense() {
                    segment::DENSE_BRICK_DECODED_COST as u64
                } else {
                    segment::UNIFORM_BRICK_DECODED_COST as u64
                }
            })
            .sum()
    }

    /// Whether a single-blob (v1) transfer of this snapshot could exceed the whole-blob
    /// decompression bound: postcard is at most 1.5x the decoded size, so anything whose decoded
    /// estimate times 1.5 passes the bound is sent segmented.
    pub fn needs_segmentation(&self) -> bool {
        self.estimated_decoded_bytes() * 3 / 2 > limits::MAX_ASSEMBLED_TRANSFER_DECOMPRESSED as u64
    }
}

/// Packages `snapshot` as a **segmented** transfer (`world_version == 2`) with per-segment decoded
/// budget `decoded_cap`.
///
/// One segment is expanded, hashed, compressed and framed at a time; every temporary is dropped
/// before the next, so the peak is `O(decoded_cap)` regardless of world size. The snapshot's brick
/// handles are released as they are consumed. Only the compressed frames are retained (class B).
pub fn transfer_from_snapshot_segmented(
    snapshot: BaselineSnapshot,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    decoded_cap: usize,
) -> Result<BaselineTransfer, BaselineError> {
    transfer_from_snapshot_segmented_limited(
        snapshot,
        transfer_id,
        interest_epoch,
        decoded_cap,
        limits::MAX_ASSEMBLED_TRANSFER,
    )
}

/// [`transfer_from_snapshot_segmented`] with an explicit ceiling on the cumulative **compressed**
/// transfer. The ceiling is enforced as each frame is produced, **before** it is appended to the
/// retained payload, so a poorly compressible world fails at the first frame that would cross it
/// instead of growing the buffer to the end and checking afterwards.
pub fn transfer_from_snapshot_segmented_limited(
    snapshot: BaselineSnapshot,
    transfer_id: TransferId,
    interest_epoch: InterestEpoch,
    decoded_cap: usize,
    max_compressed: usize,
) -> Result<BaselineTransfer, BaselineError> {
    if !(segment::DENSE_BRICK_DECODED_COST..=segment::MAX_SEGMENT_DECODED_BYTES)
        .contains(&decoded_cap)
    {
        return Err(BaselineError::Segmented(format!(
            "segment budget {decoded_cap} is outside {}..={}",
            segment::DENSE_BRICK_DECODED_COST,
            segment::MAX_SEGMENT_DECODED_BYTES
        )));
    }
    let checkpoint_tick = snapshot.checkpoint_tick;
    let journal_cursor = snapshot.journal_cursor;
    let seg_err = |e: segment::SegmentError| BaselineError::Segmented(e.to_string());

    let mut payload: Vec<u8> = Vec::new();
    let mut raw_hashes: Vec<Hash32> = Vec::new();
    let mut segments = 0u32;
    let mut volumes_n = 0u32;
    let mut bricks_n = 0u64;
    let mut decoded_total = 0u64;
    let mut max_segment_decoded = 0usize;

    let mut cur = BaselineSegment {
        index: 0,
        volumes: Vec::new(),
    };
    let mut cur_cost = 0usize;

    // Encodes and clears the current segment.
    let flush = |cur: &mut BaselineSegment,
                 cur_cost: &mut usize,
                 payload: &mut Vec<u8>,
                 raw_hashes: &mut Vec<Hash32>,
                 segments: &mut u32|
     -> Result<(), BaselineError> {
        if cur.volumes.is_empty() {
            return Ok(());
        }
        let frame = segment::encode_segment_frame(cur, decoded_cap).map_err(seg_err)?;
        if frame.bytes.len() > 5 + segment::segment_frame_cap(decoded_cap) {
            return Err(BaselineError::Segmented(format!(
                "segment {} frame is {} bytes, over its {} byte cap",
                cur.index,
                frame.bytes.len(),
                segment::segment_frame_cap(decoded_cap)
            )));
        }
        // The manifest frame (a few dozen bytes) is prepended later; reserve its ceiling.
        if payload.len() + frame.bytes.len() + 5 + segment::MAX_MANIFEST_BODY > max_compressed {
            return Err(BaselineError::TooLarge {
                bytes: payload.len() + frame.bytes.len(),
                cap: max_compressed,
            });
        }
        payload.extend_from_slice(&frame.bytes);
        raw_hashes.push(frame.raw_hash);
        *segments += 1;
        let next = cur.index + 1;
        *cur = BaselineSegment {
            index: next,
            volumes: Vec::new(),
        };
        *cur_cost = 0;
        Ok(())
    };

    for volume in snapshot.volumes {
        volumes_n += 1;
        let header = VolumeHeader {
            cell_size_code: volume.cell_size_code,
            owner: volume.owner,
            bounds: volume.bounds,
        };
        let mut header = Some(header);
        let mut run: Option<SegmentVolume> = None;
        for (ordinal, (coord, snap)) in (0u32..).zip(volume.bricks) {
            let brick = BaselineBrick {
                coord: [coord.x, coord.y, coord.z],
                revision: snap.revision().get(),
                edited: snap.is_edited(),
                cells: cells_of(&snap),
            };
            drop(snap);
            let cost = segment::brick_decoded_cost(&brick.cells);
            if cost > decoded_cap {
                return Err(BaselineError::Segmented(format!(
                    "one brick costs {cost} decoded bytes, over the {decoded_cap} budget"
                )));
            }
            if cur_cost + cost > decoded_cap {
                if let Some(mut r) = run.take() {
                    r.last = false;
                    cur.volumes.push(r);
                }
                flush(
                    &mut cur,
                    &mut cur_cost,
                    &mut payload,
                    &mut raw_hashes,
                    &mut segments,
                )?;
            }
            let r = run.get_or_insert_with(|| SegmentVolume {
                volume_id: volume.volume_id,
                header: header.take(),
                first_ordinal: ordinal,
                bricks: Vec::new(),
                last: false,
            });
            r.bricks.push(brick);
            cur_cost += cost;
            decoded_total += cost as u64;
            max_segment_decoded = max_segment_decoded.max(cur_cost);
            bricks_n += 1;
        }
        let Some(mut r) = run.take() else {
            return Err(BaselineError::Segmented(format!(
                "volume {} has no bricks",
                volume.volume_id
            )));
        };
        r.last = true;
        cur.volumes.push(r);
    }
    flush(
        &mut cur,
        &mut cur_cost,
        &mut payload,
        &mut raw_hashes,
        &mut segments,
    )?;

    let manifest = SegmentManifest {
        schema: segment::SEGMENT_SCHEMA,
        segment_count: segments,
        volume_count: volumes_n,
        total_bricks: bricks_n,
        total_decoded_bytes: decoded_total,
        segment_decoded_cap: decoded_cap as u32,
    };
    manifest.validate().map_err(seg_err)?;
    let manifest_frame = segment::encode_manifest_frame(&manifest);
    let assembled_hash = segment::chain_hash(&manifest_frame.bytes[5..], &raw_hashes);
    let mut framed = manifest_frame.bytes;
    framed.extend_from_slice(&payload);
    drop(payload);

    if framed.len() > max_compressed {
        return Err(BaselineError::TooLarge {
            bytes: framed.len(),
            cap: max_compressed,
        });
    }
    let parts: Arc<[BaselinePart]> = chunk_payload(&framed, transfer_id).into();
    if parts.len() > limits::MAX_BASELINE_PARTS {
        return Err(BaselineError::TooManyParts {
            parts: parts.len(),
            cap: limits::MAX_BASELINE_PARTS,
        });
    }
    let begin = BaselineBegin {
        transfer_id,
        interest_epoch,
        checkpoint_tick: Tick(checkpoint_tick),
        journal_cursor,
        world_version: segment::BASELINE_SEGMENTED_WORLD_VERSION,
        content_version: BASELINE_CONTENT_VERSION,
        total_bytes: framed.len() as u64,
        part_count: parts.len() as u32,
        // The per-volume region list does not scale (a world can hold more volumes than the
        // 8,192-region / 64 KiB control-record limits allow); the manifest frame carries the
        // volume and brick counts instead and nothing reads `regions` on the client.
        regions: Vec::new(),
    };
    let end = BaselineEnd {
        transfer_id,
        assembled_hash,
        journal_cursor,
    };
    Ok(BaselineTransfer {
        begin,
        parts,
        end,
        segments: Some(SegmentedStats {
            segments,
            volumes: volumes_n,
            bricks: bricks_n,
            decoded_bytes: decoded_total,
            segment_cap: decoded_cap as u32,
            max_segment_decoded: max_segment_decoded as u32,
        }),
    })
}

/// Splits `payload` into ordered, hashed [`BaselinePart`]s no larger than
/// [`limits::MAX_BULK_PART`]. Reassembly is plain concatenation in `part_index`
/// order, which [`spall_net`]'s bulk reader already enforces.
pub fn chunk_payload(payload: &[u8], transfer_id: TransferId) -> Vec<BaselinePart> {
    let mut parts = Vec::new();
    for (i, chunk) in payload.chunks(limits::MAX_BULK_PART).enumerate() {
        parts.push(BaselinePart {
            transfer_id,
            part_index: i as u32,
            part_hash: Hash32::of(chunk),
            payload: chunk.to_vec(),
        });
    }
    parts
}

/// Reassembles, decompresses, and decodes a received part list (client side
/// helper; also used by the server-side round-trip test).
pub fn assemble(
    parts: &[BaselinePart],
) -> Result<BaselineWorld, spall_protocol::BaselineDecodeError> {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&part.payload);
    }
    BaselineWorld::decode_compressed(&bytes)
}

/// A targeted baseline patch for one diverged brick — the authoritative answer
/// to a brick [`RepairRequest`]. Carries the brick's real revision + material
/// layer so the replica restores exact parity (revision included), which a
/// `CellRun` replay cannot (`docs/protocol.md`: "hash repairs"). `None` for a
/// body repair or a brick the world does not hold.
pub fn brick_repair_patch(sim: &Simulation, request: &RepairRequest) -> Option<BaselineWorld> {
    logical_brick_repair_patch(sim, request, None)
}

/// [`brick_repair_patch`] that can also patch a brick this server has evicted,
/// pulling its cells from `backing`.
pub fn logical_brick_repair_patch(
    sim: &Simulation,
    request: &RepairRequest,
    backing: Option<&dyn BrickBacking>,
) -> Option<BaselineWorld> {
    let RepairKey::Brick { volume, coord } = request.key else {
        return None;
    };
    let world = sim.world();
    let vol = world.volume_ref(volume)?;
    let owner = match world.volume_body(volume)?.entity {
        Some(entity) => BaselineOwner::Body(entity),
        None => BaselineOwner::Terrain,
    };
    let brick = if let Ok(Some(snap)) = vol.snapshot_brick(coord) {
        BaselineBrick {
            coord: [coord.x, coord.y, coord.z],
            revision: snap.revision().get(),
            edited: snap.is_edited(),
            cells: cells_of(&snap),
        }
    } else if world.evicted(volume).contains(coord) {
        let brick = match verified_backing_brick(world.evicted(volume), volume, coord, backing?) {
            Ok(b) => b,
            Err(_) => return None,
        };
        BaselineBrick {
            coord: [coord.x, coord.y, coord.z],
            revision: brick.revision().get(),
            edited: brick.is_edited(),
            cells: cells_of(&brick.snapshot()),
        }
    } else {
        return None;
    };
    let bv = BaselineVolume {
        volume_id: volume,
        cell_size_code: vol.cell_size().to_u8(),
        owner,
        bounds: vol
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
        bricks: vec![brick],
    };
    Some(BaselineWorld {
        schema: spall_protocol::BASELINE_WORLD_SCHEMA,
        checkpoint_tick: sim.current_tick().get(),
        volumes: vec![bv],
    })
}

fn baseline_volume(
    body: &Body,
    owner: BaselineOwner,
    evicted: &EvictedBricks,
    backing: Option<&dyn BrickBacking>,
) -> BaselineVolume {
    let v = &body.volume;
    let mut bricks: Vec<BaselineBrick> = spall_voxel::logical_bricks(v, evicted)
        .expect("logical volume: resident/evicted digest invariant holds")
        .into_iter()
        .map(|lb| {
            let coord = lb.coord;
            if let Ok(Some(snap)) = v.snapshot_brick(coord) {
                return BaselineBrick {
                    coord: [coord.x, coord.y, coord.z],
                    revision: snap.revision().get(),
                    edited: snap.is_edited(),
                    cells: cells_of(&snap),
                };
            }
            // Evicted: its cells come from the durable backing.
            let backing =
                backing.expect("a baseline over evicted geometry needs a durable backing");
            let brick =
                verified_backing_brick(evicted, v.id(), coord, backing).unwrap_or_else(|error| {
                    panic!(
                        "baseline: durable brick {coord:?} of volume {} {error}",
                        v.id()
                    )
                });
            BaselineBrick {
                coord: [coord.x, coord.y, coord.z],
                revision: brick.revision().get(),
                edited: brick.is_edited(),
                cells: cells_of(&brick.snapshot()),
            }
        })
        .collect();
    bricks.sort_by_key(|b| (b.coord[2], b.coord[1], b.coord[0]));
    BaselineVolume {
        volume_id: v.id(),
        cell_size_code: v.cell_size().to_u8(),
        owner,
        bounds: v
            .bounds()
            .map(|b| [[b.min.x, b.min.y, b.min.z], [b.max.x, b.max.y, b.max.z]]),
        bricks,
    }
}

#[derive(Debug)]
enum VerifiedBackingError {
    Unavailable,
    Digest(DigestError),
}

impl std::fmt::Display for VerifiedBackingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("is unavailable"),
            Self::Digest(error) => write!(f, "does not match its retained digest: {error}"),
        }
    }
}

fn verified_backing_brick(
    evicted: &EvictedBricks,
    volume: VolumeId,
    coord: BrickCoord,
    backing: &dyn BrickBacking,
) -> Result<Brick, VerifiedBackingError> {
    let brick = match backing.load(volume, coord) {
        spall_sim::BackingBrick::Loaded(brick) => brick,
        spall_sim::BackingBrick::KnownEmpty { revision, edited } => {
            let air = vec![spall_core::MaterialId::AIR; CELLS_PER_BRICK];
            Brick::restored(&air, revision, edited)
        }
        spall_sim::BackingBrick::Unavailable => return Err(VerifiedBackingError::Unavailable),
    };
    evicted
        .verify_candidate(coord, &brick)
        .map_err(VerifiedBackingError::Digest)
        .map(|()| brick)
}

fn cells_of(snap: &BrickSnapshot) -> BaselineCells {
    let first = snap
        .get(LocalCell::from_linear_index(0).expect("0 < 32768"))
        .raw();
    let mut uniform = true;
    let mut dense = vec![0u16; CELLS_PER_BRICK];
    for (i, slot) in dense.iter_mut().enumerate() {
        let local = LocalCell::from_linear_index(i as u16).expect("i < CELLS_PER_BRICK");
        let raw = snap.get(local).raw();
        *slot = raw;
        if raw != first {
            uniform = false;
        }
    }
    if uniform {
        BaselineCells::Uniform(first)
    } else {
        BaselineCells::Dense(dense)
    }
}

fn region_of(v: &BaselineVolume) -> Option<BaselineRegion> {
    let first = v.bricks.first()?;
    let mut min = first.coord;
    let mut max = first.coord;
    let mut revision = first.revision;
    for b in &v.bricks {
        for axis in 0..3 {
            min[axis] = min[axis].min(b.coord[axis]);
            max[axis] = max[axis].max(b.coord[axis]);
        }
        revision = revision.max(b.revision);
    }
    Some(BaselineRegion {
        volume: v.volume_id,
        min_brick: BrickCoord::new(min[0], min[1], min[2]),
        max_brick: BrickCoord::new(max[0], max[1], max[2]),
        revision: Revision(revision),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::units::{BRUSH_UNIT, BrushPoint};
    use spall_core::{EntityId, SphereBrush};
    use spall_sim::{EditIntent, EditTarget, SimulationConfig, fixtures};

    fn bridge_after_cut() -> Simulation {
        let mut sim = Simulation::new(SimulationConfig::new(fixtures::bridged_terrain_setup()))
            .expect("bridge scene is valid");
        let h = BRUSH_UNIT / 2;
        let brush = SphereBrush::new(
            BrushPoint::from_units(10 * BRUSH_UNIT + h, 4 * BRUSH_UNIT + h, BRUSH_UNIT + h),
            2 * BRUSH_UNIT,
        )
        .expect("brush is valid");
        sim.submit(EditIntent::cut(
            spall_protocol::RequestId(1),
            EntityId::new(1).unwrap(),
            EditTarget::Terrain,
            brush,
        ))
        .expect("submit");
        sim.run_until_idle(24).expect("run to idle");
        sim
    }

    #[test]
    fn a_capture_round_trips_through_parts_and_matches_the_live_world() {
        let sim = bridge_after_cut();
        assert!(sim.world().body_count() >= 1, "the cut detached the beam");

        let transfer =
            capture_transfer(&sim, TransferId(1), InterestEpoch(1), JournalSeq(3)).unwrap();

        // Markers are consistent.
        assert_eq!(transfer.begin.part_count as usize, transfer.parts.len());
        assert_eq!(
            transfer.begin.total_bytes as usize,
            transfer.payload_bytes()
        );
        assert_eq!(transfer.begin.journal_cursor, JournalSeq(3));
        assert_eq!(
            transfer.begin.checkpoint_tick.get(),
            sim.current_tick().get()
        );
        // One region per volume (terrain + every body).
        assert_eq!(transfer.begin.regions.len(), sim.world().body_count() + 1);

        // Reassembly reproduces the captured world exactly.
        let rebuilt = assemble(&transfer.parts).unwrap();
        assert_eq!(rebuilt.volumes.len(), sim.world().body_count() + 1);
        assert_eq!(Hash32::of(&rebuilt.encode()), transfer.end.assembled_hash);
    }

    #[test]
    fn immutable_snapshot_encodes_the_same_baseline_as_the_live_tick() {
        let sim = spall_sim::Simulation::new(spall_sim::SimulationConfig::new(
            spall_sim::fixtures::bridged_terrain_setup(),
        ))
        .unwrap();
        let live = capture_transfer(&sim, TransferId(11), InterestEpoch(1), JournalSeq(0)).unwrap();
        let detached =
            transfer_from_snapshot(snapshot_world(&sim, None), TransferId(12), InterestEpoch(1))
                .unwrap();
        assert_eq!(
            live.decode_v1_world().unwrap(),
            detached.decode_v1_world().unwrap()
        );
        assert_eq!(live.begin.journal_cursor, detached.begin.journal_cursor);
    }

    /// T23 / G3 row 11: the wire payload a late-join transfer actually ships
    /// (`transfer.payload_bytes()`, what the join-budget measurement reports as
    /// "compressed baseline size") must genuinely be smaller than the raw
    /// postcard encoding, not merely carry the label — `docs/protocol.md`
    /// requires "compressed, hashed baseline parts on bulk streams".
    #[test]
    fn a_capture_of_a_realistic_resident_world_is_meaningfully_compressed_over_the_wire() {
        let sim = Simulation::new(SimulationConfig::new(fixtures::separated_regions_setup()))
            .expect("separated-regions scene is valid");
        let world = world_baseline(&sim);
        let raw = world.encode();

        let transfer =
            capture_transfer(&sim, TransferId(1), InterestEpoch(1), JournalSeq(0)).unwrap();
        assert!(
            transfer.payload_bytes() < raw.len(),
            "compressed {} bytes should be smaller than raw {} bytes for a realistic \
             resident-terrain baseline",
            transfer.payload_bytes(),
            raw.len()
        );

        // Reassembly (decompress + decode) still recovers the exact world.
        let rebuilt = assemble(&transfer.parts).unwrap();
        assert_eq!(rebuilt, world);
    }

    #[test]
    fn every_baseline_brick_encoding_is_well_formed_and_decodes_equal() {
        let sim = bridge_after_cut();
        let world = world_baseline(&sim);
        for v in &world.volumes {
            assert!(!v.bricks.is_empty());
            for b in &v.bricks {
                match &b.cells {
                    BaselineCells::Uniform(_) => {}
                    BaselineCells::Dense(cells) => assert_eq!(cells.len(), CELLS_PER_BRICK),
                }
            }
        }
        let decoded = BaselineWorld::decode(&world.encode()).unwrap();
        assert_eq!(decoded, world);
    }
}
