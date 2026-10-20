//! Authoritative, revision-bound thin snow and transient footprint DTOs.
//!
//! A frame replaces one 4 m XZ tile at one top-cell Y layer. Its 16×16 mask is
//! authoritative: clients may derive the matching upward-facing triangles from
//! replica geometry, but must not infer exposure themselves.

use crate::records::WIRE_SCHEMA_VERSION;
use crate::records::{Record, RecordError, WireTag};
use serde::{Deserialize, Serialize};

pub const SNOW_TILE_CELLS: i32 = 16;
pub const SNOW_MAX_DEPTH_Q16: u16 = 8192; // 0.125 m, Q16 metres.
pub const SNOW_MIN_FOOTPRINT_DEPTH_Q16: u16 = 132; // At least 2 mm.
pub const SNOW_MAX_SURFACES_PER_FRAME: usize = 256;
pub const SNOW_MAX_FOOTPRINTS_PER_FRAME: usize = 64;
pub const SNOW_FOOTPRINT_TTL_TICKS: u64 = 18_000; // Five minutes at 60 Hz.

/// Stable within a world epoch, tile and exposure cohort; geometry revisions
/// may advance for unrelated edits without changing this identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SnowSurfaceId(pub [u8; 16]);

/// One exposure cohort of upward-facing terrain cells at a shared height.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnowSurface {
    pub id: SnowSurfaceId,
    /// Global solid-cell Y immediately below the coated +Y face.
    pub top_cell_y: i32,
    /// Revision of the actual terrain brick containing these faces.
    pub geometry_revision: u64,
    /// Incremented when this tile/Y exposure mask changes.
    pub exposure_generation: u32,
    /// Row-major `local_z * 16 + local_x` mask, disjoint between cohorts.
    pub exposed_faces: [u64; 4],
    /// Q16 metres. Canonical simulation state retains additional Q32 fraction.
    pub depth_q16: u16,
}

/// A server-authored temporary track, bound to the exact supporting snow face.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnowFootprint {
    pub id: u64,
    pub surface_id: SnowSurfaceId,
    pub geometry_revision: u64,
    /// Global solid cell under the contact; XZ must lie in the frame tile and Y
    /// must equal the frame's `top_cell_y`.
    pub contact_cell: [i32; 3],
    /// Sub-cell offsets in millimetres, each in 0..250.
    pub offset_mm: [u16; 2],
    /// Heading in signed normalized turns: -32768..=32767 maps to -π..=π.
    pub heading_q15: i16,
    /// 0 is left, 1 is right.
    pub foot: u8,
    pub created_tick: u64,
    pub strength_q8: u8,
}

/// Atomic full replacement for one 4 m tile and one Y layer. An empty
/// `surfaces` / `footprints` pair is a tombstone. Parts remain below the
/// 64 KiB control-record ceiling because the maximum cohort counts are fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnowSurfaceFrame {
    pub schema: u16,
    pub world_epoch: u64,
    pub world_revision: u64,
    pub effective_tick: u64,
    pub snapshot_seq: u64,
    /// Signed world tile coordinates; one tile spans 16 quarter-metre cells.
    pub tile_x: i32,
    pub tile_z: i32,
    /// Global solid-cell Y for every surface row in this frame.
    pub top_cell_y: i32,
    pub surfaces: Vec<SnowSurface>,
    pub footprints: Vec<SnowFootprint>,
}

impl Record for SnowSurfaceFrame {
    const TAG: WireTag = WireTag::SnowSurfaceFrame;

