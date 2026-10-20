//! The persistent world save schema — versioned DTOs that are deliberately
//! **separate from runtime handles**.
//!
//! `docs/architecture.md`: "persistence does not own simulation objects" and
//! "No physics library internals are the primary save format". Nothing here
//! references `rapier3d`, `hecs`, `spall_sim`, or a live [`spall_voxel::Volume`];
//! `spall_sim::persist` converts authoritative state to and from these records.
//!
//! Every record is `postcard`-encoded (the workspace's binary DTO codec) and
//! the whole tree is versioned by [`STORE_SCHEMA_VERSION`]. Wire records that
//! appear inside the journal ([`JournalPayload`]) keep their own independent
//! `spall_protocol` schema version, exactly as `docs/protocol.md` requires
//! ("Version the wire schema independently of the world save schema").

use serde::{Deserialize, Serialize};

#[cfg(test)]
use spall_protocol::baseline::BaselineWorld;
use spall_protocol::{
    CodecError, MotionSnapshot, TopologyTransaction, decode_control, encode_control,
};

/// Version of the on-disk save schema: the single pre-release save version, defined in
/// `spall_core` so crates below the store can stamp it too. Bumped on any change to a persisted
/// layout. A database written by another version is rejected on open without modification
/// (`SchemaTooOld`/`SchemaTooNew`); there is no migration before the first public release.
pub use spall_core::STORE_SCHEMA_VERSION;

/// Largest accepted stored brick payload, compressed. `docs/protocol.md` caps a
/// material-only brick record at 256 KiB decompressed for 64 KiB of real
/// payload; the compressed frame is always smaller, and this bounds a hostile
/// or corrupt row before allocation.
pub const MAX_STORED_BRICK_BYTES: usize = 256 * 1024;
/// Largest stored stacked-column water checkpoint (ENG-167).
pub const MAX_STORED_COLUMN_WATER_BYTES: usize = 512 * 1024 * 1024;

/// Errors converting between save records and their encoded forms.
#[derive(Debug, thiserror::Error)]
pub enum DtoError {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("protocol codec: {0}")]
    Codec(#[from] CodecError),
    #[error("transaction attachments: {0}")]
    Attachments(#[from] spall_protocol::TransactionAttachmentError),
    #[error("typed journal control bounds or row tick mismatch")]
    JournalBounds,
    #[error("invalid versioned snow tile journal state")]
    SnowTileBounds,
}

/// Encode a save record to its postcard bytes.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, DtoError> {
    Ok(postcard::to_stdvec(value)?)
}

/// Decode a save record from postcard bytes.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, DtoError> {
    Ok(postcard::from_bytes(bytes)?)
}

/// Journal records use the current protocol schema exactly. Historical headers
/// are rejected by the shared codec; saved rows are never reinterpreted under a
/// newer enum layout.
fn decode_saved_record<R: spall_protocol::Record>(bytes: &[u8]) -> Result<R, CodecError> {
    decode_control(bytes)
}
/// Required world metadata (`docs/protocol.md` "Persistence"): schema version,
/// world identity, seed, generator version, material manifest hash, cell-size
/// codes, next-ID counters, and structural algorithm versions. The
/// checkpoint tick/cursor live on the [`Checkpoint`] itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWorldMeta {
    pub store_schema_version: u32,
    pub world_id: u128,
    pub seed: u64,
    pub generator_version: u32,
    pub material_manifest_hash: [u8; 32],
    /// Cell-size codes in use, ascending. `spall_core::CellSizeCode as u8`.
    pub cell_size_codes: Vec<u8>,
    pub next_entity: u64,
    pub next_volume: u64,
    pub next_transaction: u64,
    pub next_journal_seq: u64,
    pub integer_brush_version: u32,
    pub structure_graph_version: u32,
    pub topology_hash_version: u32,
}

/// A rigid pose stored losslessly — no i16 quaternion quantization, so a
/// "rotated fractured body" comes back at exactly the saved orientation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredPose {
    pub translation_m: [f64; 3],
    /// Unit quaternion, `x, y, z, w`.
    pub rotation_xyzw: [f64; 4],
}

/// Whether a stored body is the world terrain grid or a detached dynamic body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredBodyKind {
    Terrain,
    Dynamic,
}

