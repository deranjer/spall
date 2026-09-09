//! Versioned wire records.
//!
//! These are the DTOs from `docs/protocol.md` "Minimal record families". They
//! use explicit numeric sizes and tags; they never serialize library handles,
//! ECS entities, pointers, `usize`, or raw Rust structs. Each record carries a
//! [`WireTag`] and a `SCHEMA_VERSION`, and [`Record::validate`] rejects
//! out-of-range counts and non-finite floats *without* allocating from the
//! untrusted numbers.

use serde::{Deserialize, Serialize};
use spall_core::{
    BrickCoord, EntityId, GlobalCell, JournalSeq, MaterialId, MaterialManifest, Pose, Revision,
    SphereBrush, Tick, TransactionId, VolumeId,
};

use crate::canonical::Hash32;
use crate::limits::{
    self, MAX_BASELINE_PARTS, MAX_BASELINE_REGIONS, MAX_BULK_PART, MAX_CELL_RUN_LEN,
    MAX_REDUNDANT_INPUTS, MAX_SPLIT_BASELINE_BLOB, MAX_TRANSACTION_OPS, MAX_TRANSACTION_REFS,
    SizeLimitError,
};

/// Schema version stamped into every encoded record header.
pub const WIRE_SCHEMA_VERSION: u16 = 1;

/// Stable per-family wire tag. The `u16` discriminant is part of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u16)]
pub enum WireTag {
    InputFrame = 1,
    ActionRequest = 2,
    ActionStatus = 3,
    TopologyTransaction = 4,
    MotionSnapshot = 5,
    BaselineBegin = 6,
    BaselinePart = 7,
    BaselineEnd = 8,
    BaselineAck = 9,
    RepairRequest = 10,
    DurableThrough = 11,
    Handshake = 12,
}

impl WireTag {
    pub const fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::InputFrame,
            2 => Self::ActionRequest,
            3 => Self::ActionStatus,
            4 => Self::TopologyTransaction,
            5 => Self::MotionSnapshot,
            6 => Self::BaselineBegin,
            7 => Self::BaselinePart,
            8 => Self::BaselineEnd,
            9 => Self::BaselineAck,
            10 => Self::RepairRequest,
            11 => Self::DurableThrough,
            12 => Self::Handshake,
            _ => return None,
        })
    }

    pub const fn to_u16(self) -> u16 {
        self as u16
    }
}

/// Why a decoded record failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Size(#[from] SizeLimitError),
    #[error("field {0} is not finite")]
    NonFinite(&'static str),
    #[error("field {field} is out of range: {detail}")]
    OutOfRange {
        field: &'static str,
        detail: &'static str,
    },
    #[error("material id {0} is not defined by the manifest")]
    UnknownMaterial(u16),
    #[error("record is internally inconsistent: {0}")]
    Inconsistent(&'static str),
}

/// Common behaviour for every wire record.
pub trait Record: Serialize + for<'de> Deserialize<'de> + Sized {
    const TAG: WireTag;
    const SCHEMA_VERSION: u16 = WIRE_SCHEMA_VERSION;

    /// Self-contained validation: counts within limits, floats finite, internal
    /// references consistent. Does not consult a material manifest.
    fn validate(&self) -> Result<(), RecordError>;
}

// --- small shared newtypes ----------------------------------------------------

macro_rules! wire_u64 {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        pub struct $name(pub u64);
    };
}

wire_u64!(
    /// Client-chosen unique id for an [`ActionRequest`].
    RequestId
);
wire_u64!(
    /// Monotonic client input sequence number.
    InputSeq
);
wire_u64!(
    /// Monotonic per-connection motion snapshot sequence number.
    SnapshotSeq
);
wire_u64!(
    /// Identity of one baseline transfer.
    TransferId
);
wire_u64!(
    /// Interest-set epoch; bumped when a client's relevance region changes.
    InterestEpoch
);
wire_u64!(
    /// Per-connection control-stream sequence (mirrors `session::StreamSeq` but
    /// travels inside records).
    ControlSeq
);

fn finite3(v: &[f32; 3], field: &'static str) -> Result<(), RecordError> {
    if v.iter().all(|c| c.is_finite()) {
        Ok(())
    } else {
        Err(RecordError::NonFinite(field))
    }
}

