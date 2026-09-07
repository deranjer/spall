//! Connection handshake and compatibility check.
//!
//! `docs/protocol.md`: "Handshake includes protocol version, exact content
//! manifest hash, world ID, generator version, supported algorithms, server
//! tick rate, cell-size codes, session identity, and negotiated limits. Reject
//! incompatibility before creating a player."
//!
//! Mismatch is a clear rejection, not speculative compatibility code.

use serde::{Deserialize, Serialize};
use spall_core::{CellSizeCode, WorldId};

use crate::canonical::Hash32;
use crate::limits;
use crate::records::{Record, RecordError, WireTag};
use crate::session::SessionId;

/// Current wire protocol version. Bumped on any breaking record-layout change.
pub const PROTOCOL_VERSION: u16 = 1;

/// Fixed simulation and snapshot rates (Hz).
pub const SERVER_TICK_HZ: u16 = 60;
pub const MOTION_SNAPSHOT_HZ: u16 = 20;

/// Versions of the pluggable algorithms whose output participates in hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlgorithmVersions {
    pub integer_brush: u32,
    pub structure_graph: u32,
    pub topology_hash: u32,
}

/// The negotiated byte / count limits for a connection. Both sides must agree;
/// a client asking for more than the server's ceilings is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedLimits {
    pub max_control_record: u32,
    pub max_bulk_part: u32,
    pub max_assembled_transfer: u64,
    pub max_datagram_payload: u32,
}

impl NegotiatedLimits {
    /// The protocol defaults from [`crate::limits`].
    pub const DEFAULT: Self = Self {
        max_control_record: limits::MAX_CONTROL_RECORD as u32,
        max_bulk_part: limits::MAX_BULK_PART as u32,
        max_assembled_transfer: limits::MAX_ASSEMBLED_TRANSFER as u64,
        max_datagram_payload: limits::MAX_DATAGRAM_PAYLOAD as u32,
    };

    fn within(self, ceiling: Self) -> bool {
        self.max_control_record <= ceiling.max_control_record
            && self.max_bulk_part <= ceiling.max_bulk_part
            && self.max_assembled_transfer <= ceiling.max_assembled_transfer
            && self.max_datagram_payload <= ceiling.max_datagram_payload
    }
}

/// The handshake record. Client sends its expectations; server answers with its
/// own and the assigned session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handshake {
    pub protocol_version: u16,
    pub content_manifest_hash: Hash32,
    pub world_id: WorldId,
    pub generator_version: u32,
    pub algorithms: AlgorithmVersions,
    pub server_tick_hz: u16,
    pub motion_snapshot_hz: u16,
    pub cell_size_codes: Vec<CellSizeCode>,
    pub session: SessionId,
    pub limits: NegotiatedLimits,
}

impl Record for Handshake {
    const TAG: WireTag = WireTag::Handshake;

    fn validate(&self) -> Result<(), RecordError> {
        // At most one entry per known cell-size code.
        limits::check_count("Handshake.cell_size_codes", self.cell_size_codes.len(), 8)
            .map_err(RecordError::Size)?;
        if self.server_tick_hz == 0 || self.motion_snapshot_hz == 0 {
            return Err(RecordError::OutOfRange {
                field: "Handshake.tick_hz",
                detail: "rates must be non-zero",
            });
        }
        Ok(())
    }
}

/// Why two handshakes are incompatible. Each is a hard stop.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Incompatibility {
    #[error("protocol version mismatch: client {client}, server {server}")]
    ProtocolVersion { client: u16, server: u16 },
    #[error("content manifest hash mismatch")]
    ContentManifest,
    #[error("world id mismatch: client {client}, server {server}")]
    WorldId { client: WorldId, server: WorldId },
    #[error("generator version mismatch: client {client}, server {server}")]
    GeneratorVersion { client: u32, server: u32 },
    #[error("algorithm version mismatch")]
    Algorithms,
    #[error("server tick rate mismatch: client {client}, server {server}")]
    TickRate { client: u16, server: u16 },
    #[error("client requested cell-size codes the server does not serve")]
    CellSizeCodes,
    #[error("client requested limits above the server ceiling")]
    Limits,
    #[error("handshake record failed validation: {0}")]
    Invalid(RecordError),
}