/// One authoritative body record (`docs/protocol.md`: "stable IDs, voxel
/// geometry references, pose/velocity, sleep state, damage/bond state, and mass
/// inputs"). Geometry is referenced by `volume_id`; the cells live in
/// [`StoredBrick`] rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredBody {
    /// `0` marks terrain (canonical owner is the world grid, not an entity).
    pub entity_id: u64,
    pub volume_id: u64,
    pub kind: StoredBodyKind,
    /// `spall_core::CellSizeCode as u8`.
    pub cell_size_code: u8,
    pub pose: StoredPose,
    pub linvel_m_s: [f64; 3],
    pub angvel_rad_s: [f64; 3],
    pub sleeping: bool,
    /// Bumped on every collider rebuild; recovery restores it so replication
    /// revision comparisons stay monotone across a restart.
    pub collider_revision: u64,
    /// Integer downsample factor the collider was last built at (`1` = exact).
    pub coarsen_k: u32,
    /// Inclusive global-cell box `[min, max]` the collider covers.
    pub collider_region: [[i64; 3]; 2],
    /// Mass input: bulk density, kg/m³ (per-cell material density is re-derived
    /// from the manifest on load).
    pub density_kg_m3: f32,
    /// Brick-coordinate bounds `[min, max]` if the volume is bounded.
    pub volume_bounds: Option<[[i64; 3]; 2]>,
}

/// How a stored brick's 32³ material layer is encoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrickPayload {
    /// Every cell holds this material id (air included — a modified-air
    /// tombstone is `Uniform(0)` with `edited = true`).
    Uniform(u16),
    /// A zstd frame over exactly `32768` little-endian `u16` material ids.
    DenseZstd(Vec<u8>),
}

/// One persisted brick. `edited` carries the modified-air tombstone flag so a
/// mined-out brick is never regenerated after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBrick {
    pub volume_id: u64,
    pub coord: [i64; 3],
    pub revision: u64,
    pub edited: bool,
    pub payload: BrickPayload,
}

/// A coherent, immutable saved simulation snapshot at one tick, plus the
/// journal cursor it is consistent with. Publishing this and its cursor in one
/// DB transaction is the checkpoint-publication contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Current versioned ecology bytes; empty when ecology is not configured.
    pub vegetation: Vec<u8>,
    /// Stacked-column water (schema 3 DTO) in its own additive checkpoint row.
    pub column_water: Option<spall_protocol::ColumnWaterState>,
    /// Authoritative weather clock; None when weather is not configured.
    pub weather: Option<spall_protocol::WeatherState>,
    /// Versioned canonical thin snow; transient footprints are excluded by the owner.
    /// None when the caller has no snow state.
    pub snow: Option<Vec<u8>>,
    pub tick: u64,
    /// Highest journal sequence this checkpoint already includes. Recovery
    /// replays the durable journal suffix strictly after this.
    pub journal_cursor: u64,
    /// Canonical topology hash of the whole world at `tick`, for verification.
    pub world_hash: [u8; 32],
    pub meta: StoredWorldMeta,
    pub bodies: Vec<StoredBody>,
    pub bricks: Vec<StoredBrick>,
}

/// One ordered authoritative journal record between checkpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub seq: u64,
    pub tick: u64,
    pub payload: JournalPayload,
}

impl JournalRecord {
    /// Validate typed transport and its row metadata before publication/recovery.
    pub fn validate(&self) -> Result<(), DtoError> {
        self.payload.validate_auxiliary_bounds()?;
        if let JournalPayload::TopologyAttachments { transaction, .. } = &self.payload {
            let tx: TopologyTransaction = decode_saved_record(transaction)?;
            if tx.server_tick.get() != self.tick {
                return Err(DtoError::JournalBounds);
            }
        }
        Ok(())
    }
}

/// Game-owned durable event committed atomically with its world journal row.
/// The payload has its own game schema; the engine stores opaque bytes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRecord {
    pub event_id: [u8; 32],
    pub journal_seq: u64,
    pub payload: Vec<u8>,
}

