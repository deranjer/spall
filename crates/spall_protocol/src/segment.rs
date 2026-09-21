//! Segmented baseline payloads (`BaselineBegin.world_version == 2`).
//!
//! A baseline is still one transfer (one `BaselineBegin`, one bulk stream of
//! [`BaselinePart`](crate::BaselinePart)s, one `BaselineEnd`), but its payload is a
//! sequence of independently bounded frames instead of one blob:
//!
//! ```text
//! frame    := kind:u8  len:u32-LE  body[len]
//! manifest := kind 0, postcard(SegmentManifest)                       (always first)
//! segment  := kind 1, raw_hash:[u8;32] ++ zstd(postcard(BaselineSegment))
//! ```
//!
//! The unit of allocation, hashing, validation and staging is the **segment**. Its budget is stated
//! in *decoded* bytes ([`DENSE_BRICK_DECODED_COST`] per dense brick), not serialized bytes: one
//! dense brick is 64 KiB decoded but 32-96 KiB as postcard.
//!
//! Decompression keeps the existing protection: output is read through `take(cap + 1)` with a
//! per-segment cap, so a compression bomb can never allocate past it.
//! See `docs/reports/large-world-baseline-design.md`.

use serde::{Deserialize, Serialize};
use spall_core::{CELLS_PER_BRICK, VolumeId};

use crate::baseline::{BaselineBrick, BaselineCells, BaselineOwner};
use crate::canonical::Hash32;

/// `BaselineBegin.world_version` of a segmented transfer (`1` is the single blob).
pub const BASELINE_SEGMENTED_WORLD_VERSION: u32 = 2;
/// Schema of [`SegmentManifest`] / [`BaselineSegment`].
pub const SEGMENT_SCHEMA: u16 = 2;
/// Decoded cost of a dense brick: `Vec<u16>` cells plus bookkeeping.
pub const DENSE_BRICK_DECODED_COST: usize = CELLS_PER_BRICK * 2 + 64;
/// Decoded cost of a uniform brick.
pub const UNIFORM_BRICK_DECODED_COST: usize = 64;
/// Default per-segment decoded budget.
pub const DEFAULT_SEGMENT_DECODED_BYTES: usize = 4 * 1024 * 1024;
/// The largest per-segment decoded budget any sender may declare.
pub const MAX_SEGMENT_DECODED_BYTES: usize = 32 * 1024 * 1024;
/// Most segments in one transfer.
pub const MAX_BASELINE_SEGMENTS: usize = 16 * 1024;
/// Sanity ceiling on a manifest's declared total decoded bytes.
pub const MAX_SEGMENTED_TOTAL_DECODED: u64 = 16 * 1024 * 1024 * 1024;
/// Most volumes a segmented transfer may declare.
pub const MAX_SEGMENTED_VOLUMES: u32 = 1 << 20;
/// Manifest frames are tiny.
pub const MAX_MANIFEST_BODY: usize = 4 * 1024;

/// A client advertises segmented-baseline support by sending this as the (otherwise unused)
/// `verified_manifest_hash` of its sentinel baseline request. Old servers ignore it.
pub fn baseline_cap_segmented() -> Hash32 {
    Hash32::of(b"spall.baseline.capability.segmented.v2")
}

/// Postcard-raw cap for a segment whose decoded budget is `decoded_cap`: a dense brick is at most
/// three varint bytes per cell (1.5x its decoded size).
pub fn segment_raw_cap(decoded_cap: usize) -> usize {
    2 * decoded_cap + 64 * 1024
}

/// Largest frame body for a segment budget: 32 bytes of hash plus a zstd frame that, being
/// incompressible at worst, does not exceed the raw cap by more than a small overhead.
pub fn segment_frame_cap(decoded_cap: usize) -> usize {
    32 + segment_raw_cap(decoded_cap) + 1024
}