    fn validate(&self) -> Result<(), RecordError> {
        if self.schema != WIRE_SCHEMA_VERSION {
            return Err(RecordError::Inconsistent("unsupported snow schema"));
        }
        if self.world_epoch == 0 || self.snapshot_seq == 0 {
            return Err(RecordError::Inconsistent(
                "snow epoch and snapshot sequence must be nonzero",
            ));
        }
        if self.surfaces.len() > SNOW_MAX_SURFACES_PER_FRAME {
            return Err(RecordError::OutOfRange {
                field: "SnowSurfaceFrame.surfaces",
                detail: "more than 256 cohorts",
            });
        }
        if self.footprints.len() > SNOW_MAX_FOOTPRINTS_PER_FRAME {
            return Err(RecordError::OutOfRange {
                field: "SnowSurfaceFrame.footprints",
                detail: "more than 64 footprints",
            });
        }
        let mut previous_id = None;
        let mut occupied = [0u64; 4];
        for surface in &self.surfaces {
            if surface.top_cell_y != self.top_cell_y
                || surface.exposure_generation == 0
                || surface.depth_q16 > SNOW_MAX_DEPTH_Q16
                || surface.exposed_faces.iter().all(|word| *word == 0)
            {
                return Err(RecordError::Inconsistent("invalid snow surface bounds"));
            }
            if previous_id.is_some_and(|prior| prior >= surface.id) {
                return Err(RecordError::Inconsistent(
                    "snow surfaces must have sorted unique IDs",
                ));
            }
            previous_id = Some(surface.id);
            for (seen, mask) in occupied.iter_mut().zip(surface.exposed_faces) {
                if *seen & mask != 0 {
                    return Err(RecordError::Inconsistent(
                        "snow cohort face masks must be disjoint",
                    ));
                }
                *seen |= mask;
            }
        }
        let min_x = i64::from(self.tile_x) * i64::from(SNOW_TILE_CELLS);
        let min_z = i64::from(self.tile_z) * i64::from(SNOW_TILE_CELLS);
        let max_x = min_x + i64::from(SNOW_TILE_CELLS);
        let max_z = min_z + i64::from(SNOW_TILE_CELLS);
        let mut previous_footprint = 0;
        for footprint in &self.footprints {
            let [x, y, z] = footprint.contact_cell;
            let dx = i64::from(x) - min_x;
            let dz = i64::from(z) - min_z;
            let face = usize::try_from(dz * i64::from(SNOW_TILE_CELLS) + dx)
                .ok()
                .filter(|_| i64::from(x) >= min_x && i64::from(x) < max_x)
                .filter(|_| i64::from(z) >= min_z && i64::from(z) < max_z)
                .ok_or(RecordError::Inconsistent("footprint outside snow tile"))?;
            let supported = self.surfaces.iter().any(|surface| {
                surface.id == footprint.surface_id
                    && surface.geometry_revision == footprint.geometry_revision
                    && surface.top_cell_y == y
                    && surface.depth_q16 >= SNOW_MIN_FOOTPRINT_DEPTH_Q16
                    && surface.exposed_faces[face / 64] & (1u64 << (face % 64)) != 0
            });
            if footprint.id == 0
                || footprint.id <= previous_footprint
                || footprint.offset_mm.iter().any(|offset| *offset >= 250)
                || footprint.foot > 1
                || footprint.strength_q8 == 0
                || footprint.created_tick > self.effective_tick
                || self.effective_tick - footprint.created_tick > SNOW_FOOTPRINT_TTL_TICKS
                || !supported
            {
                return Err(RecordError::Inconsistent("invalid snow footprint"));
            }
            previous_footprint = footprint.id;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> SnowSurfaceFrame {
        let id = SnowSurfaceId([1; 16]);
        let mut mask = [0u64; 4];
        mask[0] = 1;
        SnowSurfaceFrame {
            schema: WIRE_SCHEMA_VERSION,
            world_epoch: 1,
            world_revision: 0,
            effective_tick: 20,
            snapshot_seq: 1,
            tile_x: 0,
            tile_z: 0,
            top_cell_y: 7,
            surfaces: vec![SnowSurface {
                id,
                top_cell_y: 7,
                geometry_revision: 3,
                exposure_generation: 1,
                exposed_faces: mask,
                depth_q16: 132,
            }],
            footprints: vec![SnowFootprint {
                id: 1,
                surface_id: id,
                geometry_revision: 3,
                contact_cell: [0, 7, 0],
                offset_mm: [125, 125],
                heading_q15: 0,
                foot: 0,
                created_tick: 19,
                strength_q8: 255,
            }],
        }
    }

    #[test]
    fn snow_frame_round_trips_and_validates_authoritative_face_binding() {
        let frame = frame();
        let bytes = crate::encode_control(&frame).unwrap();
        assert_eq!(
            crate::decode_control::<SnowSurfaceFrame>(&bytes).unwrap(),
            frame
        );

        let mut stale = frame.clone();
        stale.footprints[0].geometry_revision = 2;
        assert!(crate::encode_control(&stale).is_err());

        let mut sheltered = frame;
        sheltered.surfaces[0].exposed_faces = [0; 4];
        assert!(crate::encode_control(&sheltered).is_err());
    }
}
