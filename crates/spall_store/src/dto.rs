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

use spall_protocol::baseline::{BaselineDecodeError, BaselineWorld};
use spall_protocol::{
    CodecError, MotionSnapshot, TopologyTransaction, decode_control, encode_control,
};

/// Version of the on-disk save schema. Bumped on any change to the types in
/// this module. A database written by a newer schema is rejected on open
/// without modification (`docs/protocol.md`: "Unknown newer schemas are
/// rejected without modifying the database").
///
/// **Deliberately kept at 1 for the T17-increment-2 `JournalPayload::TopologyBulkSplit`
/// addition (ENG-64):** appending a `postcard` enum variant leaves every existing
/// `Topology` / `PoseBatch` row fully decodable, this repo has no deployed older
/// binary, and no migration harness exists — bumping would reject every existing
/// world database on open with `SchemaTooOld`.
pub const STORE_SCHEMA_VERSION: u32 = 1;

/// Largest accepted stored brick payload, compressed. `docs/protocol.md` caps a
/// material-only brick record at 256 KiB decompressed for 64 KiB of real
/// payload; the compressed frame is always smaller, and this bounds a hostile
/// or corrupt row before allocation.
pub const MAX_STORED_BRICK_BYTES: usize = 256 * 1024;

/// Errors converting between save records and their encoded forms.
#[derive(Debug, thiserror::Error)]
pub enum DtoError {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("protocol codec: {0}")]
    Codec(#[from] CodecError),
    #[error("bulk split baseline: {0}")]
    Baseline(#[from] BaselineDecodeError),
}

/// Encode a save record to its postcard bytes.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, DtoError> {
    Ok(postcard::to_stdvec(value)?)
}

/// Decode a save record from postcard bytes.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, DtoError> {
    Ok(postcard::from_bytes(bytes)?)
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
    /// T17 increment 2 (ENG-64): a giant split whose geometry did not fit even a
    /// compressed inline op blob. `transaction` decodes to a
    /// `TopologyTransaction` whose ops are `SplitOffBulkBaseline` /
    /// `SourcePatchBulkBaseline` markers only; `baseline` is
    /// `BaselineWorld::encode_compressed()` — every child volume plus the
    /// source's post-cut affected bricks — replayed exactly like the bulk
    /// transfer a live replica assembles. **Appended variant:** older `Topology`
    /// / `PoseBatch` rows keep their discriminant indices and still decode.
    TopologyBulkSplit {
        /// `encode_control(&TopologyTransaction)` bytes (marker ops only, small).
        transaction: Vec<u8>,
        /// `encode_control(&MotionSnapshot)` bytes, one per participant body.
        participants: Vec<Vec<u8>>,
        /// `BaselineWorld::encode_compressed()` bytes (zstd).
        baseline: Vec<u8>,
    },
}

impl JournalPayload {
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

    /// Build a `TopologyBulkSplit` payload: the marker transaction, its
    /// participants, and the out-of-band `BaselineWorld` (T17 increment 2).
    pub fn topology_bulk_split(
        transaction: &TopologyTransaction,
        participants: &[MotionSnapshot],
        baseline: &BaselineWorld,
    ) -> Result<Self, DtoError> {
        Ok(Self::TopologyBulkSplit {
            transaction: encode_control(transaction)?,
            participants: participants
                .iter()
                .map(encode_control)
                .collect::<Result<_, _>>()?,
            baseline: baseline.encode_compressed(),
        })
    }

    /// Decode a `TopologyBulkSplit` payload. Returns `None` for any other
    /// variant.
    #[allow(clippy::type_complexity)]
    pub fn as_topology_bulk_split(
        &self,
    ) -> Option<Result<(TopologyTransaction, Vec<MotionSnapshot>, BaselineWorld), DtoError>> {
        let Self::TopologyBulkSplit {
            transaction,
            participants,
            baseline,
        } = self
        else {
            return None;
        };
        Some((|| {
            let tx = decode_control::<TopologyTransaction>(transaction)?;
            let parts = participants
                .iter()
                .map(|b| decode_control::<MotionSnapshot>(b))
                .collect::<Result<Vec<_>, _>>()?;
            let world = BaselineWorld::decode_compressed(baseline)?;
            Ok((tx, parts, world))
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
            let tx = decode_control::<TopologyTransaction>(transaction)?;
            let parts = participants
                .iter()
                .map(|b| decode_control::<MotionSnapshot>(b))
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
                .map(|b| decode_control::<MotionSnapshot>(b).map_err(DtoError::from))
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

    fn a_baseline_world() -> BaselineWorld {
        use spall_protocol::baseline::{
            BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume,
        };
        BaselineWorld {
            schema: spall_protocol::BASELINE_WORLD_SCHEMA,
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

    #[test]
    fn journal_topology_bulk_split_round_trips() {
        let world = a_baseline_world();
        let payload = JournalPayload::topology_bulk_split(&a_tx(), &[], &world).unwrap();
        let (tx, parts, w) = payload.as_topology_bulk_split().unwrap().unwrap();
        assert_eq!(tx, a_tx());
        assert!(parts.is_empty());
        assert_eq!(w, world);
        // The variant is disjoint from the other accessors.
        assert!(payload.as_topology().is_none());
        assert!(payload.as_pose_batch().is_none());
    }

    #[test]
    fn appending_the_bulk_split_variant_keeps_old_rows_decodable() {
        // A `Topology` row encoded before the new variant existed still round
        // trips (postcard variant-append is one-way compatible; keep
        // STORE_SCHEMA_VERSION at 1).
        let payload = JournalPayload::Topology {
            transaction: encode_control(&a_tx()).unwrap(),
            participants: vec![],
        };
        let bytes = encode(&payload).unwrap();
        let back: JournalPayload = decode(&bytes).unwrap();
        assert_eq!(back, payload);
        assert_eq!(STORE_SCHEMA_VERSION, 1);
    }

    #[test]
    fn checkpoint_postcard_round_trips() {
        let cp = Checkpoint {
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