fn finite3_f64(v: &[f64; 3], field: &'static str) -> Result<(), RecordError> {
    if v.iter().all(|c| c.is_finite()) {
        Ok(())
    } else {
        Err(RecordError::NonFinite(field))
    }
}

fn unit_axis(v: &[f32; 3], field: &'static str) -> Result<(), RecordError> {
    finite3(v, field)?;
    if v.iter().all(|c| (-1.0..=1.0).contains(c)) {
        Ok(())
    } else {
        Err(RecordError::OutOfRange {
            field,
            detail: "each axis must be within -1.0..=1.0",
        })
    }
}

// --- InputFrame -------------------------------------------------------------

/// One redundant copy of a recent input, carried inside [`InputFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RecentInput {
    pub input_seq: InputSeq,
    pub movement: [f32; 3],
    pub view_dir: [f32; 3],
    pub buttons: u32,
}

/// `InputFrame`: player movement intent. Sent as a datagram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFrame {
    pub session: crate::session::SessionId,
    pub player: EntityId,
    pub input_seq: InputSeq,
    pub intended_tick: Tick,
    pub movement: [f32; 3],
    pub view_dir: [f32; 3],
    pub buttons: u32,
    /// Up to [`limits::MAX_REDUNDANT_INPUTS`] recent frames, newest first.
    pub recent: Vec<RecentInput>,
}

impl Record for InputFrame {
    const TAG: WireTag = WireTag::InputFrame;

    fn validate(&self) -> Result<(), RecordError> {
        limits::check_count("InputFrame.recent", self.recent.len(), MAX_REDUNDANT_INPUTS)?;
        unit_axis(&self.movement, "InputFrame.movement")?;
        finite3(&self.view_dir, "InputFrame.view_dir")?;
        for r in &self.recent {
            unit_axis(&r.movement, "InputFrame.recent.movement")?;
            finite3(&r.view_dir, "InputFrame.recent.view_dir")?;
        }
        Ok(())
    }
}

// --- ActionRequest / ActionStatus ----------------------------------------

/// The tool action a client is requesting. The server re-derives the actual
/// affected cells; fields here are only a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ActionKind {
    /// Remove material with a sphere brush.
    Cut = 0,
    /// Add material with a sphere brush.
    Place = 1,
}

/// What the client claims it is aiming at.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ClaimedTarget {
    Terrain,
    Body(EntityId),
}

/// `ActionRequest`: an edge-triggered tool use. Reliable and deduplicated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub request_id: RequestId,
    pub input_seq: InputSeq,
    pub action: ActionKind,
    pub tool: u16,
    pub aim_origin_m: [f64; 3],
    pub aim_dir: [f32; 3],
    pub claimed_target: ClaimedTarget,
    /// The client's proposed brush, in the target volume's local fixed-point
    /// units. The server may replace it.
    pub claimed_brush: SphereBrush,
}

impl Record for ActionRequest {
    const TAG: WireTag = WireTag::ActionRequest;

    fn validate(&self) -> Result<(), RecordError> {
        finite3_f64(&self.aim_origin_m, "ActionRequest.aim_origin_m")?;
        finite3(&self.aim_dir, "ActionRequest.aim_dir")?;
        if self.aim_dir.iter().all(|c| *c == 0.0) {
            return Err(RecordError::OutOfRange {
                field: "ActionRequest.aim_dir",
                detail: "direction must be non-zero",
            });
        }
        Ok(())
    }
}

/// Outcome of an [`ActionRequest`]. `Queued` is not `Committed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ActionOutcome {
    Queued,
    Rejected { reason: String },
    Committed { transaction: TransactionId },
}

/// `ActionStatus`: server's answer for one request id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionStatus {
    pub request_id: RequestId,
    pub outcome: ActionOutcome,
}

impl Record for ActionStatus {
    const TAG: WireTag = WireTag::ActionStatus;

    fn validate(&self) -> Result<(), RecordError> {
        if let ActionOutcome::Rejected { reason } = &self.outcome {
            limits::check_count("ActionStatus.reason", reason.len(), 1024)?;
        }
        Ok(())
    }
}

// --- TopologyTransaction ------------------------------------------------------