/// The content of a [`JournalRecord`]. Topology and pose bytes are
/// `spall_protocol` wire records kept verbatim; `spall_store` never interprets
/// their semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JournalPayload {
    /// A committed transaction plus the participant body states required to
    /// recover it with correct ownership/frame even if earlier pose batches
    /// were lost (`docs/protocol.md`).
    Topology {
        /// `encode_control(&TopologyTransaction)` bytes.
        transaction: Vec<u8>,
        /// `encode_control(&MotionSnapshot)` bytes, one per participant body.
        participants: Vec<Vec<u8>>,
    },
    /// A periodic 20 Hz body pose batch.
    PoseBatch {
        /// `encode_control(&MotionSnapshot)` bytes.
        snapshots: Vec<Vec<u8>>,
    },
    /// One ordered parent transaction with its complete typed bulk attachment set.
    /// The existing protocol envelope validates coverage and cumulative work
    /// before nested payloads are exposed to recovery.
    TopologyAttachments {
        #[serde(deserialize_with = "decode_journal_control")]
        transaction: Vec<u8>,
        #[serde(deserialize_with = "decode_journal_participants")]
        participants: Vec<Vec<u8>>,
        #[serde(deserialize_with = "decode_attachment_bytes")]
        attachments: Vec<u8>,
    },
    /// Appended: canonical vegetation after this tick's topology commits.
    VegetationState(Vec<u8>),
    VegetationClock {
        time_ms: u64,
        credit_ms: u64,
    },
    /// Appended (ENG-167): changed column-water regions since the last water
    /// record, applied over the checkpoint's `column_water` on recovery.
    ColumnWaterDelta(spall_protocol::ColumnWaterDelta),
    /// Appended versioned weather state and explicit weather clock source.
    WeatherState(spall_protocol::WeatherState),
    /// Appended: exact owner-versioned tile/Y cohort state after geometry or
    /// exposure changes. Includes Q32 depth/credit; excludes temporary tracks.
    SnowTileState {
        world_epoch: u64,
        tile: [i32; 3],
        state: Vec<u8>,
    },
}

// Borrow the postcard byte slice and reject its length before allocating a copy.
fn decode_attachment_bytes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<u8>, D::Error> {
    let bytes: &'de [u8] = Deserialize::deserialize(deserializer)?;
    if bytes.len() > spall_protocol::MAX_TRANSACTION_ATTACHMENT_BYTES {
        return Err(serde::de::Error::custom(
            "attachment compressed-byte budget exceeded",
        ));
    }
    Ok(bytes.to_vec())
}

fn decode_journal_control<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    let bytes: &'de [u8] = Deserialize::deserialize(d)?;
    if bytes.len() > spall_protocol::limits::MAX_CONTROL_RECORD {
        return Err(serde::de::Error::custom(
            "journal control-byte budget exceeded",
        ));
    }
    Ok(bytes.to_vec())
}

fn decode_journal_participants<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<Vec<u8>>, D::Error> {
    struct Participants;
    impl<'de> serde::de::Visitor<'de> for Participants {
        type Value = Vec<Vec<u8>>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("bounded participant controls")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            if seq
                .size_hint()
                .is_some_and(|n| n > spall_protocol::limits::MAX_TRANSACTION_REFS)
            {
                return Err(serde::de::Error::custom(
                    "journal participant count exceeded",
                ));
            }
            let mut result = Vec::new();
            let mut remaining = spall_protocol::MAX_TRANSACTION_ATTACHMENT_BYTES;
            while let Some(bytes) = seq.next_element::<&'de [u8]>()? {
                if result.len() >= spall_protocol::limits::MAX_TRANSACTION_REFS
                    || bytes.len() > spall_protocol::limits::MAX_CONTROL_RECORD
                    || bytes.len() > remaining
                {
                    return Err(serde::de::Error::custom(
                        "journal participant budget exceeded",
                    ));
                }
                remaining -= bytes.len();
                result.push(bytes.to_vec());
            }
            Ok(result)
        }
    }
    d.deserialize_seq(Participants)
}

impl JournalPayload {
    /// The store checks the version/size envelope; simulation validates the
    /// canonical cohort contents while replaying this ordered journal entry.
    pub fn validate_auxiliary_bounds(&self) -> Result<(), DtoError> {
        if let Some(decoded) = self.as_topology_with_attachments() {
            decoded?;
        }
        if let Self::SnowTileState {
            world_epoch, state, ..
        } = self
            && (*world_epoch == 0
                || state.len() > 64 * 1024
                || !state.starts_with(&1u16.to_le_bytes()))
        {
            return Err(DtoError::SnowTileBounds);
        }
        Ok(())
    }
    /// Build a `Topology` payload from live protocol records.
    pub fn topology(
        transaction: &TopologyTransaction,
        participants: &[MotionSnapshot],
    ) -> Result<Self, DtoError> {
        Ok(Self::Topology {
            transaction: encode_control(transaction)?,
            participants: participants
                .iter()
                .map(encode_control)
                .collect::<Result<_, _>>()?,
        })
    }

