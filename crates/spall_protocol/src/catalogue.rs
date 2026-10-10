//! Deferred terrain catalogue chunks (`WireTag::CatalogueChunk`).
//!
//! A regional baseline carries exact distant terrain as digests, which for a large world is far
//! bigger than the geometry a player needs to start. A client that negotiates the deferred
//! variant (`BaselineBegin.world_version == BASELINE_DEFERRED_REGIONAL_WORLD_VERSION`) receives
//! only the spawn region and bodies in the baseline, and the digest catalogue afterwards as an
//! ordinary segmented payload ([`crate::segment`]) split into bounded chunks on the same reliable
//! stream. The server interleaves chunks between its other reliable messages, so no other record
//! ever waits behind the whole catalogue.
//!
//! The chunks of one catalogue carry the `transfer_id` of the baseline they complete and arrive
//! in order. The receiver feeds `payload` bytes, in order, to the same frame reader a baseline uses.

use serde::{Deserialize, Serialize};

use crate::canonical::Hash32;
use crate::records::{Record, RecordError, TransferId, WireTag};

/// Largest payload of one chunk. Well under the 64 KiB control record limit.
pub const CATALOGUE_CHUNK_BYTES: usize = 32 * 1024;
/// Most chunks of one catalogue: 512 MiB of compressed catalogue, the same ceiling as a streamed
/// baseline.
pub const MAX_CATALOGUE_CHUNKS: u32 =
    (crate::segment::MAX_STREAMED_BASELINE_COMPRESSED / CATALOGUE_CHUNK_BYTES) as u32;

/// `CatalogueChunk`: one in-order slice of a deferred catalogue's segmented payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogueChunk {
    /// The baseline this catalogue completes. A chunk for any other id is stale.
    pub transfer_id: TransferId,
    pub index: u32,
    pub count: u32,
    /// The chained segment hash the assembled payload must reproduce (see
    /// [`crate::segment::chain_hash`]); identical in every chunk of one catalogue.
    pub chain_hash: Hash32,
    /// The catalogue lists only the terrain bricks whose digest differs from the one the client
    /// already holds for the world this baseline replaces (a world reset); every other digest
    /// carries over. A client without that catalogue cannot use it.
    pub delta: bool,
    /// The canonical world hash the client must reach once the catalogue is merged, checked
    /// before any held transaction replays. Present on every delta catalogue.
    pub expected_world_hash: Option<Hash32>,
    /// Set when the delta builds on a catalogue the client had only partly received when the
    /// reset replaced it: the first `n` chunks of that catalogue, whose completed segments (plus
    /// whatever it was itself built on) are what the client already holds. `None` means the
    /// delta builds on a complete catalogue. Only a delta may name a basis.
    pub basis_chunks: Option<u32>,
    pub payload: Vec<u8>,
}

impl Record for CatalogueChunk {
    const TAG: WireTag = WireTag::CatalogueChunk;

    fn validate(&self) -> Result<(), RecordError> {
        if self.delta && self.expected_world_hash.is_none() {
            return Err(RecordError::Inconsistent(
                "a delta catalogue must declare its expected world hash",
            ));
        }
        if self.basis_chunks.is_some() && !self.delta {
            return Err(RecordError::Inconsistent(
                "only a delta catalogue can build on a partly received one",
            ));
        }
        if self.basis_chunks.is_some_and(|n| n > MAX_CATALOGUE_CHUNKS) {
            return Err(RecordError::Inconsistent("invalid catalogue resume basis"));
        }
        if self.count == 0
            || self.count > MAX_CATALOGUE_CHUNKS
            || self.index >= self.count
            || self.payload.is_empty()
            || self.payload.len() > CATALOGUE_CHUNK_BYTES
        {
            return Err(RecordError::Inconsistent("invalid catalogue chunk"));
        }
        Ok(())
    }
}

/// Most chunks the server keeps unacknowledged on a connection. Each is at most
/// [`CATALOGUE_CHUNK_BYTES`], so a slow link buffers about this many bytes of catalogue ahead of
/// whatever else the server must send, instead of however much the transport will accept.
pub const CATALOGUE_WINDOW_CHUNKS: u32 = 4;

/// `CatalogueAck`: the client has processed catalogue chunk `index` of `transfer_id`. The server
/// uses it as credit to send the next chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogueAck {
    pub transfer_id: TransferId,
    pub index: u32,
}

impl Record for CatalogueAck {
    const TAG: WireTag = WireTag::CatalogueAck;