/// A `(volume, brick) -> revision` pair used in before/after sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrickRevision {
    pub volume: VolumeId,
    pub coord: BrickCoord,
    pub revision: Revision,
}

/// A `(volume) -> hash` pair: the canonical result hash for one affected volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeHash {
    pub volume: VolumeId,
    pub hash: Hash32,
}

/// One ordered operation inside a transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TopologyOp {
    /// Deterministic integer sphere brush against a matching source revision.
    IntegerBrush {
        volume: VolumeId,
        brush: SphereBrush,
        material: MaterialId,
    },
    /// Contiguous +X run in the volume's cell coordinates, with fixed Y/Z.
    /// The last X coordinate is `start.x + len - 1` and must fit `i64`.
    CellRun {
        volume: VolumeId,
        start: GlobalCell,
        len: u32,
        material: MaterialId,
    },
    /// Server-decided split: `child` geometry detaches from `source`.
    SplitOff {
        source: VolumeId,
        child: VolumeId,
        child_entity: EntityId,
    },
    /// A split whose child geometry is too large to encode as inline
    /// [`TopologyOp::CellRun`]s (T17): `blob` is the zstd-compressed postcard of
    /// a [`crate::baseline::BaselineVolume`] holding the whole child volume, at
    /// its authoritative brick revisions. Replaces the `SplitOff` marker **and**
    /// its child-fill runs. Bounded by
    /// [`crate::limits::MAX_SPLIT_BASELINE_BLOB`].
    SplitOffBaseline {
        source: VolumeId,
        child: VolumeId,
        child_entity: EntityId,
        blob: Vec<u8>,
    },
    /// The source side of an oversized split (T17): `blob` is the
    /// zstd-compressed postcard of a [`crate::baseline::BaselineVolume`] holding
    /// the source volume's post-cut **affected** bricks, at their authoritative
    /// revisions. Replaces the inline source-removal `CellRun`s.
    SourcePatchBaseline { source: VolumeId, blob: Vec<u8> },
}

/// `TopologyTransaction`: the authoritative record of one committed edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopologyTransaction {
    pub transaction_id: TransactionId,
    pub server_tick: Tick,
    pub control_seq: ControlSeq,
    pub algorithm_version: u32,
    pub dependencies: Vec<TransactionId>,
    pub before: Vec<BrickRevision>,
    pub after: Vec<BrickRevision>,
    pub ops: Vec<TopologyOp>,
    pub result_hashes: Vec<VolumeHash>,
}

impl Record for TopologyTransaction {
    const TAG: WireTag = WireTag::TopologyTransaction;

    fn validate(&self) -> Result<(), RecordError> {
        limits::check_count(
            "TopologyTransaction.dependencies",
            self.dependencies.len(),
            MAX_TRANSACTION_REFS,
        )?;
        limits::check_count(
            "TopologyTransaction.before",
            self.before.len(),
            MAX_TRANSACTION_REFS,
        )?;
        limits::check_count(
            "TopologyTransaction.after",
            self.after.len(),
            MAX_TRANSACTION_REFS,
        )?;
        limits::check_count(
            "TopologyTransaction.ops",
            self.ops.len(),
            MAX_TRANSACTION_OPS,
        )?;
        limits::check_count(
            "TopologyTransaction.result_hashes",
            self.result_hashes.len(),
            MAX_TRANSACTION_REFS,
        )?;
        if self.ops.is_empty() {
            return Err(RecordError::Inconsistent("transaction has no operations"));
        }
        for op in &self.ops {
            if let TopologyOp::CellRun { start, len, .. } = op
                && start
                    .x
                    .checked_add(i64::from(*len).saturating_sub(1))
                    .is_none()
            {
                return Err(RecordError::OutOfRange {
                    field: "TopologyOp.CellRun.start",
                    detail: "last +X cell must fit i64",
                });
            }
            if let TopologyOp::CellRun { len, .. } = op
                && (*len == 0 || *len > MAX_CELL_RUN_LEN)
            {
                return Err(RecordError::OutOfRange {
                    field: "TopologyOp.CellRun.len",
                    detail: "run length must be 1..=MAX_CELL_RUN_LEN",
                });
            }
            let blob = match op {
                TopologyOp::SplitOffBaseline { blob, .. }
                | TopologyOp::SourcePatchBaseline { blob, .. } => Some(blob),
                _ => None,
            };
            if let Some(blob) = blob
                && (blob.is_empty() || blob.len() > MAX_SPLIT_BASELINE_BLOB)
            {
                return Err(RecordError::OutOfRange {
                    field: "TopologyOp split baseline blob",
                    detail: "blob length must be 1..=MAX_SPLIT_BASELINE_BLOB",
                });
            }
        }
        Ok(())
    }
}