    /// Encode the complete typed set against its exact ordered parent.
    pub fn topology_with_attachments(
        transaction: &TopologyTransaction,
        participants: &[MotionSnapshot],
        attachments: &spall_protocol::TransactionAttachmentSetV1,
    ) -> Result<Self, DtoError> {
        if participants.len() > spall_protocol::limits::MAX_TRANSACTION_REFS {
            return Err(DtoError::JournalBounds);
        }
        let attachments = attachments.encode_for_transaction(transaction)?;
        Ok(Self::TopologyAttachments {
            transaction: encode_control(transaction)?,
            participants: participants
                .iter()
                .map(encode_control)
                .collect::<Result<_, _>>()?,
            attachments,
        })
    }

    /// Decode and correlate all attachment families before handing off a candidate.
    #[allow(clippy::type_complexity)]
    pub fn as_topology_with_attachments(
        &self,
    ) -> Option<
        Result<
            (
                TopologyTransaction,
                Vec<MotionSnapshot>,
                spall_protocol::ValidatedTransactionAttachmentSetV1,
            ),
            DtoError,
        >,
    > {
        let Self::TopologyAttachments {
            transaction,
            participants,
            attachments,
        } = self
        else {
            return None;
        };
        Some((|| {
            if transaction.len() > spall_protocol::limits::MAX_CONTROL_RECORD
                || participants.len() > spall_protocol::limits::MAX_TRANSACTION_REFS
                || participants
                    .iter()
                    .any(|p| p.len() > spall_protocol::limits::MAX_CONTROL_RECORD)
                || participants
                    .iter()
                    .map(Vec::len)
                    .try_fold(0usize, usize::checked_add)
                    .is_none_or(|n| n > spall_protocol::MAX_TRANSACTION_ATTACHMENT_BYTES)
            {
                return Err(DtoError::JournalBounds);
            }
            let tx = decode_saved_record::<TopologyTransaction>(transaction)?;
            let parts = participants
                .iter()
                .map(|b| decode_saved_record::<MotionSnapshot>(b))
                .collect::<Result<Vec<_>, _>>()?;
            let set = spall_protocol::decode_for_transaction(attachments, &tx)?;
            Ok((tx, parts, set))
        })())
    }

    /// Build a `PoseBatch` payload from live protocol records.
    pub fn pose_batch(snapshots: &[MotionSnapshot]) -> Result<Self, DtoError> {
        Ok(Self::PoseBatch {
            snapshots: snapshots
                .iter()
                .map(encode_control)
                .collect::<Result<_, _>>()?,
        })
    }

    /// Decode a `Topology` payload back to protocol records. Returns `None` for
    /// a `PoseBatch`.
    pub fn as_topology(
        &self,
    ) -> Option<Result<(TopologyTransaction, Vec<MotionSnapshot>), DtoError>> {
        let Self::Topology {
            transaction,
            participants,
        } = self
        else {
            return None;
        };
        Some((|| {
            let tx = decode_saved_record::<TopologyTransaction>(transaction)?;
            let parts = participants
                .iter()
                .map(|b| decode_saved_record::<MotionSnapshot>(b))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((tx, parts))
        })())
    }

    /// Decode a `PoseBatch` payload. Returns `None` for a `Topology` record.
    pub fn as_pose_batch(&self) -> Option<Result<Vec<MotionSnapshot>, DtoError>> {
        let Self::PoseBatch { snapshots } = self else {
            return None;
        };
        Some(
            snapshots
                .iter()
                .map(|b| decode_saved_record::<MotionSnapshot>(b).map_err(DtoError::from))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{GlobalCell, MaterialId, Tick, TransactionId, VolumeId};
    use spall_protocol::{ControlSeq, TopologyOp};

    fn a_tx() -> TopologyTransaction {
        TopologyTransaction {
            transaction_id: TransactionId::new(7).unwrap(),
            server_tick: Tick(9),
            control_seq: ControlSeq(3),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 4,
                material: MaterialId::AIR,
            }],
            result_hashes: vec![],
        }
    }