/// Checks a client handshake against the server's. Called before a player is
/// created. `Ok(())` means the connection may proceed.
pub fn check_compatible(client: &Handshake, server: &Handshake) -> Result<(), Incompatibility> {
    client.validate().map_err(Incompatibility::Invalid)?;

    if client.protocol_version != server.protocol_version {
        return Err(Incompatibility::ProtocolVersion {
            client: client.protocol_version,
            server: server.protocol_version,
        });
    }
    if client.content_manifest_hash != server.content_manifest_hash {
        return Err(Incompatibility::ContentManifest);
    }
    if client.world_id != server.world_id {
        return Err(Incompatibility::WorldId {
            client: client.world_id,
            server: server.world_id,
        });
    }
    if client.generator_version != server.generator_version {
        return Err(Incompatibility::GeneratorVersion {
            client: client.generator_version,
            server: server.generator_version,
        });
    }
    if client.algorithms != server.algorithms {
        return Err(Incompatibility::Algorithms);
    }
    if client.server_tick_hz != server.server_tick_hz {
        return Err(Incompatibility::TickRate {
            client: client.server_tick_hz,
            server: server.server_tick_hz,
        });
    }
    let server_codes: std::collections::BTreeSet<u8> =
        server.cell_size_codes.iter().map(|c| c.to_u8()).collect();
    if !client
        .cell_size_codes
        .iter()
        .all(|c| server_codes.contains(&c.to_u8()))
    {
        return Err(Incompatibility::CellSizeCodes);
    }
    if !client.limits.within(server.limits) {
        return Err(Incompatibility::Limits);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_handshake() -> Handshake {
        Handshake {
            protocol_version: PROTOCOL_VERSION,
            content_manifest_hash: Hash32::of(b"manifest"),
            world_id: WorldId::from_u128(0x1234),
            generator_version: 7,
            algorithms: AlgorithmVersions {
                integer_brush: 1,
                structure_graph: 1,
                topology_hash: 1,
            },
            server_tick_hz: SERVER_TICK_HZ,
            motion_snapshot_hz: MOTION_SNAPSHOT_HZ,
            cell_size_codes: vec![CellSizeCode::Quarter, CellSizeCode::Sixteenth],
            session: SessionId::from_parts(crate::session::SlotId(1), 1),
            limits: NegotiatedLimits::DEFAULT,
        }
    }

    #[test]
    fn matching_handshakes_are_compatible() {
        let server = server_handshake();
        let client = server.clone();
        assert!(check_compatible(&client, &server).is_ok());
    }

    #[test]
    fn each_mismatch_is_reported_distinctly() {
        let server = server_handshake();

        let mut c = server.clone();
        c.protocol_version = 2;
        assert!(matches!(
            check_compatible(&c, &server),
            Err(Incompatibility::ProtocolVersion { .. })
        ));

        let mut c = server.clone();
        c.content_manifest_hash = Hash32::of(b"other");
        assert_eq!(
            check_compatible(&c, &server),
            Err(Incompatibility::ContentManifest)
        );

        let mut c = server.clone();
        c.generator_version = 8;
        assert!(matches!(
            check_compatible(&c, &server),
            Err(Incompatibility::GeneratorVersion { .. })
        ));

        let mut c = server.clone();
        c.cell_size_codes = vec![CellSizeCode::Quarter];
        assert!(check_compatible(&c, &server).is_ok());

        let mut s_narrow = server.clone();
        s_narrow.cell_size_codes = vec![CellSizeCode::Quarter];
        assert_eq!(
            check_compatible(&server, &s_narrow),
            Err(Incompatibility::CellSizeCodes)
        );

        let mut c = server.clone();
        c.limits.max_control_record = NegotiatedLimits::DEFAULT.max_control_record + 1;
        assert_eq!(check_compatible(&c, &server), Err(Incompatibility::Limits));
    }

    #[test]
    fn invalid_handshake_is_rejected_before_comparison() {
        let server = server_handshake();
        let mut c = server.clone();
        c.server_tick_hz = 0;
        assert!(matches!(
            check_compatible(&c, &server),
            Err(Incompatibility::Invalid(_))
        ));
    }
}