impl TopologyTransaction {
    /// Extra validation that needs the world manifest: every material id
    /// referenced by an op must be defined (or be air).
    pub fn validate_against(&self, manifest: &MaterialManifest) -> Result<(), RecordError> {
        self.validate()?;
        for op in &self.ops {
            let material = match op {
                TopologyOp::IntegerBrush { material, .. } => *material,
                TopologyOp::CellRun { material, .. } => *material,
                TopologyOp::SplitOff { .. } => continue,
                // A split baseline blob carries whole bricks: decode it and
                // check every material it names against the manifest.
                TopologyOp::SplitOffBaseline { blob, .. }
                | TopologyOp::SourcePatchBaseline { blob, .. } => {
                    let volume =
                        crate::baseline::BaselineVolume::decode_compressed(blob).map_err(|_| {
                            RecordError::Inconsistent("split baseline op blob failed to decode")
                        })?;
                    for id in volume.material_ids() {
                        if !id.is_air() && !manifest.contains(id) {
                            return Err(RecordError::UnknownMaterial(id.raw()));
                        }
                    }
                    continue;
                }
            };
            if !material.is_air() && !manifest.contains(material) {
                return Err(RecordError::UnknownMaterial(material.raw()));
            }
        }
        Ok(())
    }
}

// --- MotionSnapshot ---------------------------------------------------------

/// `MotionSnapshot`: approximate body/player motion. Sent as a datagram.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MotionSnapshot {
    pub server_tick: Tick,
    pub snapshot_seq: SnapshotSeq,
    pub acked_input: InputSeq,
    pub body: EntityId,
    pub topology_revision: Revision,
    pub pose: Pose,
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    pub sleeping: bool,
}

impl Record for MotionSnapshot {
    const TAG: WireTag = WireTag::MotionSnapshot;

    fn validate(&self) -> Result<(), RecordError> {
        self.pose
            .checked()
            .map_err(|_| RecordError::NonFinite("MotionSnapshot.pose.translation"))?;
        self.pose
            .rotation
            .to_unit()
            .map_err(|_| RecordError::OutOfRange {
                field: "MotionSnapshot.pose.rotation",
                detail: "quaternion has zero magnitude",
            })?;
        finite3(&self.linear_velocity, "MotionSnapshot.linear_velocity")?;
        finite3(&self.angular_velocity, "MotionSnapshot.angular_velocity")?;
        Ok(())
    }
}

// --- Baseline family -------------------------------------------------------

/// One entry in a baseline manifest: a region / volume the transfer covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineRegion {
    pub volume: VolumeId,
    pub min_brick: BrickCoord,
    pub max_brick: BrickCoord,
    pub revision: Revision,
}

/// `BaselineBegin`: opens a late-join / interest transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineBegin {
    pub transfer_id: TransferId,
    pub interest_epoch: InterestEpoch,
    pub checkpoint_tick: Tick,
    pub journal_cursor: JournalSeq,
    pub world_version: u32,
    pub content_version: u32,
    pub total_bytes: u64,
    pub part_count: u32,
    pub regions: Vec<BaselineRegion>,
}

impl Record for BaselineBegin {
    const TAG: WireTag = WireTag::BaselineBegin;

    fn validate(&self) -> Result<(), RecordError> {
        limits::check_count(
            "BaselineBegin.regions",
            self.regions.len(),
            MAX_BASELINE_REGIONS,
        )?;
        if self.part_count as usize > MAX_BASELINE_PARTS {
            return Err(SizeLimitError::count(
                "BaselineBegin.part_count",
                self.part_count as usize,
                MAX_BASELINE_PARTS,
            )
            .into());
        }
        if self.total_bytes > limits::MAX_ASSEMBLED_TRANSFER as u64 {
            return Err(SizeLimitError::bytes(
                "BaselineBegin.total_bytes",
                self.total_bytes as usize,
                limits::MAX_ASSEMBLED_TRANSFER,
            )
            .into());
        }
        for r in &self.regions {
            if r.min_brick.x > r.max_brick.x
                || r.min_brick.y > r.max_brick.y
                || r.min_brick.z > r.max_brick.z
            {
                return Err(RecordError::Inconsistent("baseline region min exceeds max"));
            }
        }
        Ok(())
    }
}