    #[test]
    fn journal_topology_round_trips_through_wire_bytes() {
        let payload = JournalPayload::topology(&a_tx(), &[]).unwrap();
        let (tx, parts) = payload.as_topology().unwrap().unwrap();
        assert_eq!(tx, a_tx());
        assert!(parts.is_empty());
        assert!(payload.as_pose_batch().is_none());
    }

    #[test]
    fn saved_topology_motion_and_bulk_rows_reject_noncurrent_protocol_versions() {
        let motion = MotionSnapshot {
            server_tick: Tick(9),
            snapshot_seq: spall_protocol::SnapshotSeq(1),
            acked_input: spall_protocol::InputSeq(0),
            body: spall_core::EntityId::new(1).unwrap(),
            topology_revision: spall_core::Revision(1),
            pose: spall_core::Pose {
                translation_m: [0.0; 3],
                rotation: spall_core::QuantizedQuat {
                    x: 0,
                    y: 0,
                    z: 0,
                    w: 32767,
                },
            },
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: true,
        };
        let current_tx = encode_control(&a_tx()).unwrap();
        let current_motion = encode_control(&motion).unwrap();
        for version in [
            2u16,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            12,
            13,
            spall_protocol::WIRE_SCHEMA_VERSION + 1,
        ] {
            let mut tx = current_tx.clone();
            let mut pose = current_motion.clone();
            tx[..2].copy_from_slice(&version.to_le_bytes());
            pose[..2].copy_from_slice(&version.to_le_bytes());
            let topology = JournalPayload::Topology {
                transaction: tx.clone(),
                participants: vec![pose.clone()],
            };
            assert!(
                topology.as_topology().unwrap().is_err(),
                "topology schema {version}"
            );
            let batch = JournalPayload::PoseBatch {
                snapshots: vec![pose.clone()],
            };
            assert!(
                batch.as_pose_batch().unwrap().is_err(),
                "motion schema {version}"
            );
            let bulk = JournalPayload::TopologyAttachments {
                transaction: tx,
                participants: vec![pose],
                attachments: geometry_set().encode_for_transaction(&bulk_tx()).unwrap(),
            };
            assert!(
                bulk.as_topology_with_attachments().unwrap().is_err(),
                "bulk topology schema {version}"
            );
        }
        // Current records remain usable through every journal accessor.
        let topology = JournalPayload::Topology {
            transaction: current_tx.clone(),
            participants: vec![current_motion.clone()],
        };
        assert_eq!(
            topology.as_topology().unwrap().unwrap(),
            (a_tx(), vec![motion])
        );
        let poses = JournalPayload::PoseBatch {
            snapshots: vec![current_motion.clone()],
        };
        assert_eq!(poses.as_pose_batch().unwrap().unwrap(), vec![motion]);
        let world = a_baseline_world();
        let bulk =
            JournalPayload::topology_with_attachments(&bulk_tx(), &[motion], &geometry_set())
                .unwrap();
        let (tx, parts, set) = bulk.as_topology_with_attachments().unwrap().unwrap();
        assert_eq!(tx, bulk_tx());
        assert_eq!(parts, vec![motion]);
        assert_eq!(
            set.get(77),
            Some(&spall_protocol::ValidatedTransactionAttachmentV1::Geometry(
                world
            ))
        );
    }
    fn a_baseline_world() -> BaselineWorld {
        use spall_protocol::baseline::{
            BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume,
        };
        BaselineWorld {
            schema: spall_protocol::WIRE_SCHEMA_VERSION,
            checkpoint_tick: 9,
            volumes: vec![BaselineVolume {
                volume_id: VolumeId::new(2).unwrap(),
                cell_size_code: 2,
                owner: BaselineOwner::Body(spall_core::EntityId::new(5).unwrap()),
                bounds: Some([[0, 0, 0], [0, 0, 0]]),
                bricks: vec![BaselineBrick {
                    coord: [0, 0, 0],
                    revision: 2,
                    edited: true,
                    cells: BaselineCells::Uniform(1),
                }],
            }],
        }
    }

    fn bulk_tx() -> TopologyTransaction {
        let mut tx = a_tx();
        tx.ops = vec![TopologyOp::SplitOffBulkBaseline {
            source: VolumeId::new(1).unwrap(),
            child: VolumeId::new(2).unwrap(),
            child_entity: spall_core::EntityId::new(5).unwrap(),
            transfer_id: 77,
        }];
        tx
    }
    fn geometry_set() -> spall_protocol::TransactionAttachmentSetV1 {
        spall_protocol::TransactionAttachmentSetV1 {
            schema: spall_protocol::WIRE_SCHEMA_VERSION,
            transaction_id: bulk_tx().transaction_id,
            attachments: vec![spall_protocol::TransactionAttachmentV1::Geometry {
                transfer_id: 77,
                world: a_baseline_world(),
            }],
        }
    }

