//! Water presentation snapshots and admin commands (ENG-105 increment 2).
//!
//! Water replication is presentation-only: the server owns the fluid solver and
//! periodically sends a full keyframe of quantized cell fractions. Keyframes
//! are self-contained, so a late joiner or a client that missed one is repaired
//! by the next; changed-brick deltas use a validated baseline sequence. A keyframe is the zstd stream of
//! one byte per fluid cell, split into [`WaterSnapshot`] chunks that each fit
//! a control record.

use serde::{Deserialize, Serialize};
use spall_core::{GlobalCell, Tick};

use crate::limits::{self, SizeLimitError};
use crate::records::{Record, RecordError, RequestId, WireTag};

/// Largest fluid domain a keyframe may describe, in cells. Also the
/// decompression bound on the receiving side.
pub const MAX_WATER_CELLS: usize = 4 * 1024 * 1024;
/// Compressed bytes carried by one chunk; leaves headroom under
/// [`limits::MAX_CONTROL_RECORD`] for the envelope and other fields.
pub const MAX_WATER_CHUNK: usize = 48 * 1024;
/// Chunks in one keyframe. The compressed staging limit is 4.5 MiB. A domain
/// within the cell limit can still exceed this limit if poorly compressible;
/// the encoder returns an explicit error in that case.
pub const MAX_WATER_CHUNKS: usize = 96;
/// Largest voxels-per-fluid-cell factor a keyframe may declare.
pub const MAX_WATER_COARSEN: u8 = 8;
/// Aggregate limit across independently bounded pressure domains.
pub const MAX_WATER_REGIONS: usize = 8;
pub const MAX_WATER_SOURCE_CELLS: usize = 65_536;

/// Validate a checkpoint/journal group before allocating solver grids.
pub fn validate_water_states(states: &[WaterState]) -> Result<(), WaterCodecError> {
    if states.len() > MAX_WATER_REGIONS {
        return Err(WaterCodecError::InvalidChunk(
            "too many water regions".into(),
        ));
    }
    let mut cells = 0usize;
    for state in states {
        state.validate()?;
        cells = cells
            .checked_add(state.fractions.len())
            .ok_or(WaterCodecError::InvalidChunk(
                "aggregate water cell limit exceeded".into(),
            ))?;
    }
    if cells > MAX_WATER_CELLS {
        return Err(WaterCodecError::InvalidChunk(
            "aggregate water cell limit exceeded".into(),
        ));
    }
    Ok(())
}

/// Canonical restart state, schema independent of presentation snapshots.
/// Fractions and retained cell-volumes use explicit IEEE-754 bits. Velocities,
/// pressure and solver caches deliberately reset to rest after recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaterState {
    pub version: u16,
    pub origin: [i64; 3],
    pub voxel_dimensions: [u32; 3],
    pub coarsen: u32,
    /// cell size, density, gravity XYZ, CFL, relative/absolute pressure tolerance.
    pub config_bits: [u64; 8],
    pub max_substeps: u32,
    pub pressure_max_iterations: u32,
    pub open_top: bool,
    pub frame_seq: u64,
    pub fluid_time_bits: u64,
    pub spring_added_bits: u64,
    pub drain_removed_bits: u64,
    pub outflow_bits: u64,
    pub fractions: Vec<u64>,
    pub trapped: Vec<u64>,
    pub sources: Vec<[i64; 3]>,
    pub gated_sources: [Vec<[i64; 3]>; 3],
    pub sinks: Vec<[i64; 3]>,
    pub gated_rate: u8,
}