/// Why a segmented payload was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    #[error("baseline segment budget {0} bytes is below the cost of one dense brick")]
    BudgetTooSmall(usize),
    #[error("one brick costs {cost} decoded bytes but the segment budget is {cap}")]
    BrickOverBudget { cost: usize, cap: usize },
    #[error("unknown frame kind {0}")]
    UnknownFrameKind(u8),
    #[error("frame body of {len} bytes exceeds the {cap} byte limit")]
    FrameTooLarge { len: usize, cap: usize },
    #[error("first frame is not the manifest")]
    ManifestNotFirst,
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("segment frame is shorter than its hash")]
    ShortSegmentFrame,
    #[error("zstd: {0}")]
    Zstd(String),
    #[error("segment decompresses past its {cap} byte raw cap")]
    RawTooLarge { cap: usize },
    #[error("segment hash mismatch")]
    HashMismatch,
    #[error("postcard: {0}")]
    Postcard(String),
    #[error("segment {index}: {reason}")]
    Sequence { index: u32, reason: String },
    #[error("baseline incomplete: {0}")]
    Incomplete(String),
}

/// First frame of a segmented payload; validated before any segment is decoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub schema: u16,
    pub segment_count: u32,
    pub volume_count: u32,
    pub total_bricks: u64,
    /// Sum of the decoded cost of every brick: what the receiver's staging must budget.
    pub total_decoded_bytes: u64,
    /// The sender's per-segment decoded budget; the receiver enforces it on every segment.
    pub segment_decoded_cap: u32,
}

impl SegmentManifest {
    /// Structural checks that need no other context.
    pub fn validate(&self) -> Result<(), SegmentError> {
        let bad = |m: &str| Err(SegmentError::Manifest(m.to_string()));
        if self.schema != SEGMENT_SCHEMA {
            return bad("unsupported segment schema");
        }
        if self.segment_count == 0 || self.segment_count as usize > MAX_BASELINE_SEGMENTS {
            return bad("segment count out of range");
        }
        if self.volume_count == 0 || self.volume_count > MAX_SEGMENTED_VOLUMES {
            return bad("volume count out of range");
        }
        let cap = self.segment_decoded_cap as usize;
        if !(DENSE_BRICK_DECODED_COST..=MAX_SEGMENT_DECODED_BYTES).contains(&cap) {
            return bad("segment budget out of range");
        }
        if self.total_decoded_bytes > MAX_SEGMENTED_TOTAL_DECODED {
            return bad("total decoded bytes over the ceiling");
        }
        if self.total_bricks < u64::from(self.volume_count) {
            return bad("fewer bricks than volumes");
        }
        // Every segment holds at most `cap` decoded bytes.
        if self.total_decoded_bytes
            > u64::from(self.segment_count) * u64::from(self.segment_decoded_cap)
        {
            return bad("declared bytes exceed segment_count x budget");
        }
        Ok(())
    }
}

/// Everything about a volume except its bricks; rides in the volume's first segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeHeader {
    pub cell_size_code: u8,
    pub owner: BaselineOwner,
    pub bounds: Option<[[i64; 3]; 2]>,
}

/// A run of one volume's bricks. `first_ordinal` is the index (in canonical `(z, y, x)` order) of
/// `bricks[0]` within the volume; `last` closes the volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentVolume {
    pub volume_id: VolumeId,
    pub header: Option<VolumeHeader>,
    pub first_ordinal: u32,
    pub bricks: Vec<BaselineBrick>,
    pub last: bool,
}

/// One independently bounded piece of a baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineSegment {
    pub index: u32,
    pub volumes: Vec<SegmentVolume>,
}

/// Decoded cost of one brick's cells.
pub fn brick_decoded_cost(cells: &BaselineCells) -> usize {
    match cells {
        BaselineCells::Dense(_) => DENSE_BRICK_DECODED_COST,
        BaselineCells::Uniform(_) => UNIFORM_BRICK_DECODED_COST,
    }
}

