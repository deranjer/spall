//! Receiving a segmented (`BaselineBegin.world_version == 2`) baseline.
//!
//! [`SegmentedReceiver`] is the transport-free core: it is fed the transfer's bulk-part payloads in
//! order, reassembles frames one at a time, decodes each segment under its bounds, checks it against
//! the manifest and folds it into a [`StagedBaseline`]. Nothing is visible to the replica until
//! [`ReplicaWorld::install_staged`](crate::ReplicaWorld::install_staged) is called with the result
//! of [`SegmentedReceiver::finish`]. See `docs/reports/large-world-baseline-design.md`.

use spall_protocol::Hash32;
use spall_protocol::segment::{
    self, Frame, FrameReader, SegmentError, SegmentManifest, SequenceValidator,
};

use crate::replica::StagedBaseline;

/// What the client will let a baseline cost: the existing replica plus the staged world, its
/// installation overhead and the temporary segment buffers, against one explicit budget.
///
/// The existing replica counts because a client that already holds a world keeps it until the
/// atomic swap (old + staged coexist); the overhead factor covers the staged bricks being a little
/// larger than the sum of their decoded costs (measured 769 MiB staged against 754 MiB declared,
/// 1.02x; 1.10x is budgeted); the temporary term is four segment budgets (measured peak is about
/// 0.75 of one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagingAdmission {
    /// Total bytes the client permits itself for holding + building the baseline.
    pub budget_bytes: u64,
    /// Decoded bytes the current replica already holds ([`crate::ReplicaWorld::decoded_bytes_estimate`]).
    pub existing_replica_bytes: u64,
}

/// The default production budget: the client memory target in `docs/validation.md` (4 GiB,
/// excluding driver allocations).
pub const DEFAULT_CLIENT_BASELINE_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

impl StagingAdmission {
    /// Bytes admitting `manifest` would need: existing replica + staged world x 1.10 + four
    /// segment budgets of temporary buffers.
    pub fn required_bytes(&self, manifest: &SegmentManifest) -> u64 {
        self.existing_replica_bytes
            + manifest.total_decoded_bytes / 10 * 11
            + 4 * u64::from(manifest.segment_decoded_cap)
    }

    /// Admits or refuses `manifest` before any segment is decoded.
    pub fn check(&self, manifest: &SegmentManifest) -> Result<(), String> {
        let need = self.required_bytes(manifest);
        if need > self.budget_bytes {
            return Err(format!(
                "baseline needs {need} bytes ({} staged x1.10 + {} existing replica + 4 x {} segment buffers) \
                 but the client budget is {} bytes",
                manifest.total_decoded_bytes,
                self.existing_replica_bytes,
                manifest.segment_decoded_cap,
                self.budget_bytes
            ));
        }
        Ok(())
    }
}

/// A verified, fully staged segmented baseline.
pub struct SegmentedReceipt {
    pub staged: StagedBaseline,
    /// The chained hash `BaselineEnd` must carry (and the client acks with).
    pub chain_hash: Hash32,
    pub segments: u32,
    /// Decoded bytes staged (world-sized).
    pub decoded_bytes: u64,
    /// Largest decoded segment held at once (the bounded temporary).
    pub max_segment_decoded: u64,
    /// Most bytes the frame reassembler ever buffered.
    pub max_buffered_bytes: usize,
}

/// The terrain digests one completed segment carried, and the payload offset at which it ended.
pub type SegmentDigests = (u64, Vec<(spall_core::BrickCoord, spall_voxel::BrickDigest)>);

/// Incremental receiver for one segmented transfer.
pub struct SegmentedReceiver {
    reader: FrameReader,
    manifest_body: Option<Vec<u8>>,
    validator: Option<SequenceValidator>,
    staged: StagedBaseline,
    raw_hashes: Vec<Hash32>,
    admission: Option<StagingAdmission>,
    /// Compressed bytes accepted so far, and the ceilings they are held to.
    received: u64,
    max_compressed: u64,
    max_segment_decoded: u64,
    max_buffered: usize,
    /// When enabled ([`Self::track_digests`]), the digests of every completed segment.
    digest_trail: Option<Vec<SegmentDigests>>,
}

impl SegmentedReceiver {
    pub(crate) fn reuse_volumes(
        &mut self,
        volumes: std::collections::BTreeMap<u64, spall_voxel::Volume>,
    ) {
        self.staged.set_reuse(volumes);
    }
    /// `checkpoint_tick` comes from `BaselineBegin`; `admission` is checked against the manifest
    /// before any segment is decoded. The cumulative compressed bytes are held to the protocol's
    /// [`spall_protocol::limits::MAX_ASSEMBLED_TRANSFER`].
    pub fn new(checkpoint_tick: u64, admission: Option<StagingAdmission>) -> Self {
        Self::with_limits(
            checkpoint_tick,
            admission,
            spall_protocol::limits::MAX_ASSEMBLED_TRANSFER as u64,
        )
    }