impl WaterState {
    pub fn validate(&self) -> Result<(), WaterCodecError> {
        let bad = || WaterCodecError::InvalidChunk("invalid canonical water state".into());
        if self.version != 1
            || !(1..=u32::from(MAX_WATER_COARSEN)).contains(&self.coarsen)
            || self.frame_seq == 0
            || self.gated_rate > 3
        {
            return Err(bad());
        }
        let mut cells = 1usize;
        let mut voxels = 1usize;
        for (i, axis) in self.voxel_dimensions.iter().enumerate() {
            if *axis == 0
                || *axis % self.coarsen != 0
                || self.origin[i].checked_add(i64::from(*axis) - 1).is_none()
            {
                return Err(bad());
            }
            cells = cells
                .checked_mul((*axis / self.coarsen) as usize)
                .ok_or_else(bad)?;
            voxels = voxels.checked_mul(*axis as usize).ok_or_else(bad)?;
        }
        if voxels > 32 * 1024 * 1024 {
            return Err(bad());
        }
        if cells > MAX_WATER_CELLS || self.fractions.len() != cells || self.trapped.len() != cells {
            return Err(bad());
        }
        if self.fractions.iter().any(|b| {
            let v = f64::from_bits(*b);
            !v.is_finite() || !(-1e-9..=1.0 + 1e-9).contains(&v)
        }) || self.trapped.iter().any(|b| {
            let v = f64::from_bits(*b);
            !v.is_finite() || v < 0.0
        }) {
            return Err(bad());
        }
        if !self
            .trapped
            .iter()
            .map(|b| f64::from_bits(*b))
            .sum::<f64>()
            .is_finite()
        {
            return Err(bad());
        }
        for bits in [
            self.fluid_time_bits,
            self.spring_added_bits,
            self.drain_removed_bits,
            self.outflow_bits,
        ] {
            let v = f64::from_bits(bits);
            if !v.is_finite() || v < 0.0 {
                return Err(bad());
            }
        }
        for list in std::iter::once(&self.sources)
            .chain(self.gated_sources.iter())
            .chain(std::iter::once(&self.sinks))
        {
            if list.len() > MAX_WATER_SOURCE_CELLS {
                return Err(bad());
            }
            if list.iter().any(|cell| {
                (0..3).any(|i| {
                    cell[i] < self.origin[i]
                        || cell[i] > self.origin[i] + i64::from(self.voxel_dimensions[i]) - 1
                })
            }) {
                return Err(bad());
            }
        }
        let c = self.config_bits.map(f64::from_bits);
        if c.iter().any(|v| !v.is_finite())
            || c[0] != 0.25 * f64::from(self.coarsen)
            || c[1] <= 0.0
            || c[5] <= 0.0
            || c[5] > 1.0
            || c[6] <= 0.0
            || c[7] <= 0.0
            || self.max_substeps == 0
            || self.pressure_max_iterations == 0
        {
            return Err(bad());
        }
        Ok(())
    }
}

/// One chunk of a water keyframe. Every chunk of a frame repeats its header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaterSnapshot {
    pub server_tick: Tick,
    /// The server's fluid frame sequence; strictly increases within a world
    /// and restarts after an admin world reset.
    pub frame_seq: u64,
    /// Voxel-cell coordinate of the domain's minimum corner.
    pub origin: GlobalCell,
    /// Fluid cells per axis.
    pub dimensions: [u32; 3],
    /// Voxels per fluid cell along each axis.
    pub coarsen: u8,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub payload: Vec<u8>,
}

impl WaterSnapshot {
    /// Cells in the described domain, `None` on overflow.
    pub fn cell_count(&self) -> Option<usize> {
        self.dimensions
            .iter()
            .try_fold(1usize, |n, axis| n.checked_mul(*axis as usize))
    }
}

impl Record for WaterSnapshot {
    const TAG: WireTag = WireTag::WaterSnapshot;

    fn validate(&self) -> Result<(), RecordError> {
        let cells = self.cell_count().ok_or(RecordError::Inconsistent(
            "water domain cell count overflows",
        ))?;
        if cells == 0 {
            return Err(RecordError::Inconsistent("water domain has an empty axis"));
        }
        limits::check_count("WaterSnapshot.cells", cells, MAX_WATER_CELLS)?;
        if self.coarsen == 0 || self.coarsen > MAX_WATER_COARSEN {
            return Err(RecordError::OutOfRange {
                field: "WaterSnapshot.coarsen",
                detail: "must be 1..=8",
            });
        }
        for (origin, axis) in [self.origin.x, self.origin.y, self.origin.z]
            .into_iter()
            .zip(self.dimensions)
        {
            if origin
                .checked_add(i64::from(axis) * i64::from(self.coarsen) - 1)
                .is_none()
            {
                return Err(RecordError::Inconsistent(
                    "water domain coordinate overflow",
                ));
            }
        }
        limits::check_count(
            "WaterSnapshot.chunk_count",
            usize::from(self.chunk_count),
            MAX_WATER_CHUNKS,
        )?;
        if self.chunk_count == 0 || self.chunk_index >= self.chunk_count {
            return Err(RecordError::Inconsistent("water chunk index out of range"));
        }
        if self.payload.is_empty() {
            return Err(RecordError::Inconsistent("water chunk is empty"));
        }
        if self.payload.len() > MAX_WATER_CHUNK {
            return Err(SizeLimitError::bytes(
                "WaterSnapshot.payload",
                self.payload.len(),
                MAX_WATER_CHUNK,
            )
            .into());
        }
        if self.chunk_index + 1 < self.chunk_count && self.payload.len() != MAX_WATER_CHUNK {
            return Err(RecordError::Inconsistent("nonfinal water chunk is short"));
        }
        Ok(())
    }
}