impl BaselineSegment {
    /// Sum of the decoded cost of every brick.
    pub fn decoded_cost(&self) -> usize {
        self.volumes
            .iter()
            .flat_map(|v| &v.bricks)
            .map(|b| brick_decoded_cost(&b.cells))
            .sum()
    }
}

/// An encoded segment frame and what the sender needs to know about it.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// `kind ++ len ++ body`.
    pub bytes: Vec<u8>,
    /// BLAKE3 of the raw postcard (segments only; zero for the manifest).
    pub raw_hash: Hash32,
    pub raw_len: usize,
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Encodes the manifest frame (postcard, uncompressed).
pub fn encode_manifest_frame(m: &SegmentManifest) -> EncodedFrame {
    let body = postcard::to_stdvec(m).expect("manifest serializes");
    EncodedFrame {
        bytes: frame(0, &body),
        raw_hash: Hash32::ZERO,
        raw_len: body.len(),
    }
}

/// Encodes one segment frame. Fails if the segment exceeds the decoded `cap`.
pub fn encode_segment_frame(
    segment: &BaselineSegment,
    decoded_cap: usize,
) -> Result<EncodedFrame, SegmentError> {
    let cost = segment.decoded_cost();
    if cost > decoded_cap {
        return Err(SegmentError::BrickOverBudget {
            cost,
            cap: decoded_cap,
        });
    }
    let raw = postcard::to_stdvec(segment).expect("segment serializes");
    let raw_hash = Hash32::of(&raw);
    let compressed = zstd::stream::encode_all(raw.as_slice(), 0)
        .map_err(|e| SegmentError::Zstd(e.to_string()))?;
    let mut body = Vec::with_capacity(32 + compressed.len());
    body.extend_from_slice(&raw_hash.0);
    body.extend_from_slice(&compressed);
    Ok(EncodedFrame {
        bytes: frame(1, &body),
        raw_hash,
        raw_len: raw.len(),
    })
}

/// A received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Manifest(Vec<u8>),
    Segment(Vec<u8>),
}

/// Reassembles frames from the ordered bulk parts. Holds at most one frame; the length prefix is
/// checked against `max_body` **before** the body is buffered.
#[derive(Debug)]
pub struct FrameReader {
    buf: Vec<u8>,
    max_body: usize,
}

impl FrameReader {
    /// Starts limited to a manifest-sized body; call [`Self::set_max_body`] after the manifest.
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            max_body: MAX_MANIFEST_BODY,
        }
    }

    pub fn set_max_body(&mut self, max_body: usize) {
        self.max_body = max_body;
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered awaiting a complete frame.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame, `Ok(None)` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, SegmentError> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let kind = self.buf[0];
        if kind > 1 {
            return Err(SegmentError::UnknownFrameKind(kind));
        }
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        let cap = if kind == 0 {
            MAX_MANIFEST_BODY
        } else {
            self.max_body
        };
        if len > cap {
            return Err(SegmentError::FrameTooLarge { len, cap });
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let body = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some(if kind == 0 {
            Frame::Manifest(body)
        } else {
            Frame::Segment(body)
        }))
    }

    /// `true` when nothing is buffered (a clean end of payload).
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes and validates a manifest body.
pub fn decode_manifest(body: &[u8]) -> Result<SegmentManifest, SegmentError> {
    let m: SegmentManifest =
        postcard::from_bytes(body).map_err(|e| SegmentError::Manifest(e.to_string()))?;
    m.validate()?;
    Ok(m)
}