/// `BaselinePart`: one hashed chunk of a baseline transfer. Sent on a bulk
/// stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselinePart {
    pub transfer_id: TransferId,
    pub part_index: u32,
    pub part_hash: Hash32,
    pub payload: Vec<u8>,
}

impl Record for BaselinePart {
    const TAG: WireTag = WireTag::BaselinePart;

    fn validate(&self) -> Result<(), RecordError> {
        if self.payload.len() > MAX_BULK_PART {
            return Err(SizeLimitError::bytes(
                "BaselinePart.payload",
                self.payload.len(),
                MAX_BULK_PART,
            )
            .into());
        }
        if self.payload.is_empty() {
            return Err(RecordError::Inconsistent("baseline part is empty"));
        }
        Ok(())
    }
}

/// `BaselineEnd`: closes a transfer and states the assembled hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineEnd {
    pub transfer_id: TransferId,
    pub assembled_hash: Hash32,
    pub journal_cursor: JournalSeq,
}

impl Record for BaselineEnd {
    const TAG: WireTag = WireTag::BaselineEnd;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

/// `BaselineAck`: client confirms an installed transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineAck {
    pub transfer_id: TransferId,
    pub verified_manifest_hash: Hash32,
    pub installed_cursor: JournalSeq,
}

impl Record for BaselineAck {
    const TAG: WireTag = WireTag::BaselineAck;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

// --- RepairRequest / DurableThrough -------------------------------------------

/// What a [`RepairRequest`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepairKey {
    Brick { volume: VolumeId, coord: BrickCoord },
    Body { entity: EntityId },
}

/// `RepairRequest`: rate-limited request to re-send authoritative state whose
/// revision/hash did not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairRequest {
    pub key: RepairKey,
    pub expected_revision: Revision,
    pub current_revision: Revision,
    pub expected_hash: Hash32,
    pub current_hash: Hash32,
}

impl Record for RepairRequest {
    const TAG: WireTag = WireTag::RepairRequest;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

/// `DurableThrough`: highest contiguous journal sequence flushed to durable
/// storage. Distinct from the simulation commit cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableThrough {
    pub journal_seq: JournalSeq,
}

impl Record for DurableThrough {
    const TAG: WireTag = WireTag::DurableThrough;

    fn validate(&self) -> Result<(), RecordError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_core::{BrushPoint, QuantizedQuat};

    fn a_pose() -> Pose {
        Pose {
            translation_m: [1.0, 2.0, 3.0],
            rotation: QuantizedQuat::from_unit(0.0, 0.0, 0.0, 1.0).unwrap(),
        }
    }

    #[test]
    fn input_frame_rejects_too_many_redundant_and_bad_axes() {
        let base = RecentInput {
            input_seq: InputSeq(1),
            movement: [0.0; 3],
            view_dir: [0.0, 0.0, 1.0],
            buttons: 0,
        };
        let mut frame = InputFrame {
            session: crate::session::SessionId::from_parts(crate::session::SlotId(1), 1),
            player: EntityId::new(1).unwrap(),
            input_seq: InputSeq(10),
            intended_tick: Tick(100),
            movement: [0.0, 0.0, 0.0],
            view_dir: [0.0, 0.0, 1.0],
            buttons: 0,
            recent: vec![base; 3],
        };
        assert!(frame.validate().is_ok());

        frame.recent.push(base);
        assert!(matches!(frame.validate(), Err(RecordError::Size(_))));

        frame.recent.truncate(1);
        frame.movement = [2.0, 0.0, 0.0];
        assert!(matches!(
            frame.validate(),
            Err(RecordError::OutOfRange { .. })
        ));

        frame.movement = [0.0, 0.0, 0.0];
        frame.view_dir = [f32::NAN, 0.0, 0.0];
        assert!(matches!(frame.validate(), Err(RecordError::NonFinite(_))));
    }