    /// Like [`Self::new`] with an explicit ceiling on the cumulative **compressed** bytes (the
    /// smaller of the protocol cap and the `BaselineBegin.total_bytes` the server declared).
    /// Enforced before a payload is buffered, so an over-long or poorly compressible transfer is
    /// refused without growing any buffer past it.
    pub fn with_limits(
        checkpoint_tick: u64,
        admission: Option<StagingAdmission>,
        max_compressed: u64,
    ) -> Self {
        Self {
            reader: FrameReader::new(),
            manifest_body: None,
            validator: None,
            staged: StagedBaseline::new(checkpoint_tick),
            raw_hashes: Vec::new(),
            admission,
            received: 0,
            max_compressed,
            max_segment_decoded: 0,
            max_buffered: 0,
            digest_trail: None,
        }
    }

    /// Starts remembering, for each completed segment, the digests it carried and the payload
    /// offset where it ended, so a transfer that is interrupted can say what it had verified.
    pub fn track_digests(&mut self) {
        self.digest_trail.get_or_insert_with(Vec::new);
    }

    /// The digests of every segment completed so far (empty unless tracking was enabled).
    pub fn digest_trail(&self) -> &[SegmentDigests] {
        self.digest_trail.as_deref().unwrap_or(&[])
    }

    /// The manifest, once received.
    pub fn manifest(&self) -> Option<&SegmentManifest> {
        self.validator.as_ref().map(|v| v.manifest())
    }

    /// Feeds the next part's payload. Returns a reason naming the segment on any failure; the
    /// receiver must then be dropped (the replica has not been touched).
    pub fn push(&mut self, payload: &[u8]) -> Result<(), String> {
        // The cumulative compressed cap is checked *before* the bytes are buffered.
        self.received = self.received.saturating_add(payload.len() as u64);
        if self.received > self.max_compressed {
            return Err(format!(
                "the transfer exceeds its {} byte compressed cap ({} bytes received)",
                self.max_compressed, self.received
            ));
        }
        self.reader.push(payload);
        self.max_buffered = self.max_buffered.max(self.reader.buffered());
        while let Some(frame) = self.reader.next_frame().map_err(|e| e.to_string())? {
            match (frame, self.validator.as_mut()) {
                (Frame::Manifest(body), None) => {
                    let m = segment::decode_manifest(&body).map_err(|e| e.to_string())?;
                    if let Some(adm) = &self.admission {
                        adm.check(&m)?;
                    }
                    self.reader
                        .set_max_body(segment::segment_frame_cap(m.segment_decoded_cap as usize));
                    self.manifest_body = Some(body);
                    self.validator = Some(SequenceValidator::new(m));
                }
                (Frame::Manifest(_), Some(_)) => return Err("a second manifest frame".to_string()),
                (Frame::Segment(_), None) => {
                    return Err(SegmentError::ManifestNotFirst.to_string());
                }
                (Frame::Segment(body), Some(v)) => {
                    let (seg, raw_hash) =
                        segment::decode_segment(&body, v.manifest()).map_err(|e| e.to_string())?;
                    let index = seg.index;
                    v.accept(&seg).map_err(|e| e.to_string())?;
                    self.max_segment_decoded =
                        self.max_segment_decoded.max(seg.decoded_cost() as u64);
                    self.staged
                        .add_segment(&seg)
                        .map_err(|e| format!("segment {index}: {e}"))?;
                    self.raw_hashes.push(raw_hash);
                    if let Some(trail) = self.digest_trail.as_mut() {
                        // Bytes consumed through this frame: everything received so far less
                        // what the reader still holds beyond it.
                        let ended_at = self.received - self.reader.buffered() as u64;
                        trail.push((ended_at, segment_digests(&seg)));
                    }
                }
            }
        }
        Ok(())
    }

    /// The payload has ended. Checks completeness and returns the staged world plus the chained
    /// hash to compare with `BaselineEnd`.
    pub fn finish(self) -> Result<SegmentedReceipt, String> {
        if !self.reader.is_empty() {
            return Err("the payload ended inside a frame".to_string());
        }
        let (Some(v), Some(mb)) = (self.validator, self.manifest_body) else {
            return Err("the transfer carried no manifest".to_string());
        };
        v.finish().map_err(|e| e.to_string())?;
        Ok(SegmentedReceipt {
            chain_hash: segment::chain_hash(&mb, &self.raw_hashes),
            segments: v.segments_seen(),
            decoded_bytes: v.decoded_bytes(),
            staged: self.staged,
            max_segment_decoded: self.max_segment_decoded,
            max_buffered_bytes: self.max_buffered,
        })
    }
}

/// The terrain digests (`BaselineCells::Digest` bricks) a segment carries, as the replica records
/// them.
fn segment_digests(
    seg: &spall_protocol::segment::BaselineSegment,
) -> Vec<(spall_core::BrickCoord, spall_voxel::BrickDigest)> {
    seg.volumes
        .iter()
        .flat_map(|volume| volume.bricks.iter())
        .filter_map(|brick| match brick.cells {
            spall_protocol::BaselineCells::Digest {
                content_hash,
                solid_cells,
            } => Some((
                spall_core::BrickCoord::new(brick.coord[0], brick.coord[1], brick.coord[2]),
                spall_voxel::BrickDigest {
                    revision: spall_core::Revision(brick.revision),
                    content_hash: spall_voxel::BrickHash::from_bytes(content_hash.0),
                    solid_cells,
                    modified_air: brick.edited && solid_cells == 0,
                },
            )),
            _ => None,
        })
        .collect()
}