/// Decodes one segment frame body under the manifest's bounds: hash, decompression cap, structure
/// and decoded cost. Returns the segment and its raw hash.
pub fn decode_segment(
    body: &[u8],
    manifest: &SegmentManifest,
) -> Result<(BaselineSegment, Hash32), SegmentError> {
    use std::io::Read;
    let cap = manifest.segment_decoded_cap as usize;
    if body.len() < 32 {
        return Err(SegmentError::ShortSegmentFrame);
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&body[..32]);
    let declared = Hash32(hash);
    let raw_cap = segment_raw_cap(cap);
    let decoder =
        zstd::stream::Decoder::new(&body[32..]).map_err(|e| SegmentError::Zstd(e.to_string()))?;
    let mut raw = Vec::new();
    decoder
        .take(raw_cap as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| SegmentError::Zstd(e.to_string()))?;
    if raw.len() > raw_cap {
        return Err(SegmentError::RawTooLarge { cap: raw_cap });
    }
    if Hash32::of(&raw) != declared {
        return Err(SegmentError::HashMismatch);
    }
    let segment: BaselineSegment =
        postcard::from_bytes(&raw).map_err(|e| SegmentError::Postcard(e.to_string()))?;
    for v in &segment.volumes {
        for b in &v.bricks {
            if let BaselineCells::Dense(c) = &b.cells
                && c.len() != CELLS_PER_BRICK
            {
                return Err(SegmentError::Sequence {
                    index: segment.index,
                    reason: format!("dense brick with {} cells", c.len()),
                });
            }
        }
    }
    let cost = segment.decoded_cost();
    if cost > cap {
        return Err(SegmentError::Sequence {
            index: segment.index,
            reason: format!("decoded cost {cost} exceeds the manifest budget {cap}"),
        });
    }
    Ok((segment, declared))
}

/// `BaselineEnd.assembled_hash` of a segmented transfer: BLAKE3 over the manifest body followed by
/// every segment's raw hash in order.
pub fn chain_hash(manifest_body: &[u8], raw_hashes: &[Hash32]) -> Hash32 {
    let mut h = blake3::Hasher::new();
    h.update(manifest_body);
    for r in raw_hashes {
        h.update(&r.0);
    }
    Hash32(*h.finalize().as_bytes())
}

/// Tracks a segmented transfer's structure as segments arrive and decides, segment by segment and
/// at the end, whether it is complete and consistent. Pure bookkeeping: it builds nothing.
#[derive(Debug)]
pub struct SequenceValidator {
    manifest: SegmentManifest,
    next_index: u32,
    /// Volumes in ascending id order: the highest id opened so far.
    last_volume: Option<u64>,
    /// The volume currently open: (id, next expected ordinal).
    open: Option<(u64, u32)>,
    volumes_closed: u32,
    bricks: u64,
    decoded: u64,
    terrain_seen: bool,
}

impl SequenceValidator {
    pub fn new(manifest: SegmentManifest) -> Self {
        Self {
            manifest,
            next_index: 0,
            last_volume: None,
            open: None,
            volumes_closed: 0,
            bricks: 0,
            decoded: 0,
            terrain_seen: false,
        }
    }

    pub fn manifest(&self) -> &SegmentManifest {
        &self.manifest
    }

    pub fn segments_seen(&self) -> u32 {
        self.next_index
    }

    pub fn decoded_bytes(&self) -> u64 {
        self.decoded
    }