/// Why a keyframe could not be built or assembled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaterCodecError {
    #[error("water domain has an empty axis")]
    EmptyDomain,
    #[error("invalid water chunk: {0}")]
    InvalidChunk(String),
    #[error("water frame has {cells} fractions but its dimensions describe {expected}")]
    LengthMismatch { cells: usize, expected: usize },
    #[error("water frame describes {0} cells, above the keyframe limit")]
    TooManyCells(usize),
    #[error("compressed water frame needs {0} chunks, above the keyframe limit")]
    TooManyChunks(usize),
    #[error("water coarsening factor {0} is outside 1..=8")]
    Coarsen(u32),
    #[error("zstd: {0}")]
    Zstd(String),
}

/// Compresses one frame's fractions into ordered chunks.
pub fn encode_water_keyframe(
    server_tick: Tick,
    frame_seq: u64,
    origin: GlobalCell,
    dimensions: [u32; 3],
    coarsen: u32,
    fractions: &[u8],
) -> Result<Vec<WaterSnapshot>, WaterCodecError> {
    let expected = dimensions
        .iter()
        .try_fold(1usize, |n, axis| n.checked_mul(*axis as usize))
        .ok_or(WaterCodecError::TooManyCells(usize::MAX))?;
    if expected == 0 {
        return Err(WaterCodecError::EmptyDomain);
    }
    if expected > MAX_WATER_CELLS {
        return Err(WaterCodecError::TooManyCells(expected));
    }
    if fractions.len() != expected {
        return Err(WaterCodecError::LengthMismatch {
            cells: fractions.len(),
            expected,
        });
    }
    let coarsen = u8::try_from(coarsen)
        .ok()
        .filter(|c| (1..=MAX_WATER_COARSEN).contains(c))
        .ok_or(WaterCodecError::Coarsen(coarsen))?;
    let compressed = zstd::stream::encode_all(fractions, 3)
        .map_err(|error| WaterCodecError::Zstd(error.to_string()))?;
    let chunk_count = compressed.len().div_ceil(MAX_WATER_CHUNK).max(1);
    if chunk_count > MAX_WATER_CHUNKS {
        return Err(WaterCodecError::TooManyChunks(chunk_count));
    }
    Ok(compressed
        .chunks(MAX_WATER_CHUNK)
        .enumerate()
        .map(|(index, payload)| WaterSnapshot {
            server_tick,
            frame_seq,
            origin,
            dimensions,
            coarsen,
            chunk_index: index as u16,
            chunk_count: chunk_count as u16,
            payload: payload.to_vec(),
        })
        .collect())
}

/// A fully assembled, decoded keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaterKeyframe {
    pub server_tick: Tick,
    pub frame_seq: u64,
    pub origin: GlobalCell,
    pub dimensions: [u32; 3],
    pub coarsen: u8,
    /// One byte per fluid cell, X fastest, then Y, then Z; `255` is full.
    pub fractions: Vec<u8>,
}

/// Reassembles keyframe chunks arriving in order on the control stream. A
/// chunk from a different frame than the one in progress abandons it: the
/// server sends whole frames back to back. Missing chunks or mismatched
/// headers discard the partial frame; the next complete keyframe repairs it.
#[derive(Debug, Default)]
pub struct WaterAssembler {
    partial: Option<(WaterSnapshot, Vec<u8>)>,
}

impl WaterAssembler {
    /// Validates a chunk before staging or allocating decoded storage; returns
    /// the frame it completes, if any. Invalid input discards partial staging.
    pub fn push(&mut self, chunk: WaterSnapshot) -> Result<Option<WaterKeyframe>, WaterCodecError> {
        if let Err(error) = chunk.validate() {
            self.partial = None;
            return Err(WaterCodecError::InvalidChunk(error.to_string()));
        }
        let same_frame = self.partial.as_ref().is_some_and(|(head, bytes)| {
            head.frame_seq == chunk.frame_seq
                && head.server_tick == chunk.server_tick
                && head.chunk_count == chunk.chunk_count
                && head.dimensions == chunk.dimensions
                && head.origin == chunk.origin
                && head.coarsen == chunk.coarsen
                && bytes.len() == usize::from(chunk.chunk_index) * MAX_WATER_CHUNK
        });
        if !same_frame {
            self.partial = None;
            if chunk.chunk_index != 0 {
                // Joined mid-frame; wait for the next frame's first chunk.
                return Ok(None);
            }
        }
        let last = chunk.chunk_index + 1 == chunk.chunk_count;
        let (head, bytes) = self
            .partial
            .get_or_insert_with(|| (chunk.clone(), Vec::new()));
        bytes.extend_from_slice(&chunk.payload);
        if !last {
            return Ok(None);
        }
        let head = head.clone();
        let (_, compressed) = self.partial.take().expect("partial frame present");
        let expected = head
            .cell_count()
            .ok_or(WaterCodecError::TooManyCells(usize::MAX))?;
        use std::io::Read;
        let decoder = zstd::stream::Decoder::new(compressed.as_slice())
            .map_err(|error| WaterCodecError::Zstd(error.to_string()))?;
        let mut fractions = Vec::with_capacity(expected);
        decoder
            .take(expected as u64 + 1)
            .read_to_end(&mut fractions)
            .map_err(|error| WaterCodecError::Zstd(error.to_string()))?;
        if fractions.len() != expected {
            return Err(WaterCodecError::LengthMismatch {
                cells: fractions.len(),
                expected,
            });
        }
        Ok(Some(WaterKeyframe {
            server_tick: head.server_tick,
            frame_seq: head.frame_seq,
            origin: head.origin,
            dimensions: head.dimensions,
            coarsen: head.coarsen,
            fractions,
        }))
    }
}