    #[test]
    fn motion_snapshot_rejects_non_finite_pose() {
        let mut snap = MotionSnapshot {
            server_tick: Tick(1),
            snapshot_seq: SnapshotSeq(1),
            acked_input: InputSeq(0),
            body: EntityId::new(3).unwrap(),
            topology_revision: Revision(2),
            pose: a_pose(),
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            sleeping: false,
        };
        assert!(snap.validate().is_ok());

        snap.pose.translation_m = [0.0, f64::INFINITY, 0.0];
        assert!(matches!(snap.validate(), Err(RecordError::NonFinite(_))));

        snap.pose = a_pose();
        snap.linear_velocity = [f32::NAN, 0.0, 0.0];
        assert!(matches!(snap.validate(), Err(RecordError::NonFinite(_))));
    }

    #[test]
    fn transaction_rejects_empty_ops_and_bad_run_length() {
        let mut tx = TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: Tick(5),
            control_seq: ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![],
            result_hashes: vec![],
        };
        assert!(matches!(tx.validate(), Err(RecordError::Inconsistent(_))));

        tx.ops.push(TopologyOp::CellRun {
            volume: VolumeId::new(1).unwrap(),
            start: GlobalCell::new(0, 0, 0),
            len: 0,
            material: MaterialId(1),
        });
        assert!(matches!(tx.validate(), Err(RecordError::OutOfRange { .. })));

        tx.ops[0] = TopologyOp::CellRun {
            volume: VolumeId::new(1).unwrap(),
            start: GlobalCell::new(i64::MAX, 0, 0),
            len: 2,
            material: MaterialId(1),
        };
        assert!(matches!(tx.validate(), Err(RecordError::OutOfRange { .. })));
        if let TopologyOp::CellRun { len, .. } = &mut tx.ops[0] {
            *len = 1;
        }
        assert!(tx.validate().is_ok());