    /// Checks `segment` against everything accepted so far.
    pub fn accept(&mut self, segment: &BaselineSegment) -> Result<(), SegmentError> {
        let index = segment.index;
        let fail = |reason: String| Err(SegmentError::Sequence { index, reason });
        if index != self.next_index {
            return fail(format!(
                "expected segment {} (missing, duplicate or reordered)",
                self.next_index
            ));
        }
        if index >= self.manifest.segment_count {
            return fail("more segments than the manifest declared".to_string());
        }
        if segment.volumes.is_empty() {
            return fail("empty segment".to_string());
        }
        for v in &segment.volumes {
            let vid = v.volume_id.get();
            if v.bricks.is_empty() {
                return fail(format!("volume {vid} run has no bricks"));
            }
            match self.open {
                Some((open_id, next_ordinal)) => {
                    if vid != open_id {
                        return fail(format!(
                            "volume {open_id} was not closed before volume {vid}"
                        ));
                    }
                    if v.header.is_some() {
                        return fail(format!("volume {vid} repeats its header"));
                    }
                    if v.first_ordinal != next_ordinal {
                        return fail(format!(
                            "volume {vid} range starts at ordinal {} but {next_ordinal} was expected (gap, overlap or reorder)",
                            v.first_ordinal
                        ));
                    }
                }
                None => {
                    if self.last_volume.is_some_and(|l| vid <= l) {
                        return fail(format!("volume {vid} is duplicated or out of order"));
                    }
                    let Some(header) = &v.header else {
                        return fail(format!("volume {vid} has no header in its first range"));
                    };
                    if v.first_ordinal != 0 {
                        return fail(format!("volume {vid} does not start at ordinal 0"));
                    }
                    if header.owner == BaselineOwner::Terrain {
                        if self.terrain_seen {
                            return fail("a second terrain volume".to_string());
                        }
                        self.terrain_seen = true;
                    }
                    self.last_volume = Some(vid);
                }
            }
            let next = v.first_ordinal as u64 + v.bricks.len() as u64;
            if next > u64::from(u32::MAX) {
                return fail(format!("volume {vid} ordinal overflow"));
            }
            self.bricks += v.bricks.len() as u64;
            if v.last {
                self.open = None;
                self.volumes_closed += 1;
            } else {
                self.open = Some((vid, next as u32));
            }
        }
        self.decoded += segment.decoded_cost() as u64;
        if self.decoded > self.manifest.total_decoded_bytes
            || self.bricks > self.manifest.total_bricks
        {
            return fail("more data than the manifest declared".to_string());
        }
        self.next_index += 1;
        Ok(())
    }