    fn validate(&self) -> Result<(), RecordError> {
        if self.index >= MAX_CATALOGUE_CHUNKS {
            return Err(RecordError::Inconsistent("invalid catalogue ack"));
        }
        Ok(())
    }
}

/// `ControlProbe`: measures how long a record takes to cross the reliable control stream. The
/// server sends one with `echo: false`; the client answers in order with the same `id` and
/// `echo: true`. Because the stream is ordered, the round trip includes everything the server
/// had already queued ahead of the probe, so its delay is the stream's queueing delay and the
/// server holds back droppable traffic (water, catalogue chunks) while that delay is large.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlProbe {
    pub id: u64,
    pub echo: bool,
}

impl Record for ControlProbe {
    const TAG: WireTag = WireTag::ControlProbe;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

/// Splits one catalogue payload into chunks for `transfer_id`.
pub fn chunk_catalogue(
    transfer_id: TransferId,
    chain_hash: Hash32,
    delta: bool,
    expected_world_hash: Option<Hash32>,
    basis_chunks: Option<u32>,
    payload: &[u8],
) -> Result<Vec<CatalogueChunk>, String> {
    if payload.is_empty() {
        return Err("empty catalogue payload".into());
    }
    let count = payload.len().div_ceil(CATALOGUE_CHUNK_BYTES);
    if count as u64 > u64::from(MAX_CATALOGUE_CHUNKS) {
        return Err(format!("catalogue needs {count} chunks"));
    }
    Ok(payload
        .chunks(CATALOGUE_CHUNK_BYTES)
        .enumerate()
        .map(|(index, bytes)| CatalogueChunk {
            transfer_id,
            index: index as u32,
            count: count as u32,
            chain_hash,
            delta,
            expected_world_hash,
            basis_chunks,
            payload: bytes.to_vec(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_the_payload_in_order_and_validate() {
        let payload: Vec<u8> = (0..CATALOGUE_CHUNK_BYTES * 2 + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        let chunks = chunk_catalogue(
            TransferId(9),
            Hash32::of(&payload),
            false,
            None,
            None,
            &payload,
        )
        .unwrap();
        assert_eq!(chunks.len(), 3);
        let mut joined = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            chunk.validate().unwrap();
            assert_eq!(chunk.index as usize, i);
            assert_eq!(chunk.count, 3);
            joined.extend_from_slice(&chunk.payload);
        }
        assert_eq!(joined, payload);
    }

    #[test]
    fn malformed_chunks_are_rejected() {
        let ok = CatalogueChunk {
            transfer_id: TransferId(1),
            index: 0,
            count: 1,
            chain_hash: Hash32::ZERO,
            delta: false,
            expected_world_hash: None,
            basis_chunks: None,
            payload: vec![1],
        };
        assert!(ok.validate().is_ok());
        for bad in [
            CatalogueChunk {
                count: 0,
                ..ok.clone()
            },
            CatalogueChunk {
                index: 1,
                ..ok.clone()
            },
            CatalogueChunk {
                payload: Vec::new(),
                ..ok.clone()
            },
            CatalogueChunk {
                payload: vec![0; CATALOGUE_CHUNK_BYTES + 1],
                ..ok.clone()
            },
            CatalogueChunk {
                count: MAX_CATALOGUE_CHUNKS + 1,
                ..ok.clone()
            },
            CatalogueChunk {
                delta: true,
                ..ok.clone()
            },
            // A resume basis only makes sense on a delta.
            CatalogueChunk {
                basis_chunks: Some(2),
                ..ok.clone()
            },
            CatalogueChunk {
                delta: true,
                expected_world_hash: Some(Hash32::ZERO),
                basis_chunks: Some(MAX_CATALOGUE_CHUNKS + 1),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        let resumed = CatalogueChunk {
            delta: true,
            expected_world_hash: Some(Hash32::ZERO),
            basis_chunks: Some(7),
            ..ok
        };
        assert!(resumed.validate().is_ok());
    }

    #[test]
    fn a_resume_basis_is_carried_by_every_chunk() {
        let payload = vec![3u8; CATALOGUE_CHUNK_BYTES + 5];
        let chunks = chunk_catalogue(
            TransferId(2),
            Hash32::of(&payload),
            true,
            Some(Hash32::ZERO),
            Some(4),
            &payload,
        )
        .unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.basis_chunks == Some(4) && c.delta));
        for chunk in &chunks {
            chunk.validate().unwrap();
        }
    }
}