        tx.ops[0] = TopologyOp::IntegerBrush {
            volume: VolumeId::new(1).unwrap(),
            brush: SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 256).unwrap(),
            material: MaterialId(1),
        };
        assert!(tx.validate().is_ok());
    }

    #[test]
    fn transaction_validate_against_manifest_flags_unknown_material() {
        use spall_core::{MaterialDef, MaterialFlags, RenderProps, SimProps};
        let manifest = MaterialManifest::validated(vec![MaterialDef {
            id: MaterialId::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        }])
        .unwrap();

        let tx = TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: Tick(5),
            control_seq: ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![TopologyOp::CellRun {
                volume: VolumeId::new(1).unwrap(),
                start: GlobalCell::new(0, 0, 0),
                len: 4,
                material: MaterialId(9),
            }],
            result_hashes: vec![],
        };
        assert_eq!(
            tx.validate_against(&manifest),
            Err(RecordError::UnknownMaterial(9))
        );
        let bytes = crate::encode_control(&tx).unwrap();
        assert!(matches!(
            crate::decode_topology(&bytes, &manifest),
            Err(crate::CodecError::Invalid(RecordError::UnknownMaterial(9)))
        ));
    }

    #[test]
    fn split_baseline_ops_round_trip_and_validate() {
        use crate::baseline::{BaselineBrick, BaselineCells, BaselineOwner, BaselineVolume};
        use spall_core::{CELLS_PER_BRICK, MaterialDef, MaterialFlags, RenderProps, SimProps};

        let stone = MaterialDef {
            id: MaterialId(1),
            name: "stone".into(),
            render: RenderProps {
                albedo: [0.5; 3],
                roughness: 0.9,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 2600.0,
                friction: 0.8,
                restitution: 0.0,
                hardness: 4.0,
                bond_strength: 12.0,
                flags: MaterialFlags(MaterialFlags::COLLIDES.0 | MaterialFlags::STRUCTURAL.0),
            },
        };
        let air = MaterialDef {
            id: MaterialId::AIR,
            name: "air".into(),
            render: RenderProps {
                albedo: [0.0; 3],
                roughness: 1.0,
                metalness: 0.0,
                emissive: [0.0; 3],
            },
            sim: SimProps {
                density_kg_m3: 0.0,
                friction: 0.0,
                restitution: 0.0,
                hardness: 0.0,
                bond_strength: 0.0,
                flags: MaterialFlags::NONE,
            },
        };
        let manifest = MaterialManifest::validated(vec![air, stone]).unwrap();

        let child_blob = |material: u16| {
            BaselineVolume {
                volume_id: VolumeId::new(2).unwrap(),
                cell_size_code: 2,
                owner: BaselineOwner::Body(EntityId::new(5).unwrap()),
                bounds: Some([[0, 0, 0], [0, 0, 0]]),
                bricks: vec![BaselineBrick {
                    coord: [0, 0, 0],
                    revision: 2,
                    edited: true,
                    cells: BaselineCells::Dense({
                        let mut c = vec![0u16; CELLS_PER_BRICK];
                        c[0] = material;
                        c
                    }),
                }],
            }
            .encode_compressed()
        };

        let mut tx = TopologyTransaction {
            transaction_id: TransactionId::new(1).unwrap(),
            server_tick: Tick(9),
            control_seq: ControlSeq(1),
            algorithm_version: 1,
            dependencies: vec![],
            before: vec![],
            after: vec![],
            ops: vec![
                TopologyOp::IntegerBrush {
                    volume: VolumeId::new(1).unwrap(),
                    brush: SphereBrush::new(BrushPoint::from_cells(0, 0, 0).unwrap(), 256).unwrap(),
                    material: MaterialId::AIR,
                },
                TopologyOp::SplitOffBaseline {
                    source: VolumeId::new(1).unwrap(),
                    child: VolumeId::new(2).unwrap(),
                    child_entity: EntityId::new(5).unwrap(),
                    blob: child_blob(1),
                },
                TopologyOp::SourcePatchBaseline {
                    source: VolumeId::new(1).unwrap(),
                    blob: child_blob(1),
                },
            ],
            result_hashes: vec![],
        };

        // Wire round-trip through the control codec.
        assert!(tx.validate_against(&manifest).is_ok());
        let bytes = crate::encode_control(&tx).unwrap();
        assert_eq!(
            crate::decode_topology(&bytes, &manifest).unwrap(),
            tx,
            "SplitOffBaseline / SourcePatchBaseline survive the wire"
        );

        // An unknown material *inside* the blob is caught by validate_against.
        tx.ops[1] = TopologyOp::SplitOffBaseline {
            source: VolumeId::new(1).unwrap(),
            child: VolumeId::new(2).unwrap(),
            child_entity: EntityId::new(5).unwrap(),
            blob: child_blob(9),
        };
        assert_eq!(
            tx.validate_against(&manifest),
            Err(RecordError::UnknownMaterial(9))
        );

        // An empty blob is rejected by the structural validate().
        tx.ops[1] = TopologyOp::SplitOffBaseline {
            source: VolumeId::new(1).unwrap(),
            child: VolumeId::new(2).unwrap(),
            child_entity: EntityId::new(5).unwrap(),
            blob: Vec::new(),
        };
        assert!(matches!(tx.validate(), Err(RecordError::OutOfRange { .. })));
    }

    #[test]
    fn baseline_begin_bounds_parts_and_regions() {
        let mut begin = BaselineBegin {
            transfer_id: TransferId(1),
            interest_epoch: InterestEpoch(1),
            checkpoint_tick: Tick(10),
            journal_cursor: JournalSeq(5),
            world_version: 1,
            content_version: 1,
            total_bytes: 1024,
            part_count: 2,
            regions: vec![BaselineRegion {
                volume: VolumeId::new(1).unwrap(),
                min_brick: BrickCoord::new(0, 0, 0),
                max_brick: BrickCoord::new(3, 3, 3),
                revision: Revision(1),
            }],
        };
        assert!(begin.validate().is_ok());

        begin.total_bytes = limits::MAX_ASSEMBLED_TRANSFER as u64 + 1;
        assert!(matches!(begin.validate(), Err(RecordError::Size(_))));

        begin.total_bytes = 1024;
        begin.regions[0].min_brick = BrickCoord::new(9, 0, 0);
        assert!(matches!(
            begin.validate(),
            Err(RecordError::Inconsistent(_))
        ));
    }
}