    /// The transfer is complete only if every declared segment, volume, brick and byte arrived and
    /// no volume was left open.
    pub fn finish(&self) -> Result<(), SegmentError> {
        let inc = |m: String| Err(SegmentError::Incomplete(m));
        if self.next_index != self.manifest.segment_count {
            return inc(format!(
                "{} of {} segments arrived",
                self.next_index, self.manifest.segment_count
            ));
        }
        if let Some((vid, _)) = self.open {
            return inc(format!("volume {vid} was left open"));
        }
        if self.volumes_closed != self.manifest.volume_count {
            return inc(format!(
                "{} of {} volumes closed",
                self.volumes_closed, self.manifest.volume_count
            ));
        }
        if self.bricks != self.manifest.total_bricks {
            return inc(format!(
                "{} of {} bricks arrived",
                self.bricks, self.manifest.total_bricks
            ));
        }
        if self.decoded != self.manifest.total_decoded_bytes {
            return inc(format!(
                "{} of {} decoded bytes arrived",
                self.decoded, self.manifest.total_decoded_bytes
            ));
        }
        if !self.terrain_seen {
            return inc("no terrain volume".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brick(i: i64, dense: bool) -> BaselineBrick {
        BaselineBrick {
            coord: [i, 0, 0],
            revision: 1,
            edited: false,
            cells: if dense {
                BaselineCells::Dense(vec![(i % 3) as u16; CELLS_PER_BRICK])
            } else {
                BaselineCells::Uniform(1)
            },
        }
    }

    fn header(owner: BaselineOwner) -> Option<VolumeHeader> {
        Some(VolumeHeader {
            cell_size_code: 2,
            owner,
            bounds: None,
        })
    }

    fn vid(n: u64) -> VolumeId {
        VolumeId::new(n).unwrap()
    }

    fn manifest(
        segments: u32,
        volumes: u32,
        bricks: u64,
        decoded: u64,
        cap: usize,
    ) -> SegmentManifest {
        SegmentManifest {
            schema: SEGMENT_SCHEMA,
            segment_count: segments,
            volume_count: volumes,
            total_bricks: bricks,
            total_decoded_bytes: decoded,
            segment_decoded_cap: cap as u32,
        }
    }

    /// terrain (volume 1) split across two segments, then a one-brick body.
    fn fixture() -> (SegmentManifest, Vec<BaselineSegment>) {
        let cap = 3 * DENSE_BRICK_DECODED_COST;
        let s0 = BaselineSegment {
            index: 0,
            volumes: vec![SegmentVolume {
                volume_id: vid(1),
                header: header(BaselineOwner::Terrain),
                first_ordinal: 0,
                bricks: vec![brick(0, true), brick(1, true)],
                last: false,
            }],
        };
        let s1 = BaselineSegment {
            index: 1,
            volumes: vec![
                SegmentVolume {
                    volume_id: vid(1),
                    header: None,
                    first_ordinal: 2,
                    bricks: vec![brick(2, true)],
                    last: true,
                },
                SegmentVolume {
                    volume_id: vid(2),
                    header: header(BaselineOwner::Body(spall_core::EntityId::new(9).unwrap())),
                    first_ordinal: 0,
                    bricks: vec![brick(0, false)],
                    last: true,
                },
            ],
        };
        let decoded = (s0.decoded_cost() + s1.decoded_cost()) as u64;
        (manifest(2, 2, 4, decoded, cap), vec![s0, s1])
    }

    fn accept_all(m: &SegmentManifest, segs: &[BaselineSegment]) -> Result<(), SegmentError> {
        let mut v = SequenceValidator::new(m.clone());
        for s in segs {
            v.accept(s)?;
        }
        v.finish()
    }

    #[test]
    fn a_complete_oversized_volume_passes() {
        let (m, segs) = fixture();
        accept_all(&m, &segs).unwrap();
    }

    #[test]
    fn missing_duplicate_and_reordered_segments_are_refused() {
        let (m, segs) = fixture();
        assert!(accept_all(&m, &segs[1..]).is_err(), "missing first");
        let mut dup = vec![segs[0].clone(), segs[0].clone()];
        dup[1].index = 1;
        assert!(accept_all(&m, &dup).is_err(), "duplicate segment");
        let mut swapped = segs.clone();
        swapped.swap(0, 1);
        assert!(accept_all(&m, &swapped).is_err(), "reordered");
        // Truncated: the second segment never arrives.
        let mut v = SequenceValidator::new(m.clone());
        v.accept(&segs[0]).unwrap();
        assert!(matches!(v.finish(), Err(SegmentError::Incomplete(_))));
    }

    #[test]
    fn a_brick_range_gap_overlap_or_reorder_is_refused() {
        let (m, mut segs) = fixture();
        segs[1].volumes[0].first_ordinal = 3; // gap
        assert!(
            accept_all(&m, &segs)
                .unwrap_err()
                .to_string()
                .contains("gap")
        );
        let (m, mut segs) = fixture();
        segs[1].volumes[0].first_ordinal = 1; // overlap
        assert!(accept_all(&m, &segs).is_err());
    }

    #[test]
    fn a_volume_left_open_or_reopened_is_refused() {
        let (m, mut segs) = fixture();
        segs[1].volumes[0].last = false;
        let e = accept_all(&m, &segs).unwrap_err().to_string();
        assert!(
            e.contains("was not closed") || e.contains("left open"),
            "{e}"
        );
        // A header repeated in a continuation.
        let (m, mut segs) = fixture();
        segs[1].volumes[0].header = header(BaselineOwner::Terrain);
        assert!(
            accept_all(&m, &segs)
                .unwrap_err()
                .to_string()
                .contains("repeats its header")
        );
    }

    #[test]
    fn a_manifest_that_lies_is_refused() {
        let (mut m, segs) = fixture();
        m.total_bricks = 5;
        assert!(matches!(
            accept_all(&m, &segs),
            Err(SegmentError::Incomplete(_))
        ));
        let (mut m, segs) = fixture();
        m.total_decoded_bytes -= 1;
        assert!(accept_all(&m, &segs).is_err());
        assert!(
            manifest(0, 1, 1, 0, DENSE_BRICK_DECODED_COST)
                .validate()
                .is_err()
        );
        assert!(
            manifest(1, 1, 1, 10, 10).validate().is_err(),
            "budget below one brick"
        );
    }

    #[test]
    fn frames_round_trip_and_bound_their_input() {
        let (m, segs) = fixture();
        let cap = m.segment_decoded_cap as usize;
        let mut wire = encode_manifest_frame(&m).bytes;
        let mut hashes = Vec::new();
        for s in &segs {
            let f = encode_segment_frame(s, cap).unwrap();
            hashes.push(f.raw_hash);
            wire.extend_from_slice(&f.bytes);
        }
        // Fed in awkward 7-byte pieces: never more than one frame is buffered.
        let mut reader = FrameReader::new();
        let mut out = Vec::new();
        let mut max_buffered = 0;
        for piece in wire.chunks(7) {
            reader.push(piece);
            max_buffered = max_buffered.max(reader.buffered());
            while let Some(f) = reader.next_frame().unwrap() {
                if let Frame::Manifest(_) = f {
                    reader.set_max_body(segment_frame_cap(cap));
                }
                out.push(f);
            }
        }
        assert!(reader.is_empty());
        assert!(max_buffered <= 5 + segment_frame_cap(cap));
        let Frame::Manifest(mb) = &out[0] else {
            panic!()
        };
        let manifest = decode_manifest(mb).unwrap();
        assert_eq!(manifest, m);
        let mut got = Vec::new();
        let mut got_hashes = Vec::new();
        for f in &out[1..] {
            let Frame::Segment(b) = f else { panic!() };
            let (s, h) = decode_segment(b, &manifest).unwrap();
            got.push(s);
            got_hashes.push(h);
        }
        assert_eq!(got, segs);
        assert_eq!(got_hashes, hashes);
        assert_eq!(chain_hash(mb, &got_hashes), chain_hash(mb, &hashes));
    }

    #[test]
    fn corrupt_oversized_and_bomb_frames_are_refused() {
        let (m, segs) = fixture();
        let cap = m.segment_decoded_cap as usize;
        let f = encode_segment_frame(&segs[0], cap).unwrap();
        // Corrupt the declared hash.
        let mut bad = f.bytes[5..].to_vec();
        bad[0] ^= 1;
        assert_eq!(
            decode_segment(&bad, &m).unwrap_err(),
            SegmentError::HashMismatch
        );
        // Corrupt the compressed body.
        let mut bad = f.bytes[5..].to_vec();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert!(decode_segment(&bad, &m).is_err());
        // A length prefix past the cap is refused before buffering.
        let mut reader = FrameReader::new();
        reader.set_max_body(1000);
        reader.push(&[1, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert!(matches!(
            reader.next_frame(),
            Err(SegmentError::FrameTooLarge { .. })
        ));
        // A compression bomb decompresses past the per-segment cap and is refused.
        let raw = vec![0u8; segment_raw_cap(cap) + 10_000];
        let comp = zstd::stream::encode_all(raw.as_slice(), 0).unwrap();
        let mut body = Hash32::of(&raw).0.to_vec();
        body.extend_from_slice(&comp);
        assert_eq!(
            decode_segment(&body, &m).unwrap_err(),
            SegmentError::RawTooLarge {
                cap: segment_raw_cap(cap)
            }
        );
        // An unknown frame kind.
        let mut reader = FrameReader::new();
        reader.push(&[9, 0, 0, 0, 0]);
        assert!(matches!(
            reader.next_frame(),
            Err(SegmentError::UnknownFrameKind(9))
        ));
    }

    #[test]
    fn the_encoder_refuses_a_segment_over_its_decoded_budget() {
        let (m, segs) = fixture();
        let too_small = 2 * DENSE_BRICK_DECODED_COST;
        assert!(matches!(
            encode_segment_frame(&segs[0], too_small - 1),
            Err(SegmentError::BrickOverBudget { .. })
        ));
        let _ = m;
    }
}
