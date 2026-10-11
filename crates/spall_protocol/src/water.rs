//! Operator (admin) commands for the water and the world, and the error type
//! the column-water records share. The grid-water keyframe and delta records
//! this module once held were removed with the grid solver (ENG-169); the
//! column water travels in [`crate::column_water_wire`].

use serde::{Deserialize, Serialize};

use crate::limits::SizeLimitError;
use crate::records::{Record, RecordError, RequestId, WireTag};

/// Source and sink cells one water setup may carry.
pub const MAX_WATER_SOURCE_CELLS: usize = 65_536;

/// Why a water record could not be validated or applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WaterCodecError {
    #[error("invalid water record: {0}")]
    InvalidChunk(String),
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
