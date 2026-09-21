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

/// Incremental receiver for one segmented transfer.
pub struct SegmentedReceiver {
    reader: FrameReader,
    manifest_body: Option<Vec<u8>>,
    validator: Option<SequenceValidator>,
    staged: StagedBaseline,
    raw_hashes: Vec<Hash32>,
    staging_budget: Option<u64>,
    max_segment_decoded: u64,
    max_buffered: usize,
}

impl SegmentedReceiver {
    /// `checkpoint_tick` comes from `BaselineBegin`; `staging_budget` caps the manifest's declared
    /// decoded bytes (checked before any segment is decoded).
    pub fn new(checkpoint_tick: u64, staging_budget: Option<u64>) -> Self {
        Self {
            reader: FrameReader::new(),
            manifest_body: None,
            validator: None,
            staged: StagedBaseline::new(checkpoint_tick),
            raw_hashes: Vec::new(),
            staging_budget,
            max_segment_decoded: 0,
            max_buffered: 0,
        }
    }

    /// The manifest, once received.
    pub fn manifest(&self) -> Option<&SegmentManifest> {
        self.validator.as_ref().map(|v| v.manifest())
    }

    /// Feeds the next part's payload. Returns a reason naming the segment on any failure; the
    /// receiver must then be dropped (the replica has not been touched).
    pub fn push(&mut self, payload: &[u8]) -> Result<(), String> {
        self.reader.push(payload);
        self.max_buffered = self.max_buffered.max(self.reader.buffered());
        while let Some(frame) = self.reader.next_frame().map_err(|e| e.to_string())? {
            match (frame, self.validator.as_mut()) {
                (Frame::Manifest(body), None) => {
                    let m = segment::decode_manifest(&body).map_err(|e| e.to_string())?;
                    if let Some(budget) = self.staging_budget
                        && m.total_decoded_bytes > budget
                    {
                        return Err(format!(
                            "baseline needs {} decoded bytes of staging, the client budget is {budget}",
                            m.total_decoded_bytes
                        ));
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