    #[test]
    fn typed_geometry_journal_roundtrips_and_binds_parent_and_complete_set() {
        let tx = bulk_tx();
        let payload = JournalPayload::topology_with_attachments(&tx, &[], &geometry_set()).unwrap();
        let bytes = encode(&payload).unwrap();
        let decoded: JournalPayload = decode(&bytes).unwrap();
        assert_eq!(decoded, payload);
        let (parent, parts, set) = decoded.as_topology_with_attachments().unwrap().unwrap();
        assert_eq!(parent, tx);
        assert!(parts.is_empty());
        assert!(set.matches_transaction(&tx));
        assert_eq!(
            set.get(77),
            Some(&spall_protocol::ValidatedTransactionAttachmentV1::Geometry(
                a_baseline_world()
            ))
        );
        assert!(payload.as_topology().is_none());
        assert!(payload.as_pose_batch().is_none());
        let mut missing = geometry_set();
        missing.attachments.clear();
        assert!(JournalPayload::topology_with_attachments(&tx, &[], &missing).is_err());
        let mut extra = geometry_set();
        extra.attachments.push(extra.attachments[0].clone());
        assert!(JournalPayload::topology_with_attachments(&tx, &[], &extra).is_err());
        let mut wrong = tx.clone();
        wrong.transaction_id = TransactionId::new(8).unwrap();
        assert!(JournalPayload::topology_with_attachments(&wrong, &[], &geometry_set()).is_err());
        for mutate in 0..3 {
            let mut bad = payload.clone();
            let JournalPayload::TopologyAttachments {
                transaction,
                attachments,
                ..
            } = &mut bad
            else {
                unreachable!()
            };
            match mutate {
                0 => *transaction = encode_control(&wrong).unwrap(),
                1 => {
                    attachments.pop();
                }
                _ => attachments.push(0),
            }
            assert!(bad.as_topology_with_attachments().unwrap().is_err());
            assert!(bad.validate_auxiliary_bounds().is_err());
        }
    }

    #[test]
    fn typed_journal_controls_are_bounded_before_copying_or_participant_bodies() {
        let payload = JournalPayload::TopologyAttachments {
            transaction: vec![0; spall_protocol::limits::MAX_CONTROL_RECORD + 1],
            participants: vec![],
            attachments: vec![],
        };
        let bytes = encode(&payload).unwrap();
        assert!(decode::<JournalPayload>(&bytes).is_err());
        struct Bodies;
        impl Iterator for Bodies {
            type Item = &'static [u8];
            fn next(&mut self) -> Option<Self::Item> {
                panic!("participant body read before count admission")
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                let n = spall_protocol::limits::MAX_TRANSACTION_REFS + 1;
                (n, Some(n))
            }
        }
        let d = serde::de::value::SeqDeserializer::<_, serde::de::value::Error>::new(Bodies);
        let error = decode_journal_participants(d).unwrap_err();
        assert!(error.to_string().contains("participant count exceeded"));
    }

    #[test]
    fn checkpoint_postcard_round_trips() {
        let cp = Checkpoint {
            vegetation: Vec::new(),
            column_water: None,
            weather: None,
            snow: None,
            tick: 120,
            journal_cursor: 5,
            world_hash: [3; 32],
            meta: StoredWorldMeta {
                store_schema_version: STORE_SCHEMA_VERSION,
                world_id: 42,
                seed: 1,
                generator_version: 1,
                material_manifest_hash: [9; 32],
                cell_size_codes: vec![2],
                next_entity: 4,
                next_volume: 6,
                next_transaction: 8,
                next_journal_seq: 6,
                integer_brush_version: 1,
                structure_graph_version: 1,
                topology_hash_version: 1,
            },
            bodies: vec![],
            bricks: vec![StoredBrick {
                volume_id: 1,
                coord: [-1, 0, 2],
                revision: 3,
                edited: true,
                payload: BrickPayload::Uniform(0),
            }],
        };
        let bytes = encode(&cp).unwrap();
        assert_eq!(decode::<Checkpoint>(&bytes).unwrap(), cp);
    }
}