/// An operator command from a development/admin client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdminCommand {
    /// Rebuild the world from its scene: terrain, bodies, water, and every
    /// player back at a spawn. Connected clients receive a replacing baseline.
    ResetWorld,
    /// Sets the scene's gated water spring(s) rate, e.g. to fill a reservoir
    /// from dry: `0` is off, `1..=3` an increasingly fast fill (out-of-range
    /// values are refused, not clamped, so a stale/misbehaving client is
    /// visible rather than silently reinterpreted). A scene with no gated
    /// spring refuses.
    SetWaterSpring { rate: u8 },
    /// Opens (cuts to air) or closes (refills with stone) the scene's dam
    /// gate. A scene with no authored gate refuses.
    SetDamGate { open: bool },
}

/// `AdminRequest`: reliable operator command. The server decides whether this
/// session may issue it and answers with [`AdminStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminRequest {
    pub request_id: RequestId,
    pub command: AdminCommand,
}

impl Record for AdminRequest {
    const TAG: WireTag = WireTag::AdminRequest;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

/// Longest [`AdminStatus::message`] accepted on the wire.
pub const MAX_ADMIN_MESSAGE: usize = 512;

/// `AdminStatus`: the server's answer to one [`AdminRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminStatus {
    pub request_id: RequestId,
    pub accepted: bool,
    pub message: String,
}

impl Record for AdminStatus {
    const TAG: WireTag = WireTag::AdminStatus;

