//! Receiving a deferred terrain catalogue (`WireTag::CatalogueChunk`).
//!
//! The chunks of one catalogue carry a segmented payload split in order. [`CatalogueReceiver`]
//! feeds them to the same bounded [`SegmentedReceiver`] a baseline uses, so every segment is
//! decoded under its budget, hashed and checked against its manifest before anything reaches the
//! replica, and the chained hash is verified against the one the server declared.

use std::collections::BTreeMap;

use spall_core::BrickCoord;
use spall_protocol::{CATALOGUE_CHUNK_BYTES, CatalogueChunk, Hash32, TransferId};
use spall_voxel::BrickDigest;

use crate::replica::StagedBaseline;
use crate::segmented::{SegmentDigests, SegmentedReceiver};

/// What an interrupted catalogue left the client holding, so a world reset that replaces it can
/// send only the rest ([`CatalogueChunk::basis_chunks`]).
///
/// A segment is verified against its manifest and merged as soon as its last byte arrives, so
/// the client holds exactly the segments that end within the chunks it has processed.
#[derive(Debug, Clone)]
pub struct ResumeState {
    /// What the client held before the interrupted catalogue began: the basis a delta was built
    /// on, empty for a full catalogue.
    base: BTreeMap<BrickCoord, BrickDigest>,
    trail: Vec<SegmentDigests>,
    chunks_received: u32,
}

impl ResumeState {
    /// The digests held after the first `chunks` chunks of the interrupted catalogue: its basis
    /// plus every segment that ends within them. An error if fewer chunks were processed, which
    /// would mean the server assumed something this client does not have.
    pub fn held_after(&self, chunks: u32) -> Result<BTreeMap<BrickCoord, BrickDigest>, String> {
        if chunks > self.chunks_received {
            return Err(format!(
                "the server resumed from {chunks} catalogue chunks but this client processed {}",
                self.chunks_received
            ));
        }
        let bytes = u64::from(chunks) * CATALOGUE_CHUNK_BYTES as u64;
        let mut held = self.base.clone();
        for (ended_at, digests) in &self.trail {
            if *ended_at > bytes {
                break;
            }
            for (coord, digest) in digests {
                held.insert(*coord, *digest);
            }
        }
        Ok(held)
    }
}

/// Incremental receiver for one deferred catalogue.
pub struct CatalogueReceiver {
    transfer_id: TransferId,
    receiver: Option<SegmentedReceiver>,
    next_index: u32,
    chain_hash: Option<Hash32>,
    delta: bool,
    expected_world_hash: Option<Hash32>,
    basis_chunks: Option<u32>,
}

impl CatalogueReceiver {
    /// `checkpoint_tick` is the baseline's; `max_compressed` caps the cumulative bytes accepted.
    pub fn new(transfer_id: TransferId, checkpoint_tick: u64, max_compressed: u64) -> Self {
        let mut receiver = SegmentedReceiver::with_limits(checkpoint_tick, None, max_compressed);
        receiver.track_digests();
        Self {
            transfer_id,
            receiver: Some(receiver),
            next_index: 0,
            chain_hash: None,
            delta: false,
            expected_world_hash: None,
            basis_chunks: None,
        }
    }

    /// What this catalogue has verified so far, for a world reset that replaces it. `base` is
    /// what the client held before it began (`None` for a delta whose basis is unknown, which
    /// cannot be resumed). `None` when no chunk has been processed.
    pub fn resume_state(
        &self,
        base: Option<BTreeMap<BrickCoord, BrickDigest>>,
    ) -> Option<ResumeState> {
        let receiver = self.receiver.as_ref()?;
        if self.next_index == 0 {
            return None;
        }
        Some(ResumeState {
            base: base?,
            trail: receiver.digest_trail().to_vec(),
            chunks_received: self.next_index,
        })
    }

    /// The chunks of the interrupted catalogue the delta builds on, if it declares any. Known
    /// once the first chunk has arrived.
    pub fn basis_chunks(&self) -> Option<u32> {
        self.basis_chunks
    }

    /// Chunks processed so far.
    pub fn chunks_received(&self) -> u32 {
        self.next_index
    }

    /// Whether the catalogue lists only the digests that differ from the one the client held
    /// before a world reset. Known once the first chunk has arrived.
    pub fn delta(&self) -> bool {
        self.delta
    }

    /// The world hash a delta catalogue declares the merged replica must reach.
    pub fn expected_world_hash(&self) -> Option<Hash32> {
        self.expected_world_hash
    }

    /// The baseline this catalogue completes.
    pub fn transfer_id(&self) -> TransferId {
        self.transfer_id
    }

    /// Feeds the next chunk. `Ok(None)` while more chunks are due; `Ok(Some(staged))` after the
    /// last one, once the payload is complete and its chained hash matches. Any error means the
    /// catalogue is unusable and the receiver must be dropped.
    pub fn push(&mut self, chunk: &CatalogueChunk) -> Result<Option<StagedBaseline>, String> {
        if chunk.transfer_id != self.transfer_id {
            return Err(format!(
                "catalogue chunk for transfer {} while receiving {}",
                chunk.transfer_id.0, self.transfer_id.0
            ));
        }
        if chunk.index != self.next_index {
            return Err(format!(
                "catalogue chunk {} arrived where {} was expected",
                chunk.index, self.next_index
            ));
        }
        match self.chain_hash {
            None => {
                self.chain_hash = Some(chunk.chain_hash);
                self.delta = chunk.delta;
                self.expected_world_hash = chunk.expected_world_hash;
                self.basis_chunks = chunk.basis_chunks;
            }
            Some(hash)
                if hash != chunk.chain_hash
                    || self.delta != chunk.delta
                    || self.expected_world_hash != chunk.expected_world_hash
                    || self.basis_chunks != chunk.basis_chunks =>
            {
                return Err("catalogue chunks disagree on their header".into());
            }
            Some(_) => {}
        }
        let receiver = self
            .receiver
            .as_mut()
            .ok_or("catalogue already completed")?;
        receiver.push(&chunk.payload)?;
        self.next_index += 1;
        if self.next_index < chunk.count {
            return Ok(None);
        }
        let receipt = self
            .receiver
            .take()
            .ok_or("catalogue already completed")?
            .finish()?;
        if Some(receipt.chain_hash) != self.chain_hash {
            return Err("the assembled catalogue does not match its declared hash".into());
        }
        Ok(Some(receipt.staged))
    }
}