    fn validate(&self) -> Result<(), RecordError> {
        if self.message.len() > MAX_ADMIN_MESSAGE {
            return Err(SizeLimitError::bytes(
                "AdminStatus.message",
                self.message.len(),
                MAX_ADMIN_MESSAGE,
            )
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cells: usize, seed: u64) -> Vec<u8> {
        // Pseudo-random bytes defeat compression, forcing several chunks.
        let mut state = seed;
        (0..cells)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn multi_chunk_keyframe_round_trips_and_validates() {
        let dims = [128, 8, 128];
        let fractions = frame(128 * 8 * 128, 7);
        let origin = GlobalCell::new(-4, 2, 10);
        let chunks = encode_water_keyframe(Tick(9), 3, origin, dims, 2, &fractions).unwrap();
        assert!(chunks.len() > 1, "incompressible frame should need chunks");
        let mut assembler = WaterAssembler::default();
        let mut done = None;
        for chunk in chunks {
            chunk.validate().unwrap();
            let encoded = crate::encode_control(&chunk).unwrap();
            assert!(encoded.len() <= limits::MAX_CONTROL_RECORD);
            let decoded: WaterSnapshot = crate::decode_control(&encoded).unwrap();
            done = assembler.push(decoded).unwrap();
        }
        let done = done.expect("last chunk completes the frame");
        assert_eq!(done.fractions, fractions);
        assert_eq!(done.origin, origin);
        assert_eq!(done.coarsen, 2);
    }

    #[test]
    fn a_newer_frame_abandons_a_partial_one_and_mid_frame_joins_wait() {
        let dims = [128, 8, 128];
        let old = encode_water_keyframe(
            Tick(1),
            1,
            GlobalCell::new(0, 0, 0),
            dims,
            1,
            &frame(131072, 1),
        )
        .unwrap();
        let new_fractions = vec![0u8; 131072];
        let new = encode_water_keyframe(
            Tick(2),
            2,
            GlobalCell::new(0, 0, 0),
            dims,
            1,
            &new_fractions,
        )
        .unwrap();
        let mut assembler = WaterAssembler::default();
        // Mid-frame join: the tail of an old frame is ignored.
        assert_eq!(assembler.push(old[1].clone()).unwrap(), None);
        assert_eq!(assembler.push(old[0].clone()).unwrap(), None);
        let done = new
            .into_iter()
            .map(|chunk| assembler.push(chunk).unwrap())
            .last()
            .flatten()
            .unwrap();
        assert_eq!(done.frame_seq, 2);
        assert_eq!(done.fractions, new_fractions);
    }

    #[test]
    fn keyframe_limits_are_enforced_before_allocation() {
        let mut chunk =
            encode_water_keyframe(Tick(1), 1, GlobalCell::new(0, 0, 0), [2, 2, 2], 2, &[0; 8])
                .unwrap()
                .remove(0);
        chunk.dimensions = [4096, 4096, 4096];
        assert!(chunk.validate().is_err());
        chunk.dimensions = [2, 2, 2];
        chunk.coarsen = 9;
        assert!(chunk.validate().is_err());
        assert_eq!(
            encode_water_keyframe(Tick(1), 1, GlobalCell::new(0, 0, 0), [2, 2, 2], 2, &[0; 7]),
            Err(WaterCodecError::LengthMismatch {
                cells: 7,
                expected: 8
            })
        );
    }

    #[test]
    fn assembler_rejects_unvalidated_headers_and_recovers() {
        let valid =
            encode_water_keyframe(Tick(1), 1, GlobalCell::new(0, 0, 0), [2, 2, 2], 1, &[0; 8])
                .unwrap()
                .remove(0);
        let mut invalid = Vec::new();
        let mut chunk = valid.clone();
        chunk.dimensions = [u32::MAX; 3];
        invalid.push(chunk);
        let mut chunk = valid.clone();
        chunk.chunk_index = u16::MAX;
        invalid.push(chunk);
        let mut chunk = valid.clone();
        chunk.coarsen = 0;
        invalid.push(chunk);
        let mut chunk = valid.clone();
        chunk.chunk_count = MAX_WATER_CHUNKS as u16 + 1;
        invalid.push(chunk);
        let mut chunk = valid.clone();
        chunk.payload = vec![0; MAX_WATER_CHUNK + 1];
        invalid.push(chunk);
        let mut assembler = WaterAssembler::default();
        for chunk in invalid {
            assert!(assembler.push(chunk).is_err());
            assert!(assembler.partial.is_none());
            assert_eq!(
                assembler.push(valid.clone()).unwrap().unwrap().fractions,
                [0; 8]
            );
        }
    }

    #[test]
    fn short_nonfinal_chunk_is_rejected_and_clears_staging() {
        let chunks = encode_water_keyframe(
            Tick(1),
            1,
            GlobalCell::new(0, 0, 0),
            [128, 8, 128],
            1,
            &frame(131072, 7),
        )
        .unwrap();
        let mut assembler = WaterAssembler::default();
        assert!(assembler.push(chunks[0].clone()).unwrap().is_none());
        let mut short = chunks[0].clone();
        short.payload.pop();
        assert!(short.validate().is_err());
        assert!(assembler.push(short).is_err());
        assert!(assembler.partial.is_none());
        for chunk in chunks {
            assembler.push(chunk).unwrap();
        }
        assert!(assembler.partial.is_none());
    }

    #[test]
    fn decompression_overrun_and_corruption_do_not_publish_or_poison_repair() {
        let valid =
            encode_water_keyframe(Tick(1), 1, GlobalCell::new(0, 0, 0), [2, 2, 2], 1, &[0; 8])
                .unwrap()
                .remove(0);
        let mut assembler = WaterAssembler::default();
        let mut bomb = valid.clone();
        bomb.payload = zstd::stream::encode_all(&vec![0; MAX_WATER_CELLS][..], 3).unwrap();
        assert_eq!(
            assembler.push(bomb),
            Err(WaterCodecError::LengthMismatch {
                cells: 9,
                expected: 8,
            })
        );
        let mut corrupt = valid.clone();
        corrupt.payload = vec![0; 20];
        assert!(assembler.push(corrupt).is_err());
        assert!(assembler.partial.is_none());
        assert_eq!(assembler.push(valid).unwrap().unwrap().fractions, [0; 8]);
    }

    #[test]
    fn encoder_rejects_empty_domains() {
        assert!(
            encode_water_keyframe(Tick(1), 1, GlobalCell::new(0, 0, 0), [2, 0, 2], 1, &[],)
                .is_err()
        );
    }
}
