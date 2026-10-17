//! Portable creature/source ownership records and candidate-first reduction.
//!
//! These values are reusable by simulation, replica and recovery consumers,
//! but this module does not allocate transaction/journal sequences or publish
//! state to any of those consumers.

use serde::{Deserialize, Deserializer, Serialize, de::SeqAccess, de::Visitor};
use spall_core::{CreatureAllocatorSnapshotV1, CreatureKey, MaterialId, TransactionId, VolumeId};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use thiserror::Error;

use crate::{
    BaselineBrick, BaselineCells, CanonicalWriter, CreatureStateError, CreatureStateV1,
    CreatureValidationCatalog, Hash32, MAX_CREATURE_ANGULAR_VELOCITY_RAD_S,
    MAX_CREATURE_LINEAR_VELOCITY_M_S, MAX_CREATURE_POSITION_M,
};

pub const CREATURE_OWNER_SCHEMA: u16 = 1;
pub const CREATURE_OWNER_DOMAIN: &[u8] = b"spall.creature-owner.v1";
pub const CREATURE_OWNER_SET_DOMAIN: &[u8] = b"spall.creature-owner-set.v1";
pub const CREATURE_CHANGE_SCHEMA: u16 = 1;
pub const CREATURE_CHANGE_DOMAIN: &[u8] = b"spall.creature-change.v1";
pub const MAX_CREATURE_OWNER_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CREATURE_OWNER_PARTS: usize = 256;
pub const MAX_CREATURE_OWNER_BRICKS: usize = 4096;
pub const MAX_CREATURE_OWNER_DENSE_CELLS: usize = 8 * 1024 * 1024;
pub const MAX_CREATURE_OWNER_SOLID_CELLS: usize = 1_000_000;
pub const MAX_CREATURE_OWNER_COMPONENTS: usize = 256;
pub const MAX_CREATURE_OWNER_MASK_BRICKS: usize = 8192;
pub const MAX_CREATURE_OWNER_MASK_CELLS: usize = 1_000_000;
pub const MAX_CREATURE_OWNER_SET_RECORDS: usize = 1024;
pub const MAX_CREATURE_OWNER_SET_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CREATURE_OWNER_SET_DENSE_CELLS: usize = 16 * 1024 * 1024;
pub const MAX_CREATURE_OWNER_SET_SOLID_CELLS: usize = 4_000_000;
const CELLS_PER_BRICK: usize = 32 * 32 * 32;

/// One hierarchy-local transform in stable scalar form.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CreatureLocalPoseV1 {
    pub translation_m: [f32; 3],
    pub rotation_xyzw: [f32; 4],
}

/// One portable material cell in a component mask. `index` is the linear
/// index within the named 32³ source brick; source revision is on the mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatureMaskCellV1 {
    pub index: u16,
    pub material_id: u16,
}

/// A bounded exact source-cell subset from one source brick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CreatureSourceMaskBrickV1 {
    pub volume_id: VolumeId,
    pub part_id: u32,
    pub coord: [i64; 3],
    /// Portable ENG-214 source revision bound to the part.
    pub source_revision: u64,
    /// Revision of this exact resident source brick.
    pub brick_revision: u64,
    pub cells: Vec<CreatureMaskCellV1>,
}

/// Source part plus the original authoritative sparse volume contents.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureSourcePartV1 {
    pub part_id: u32,
    pub source_volume_id: VolumeId,
    pub source_generation: u64,
    pub source_revision: u64,
    pub cell_size_code: u8,
    pub bounds: Option<[[i64; 3]; 2]>,
    pub source_content_hash: Hash32,
    pub parent_id: Option<u32>,
    pub initial_local_pose: CreatureLocalPoseV1,
    pub frozen_local_pose: CreatureLocalPoseV1,
    pub bricks: Vec<BaselineBrick>,
}

/// Stable physical component identity and motion, with exact original-cell
/// ownership. `serial=None` is the one intact logical component; fractured
/// records use nonzero, monotonically allocated serials, including a single
/// surviving derived component after other components have retired.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureOwnedComponentV1 {
    pub owner_key: u128,
    pub serial: Option<u64>,
    pub translation_m: [f64; 3],
    pub rotation_xyzw: [f32; 4],
    pub linear_velocity_m_s: [f32; 3],
    pub angular_velocity_rad_s: [f32; 3],
    pub source_masks: Vec<CreatureSourceMaskBrickV1>,
}

/// Complete portable topology and owner metadata for one creature.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureOwnershipRecordV1 {
    pub schema: u16,
    pub state: CreatureStateV1,
    pub owner_key: u128,
    pub source_generation: u64,
    /// Monotonic component serial high-water; retired serials stay consumed.
    pub next_component_serial: u64,
    pub parts: Vec<CreatureSourcePartV1>,
    pub components: Vec<CreatureOwnedComponentV1>,
}

/// Canonically ordered actor/source ownership state. This has a distinct hash
/// domain and is never a substitute for the legacy complete-world hash.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreatureOwnershipSetV1 {
    pub schema: u16,
    pub records: Vec<CreatureOwnershipRecordV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CreatureLifecycleOperationV1 {
    Birth { after: CreatureOwnershipRecordV1 },
    Death { after: CreatureOwnershipRecordV1 },
}

/// Versioned change payload to be embedded by a later existing ordered
/// transaction consumer. It contains no independent sequence allocator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureLifecycleChangeV1 {
    pub schema: u16,
    pub transaction_id: TransactionId,
    pub operation_ordinal: u32,
    pub expected_before_hash: Hash32,
    pub expected_after_hash: Hash32,
    pub operation: CreatureLifecycleOperationV1,
}

/// Existing caller-owned replay and identity context. The replay receipt
/// comes from the transaction/journal consumer's replay guard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreatureLifecycleContextV1<'a> {
    pub transaction_id: TransactionId,
    pub operation_ordinal: u32,
    /// Receipt/evidence from the caller's existing applied-transaction ledger.
    pub replay_receipt: Option<CreatureLifecycleReplayReceiptV1>,
    pub allocator: CreatureAllocatorSnapshotV1,
    pub next_volume_id: u64,
    /// Value-only result of the checked ENG-216 transfer, required for death.
    pub death_witness: Option<&'a CreatureDeathWitnessV1>,
    /// Existing registry metadata used for collision checks; topology payloads
    /// are never cloned by the reducer.
    pub existing_keys: &'a [CreatureKey],
    pub existing_owner_keys: &'a [u128],
    pub existing_source_volumes: &'a [VolumeId],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatureLifecycleReplayReceiptV1 {
    pub transaction_id: TransactionId,
    pub operation_ordinal: u32,
    pub change_hash: Hash32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreatureDeathWitnessV1 {
    pub key: CreatureKey,
    pub source_generation: u64,
    pub before_state_revision: u64,
    pub before_topology_revision: u64,
    pub before_pose_epoch: u64,
    pub root_translation_m: [f64; 3],
    pub root_rotation_xyzw: [f32; 4],
    pub installed_linear_velocity_m_s: [f32; 3],
    pub installed_angular_velocity_rad_s: [f32; 3],
    pub frozen_local_poses: Vec<(u32, CreatureLocalPoseV1)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreatureLifecycleCandidateV1 {
    pub record: CreatureOwnershipRecordV1,
    pub next_creature_id: u64,
    pub next_volume_id: u64,
    pub outcome: CreatureReductionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatureReductionOutcome {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CreatureOwnershipError {
    #[error("unsupported creature ownership schema")]
    Schema,
    #[error("creature ownership record exceeds a byte, count, or work bound")]
    Limit,
    #[error("creature source identity, cell, or hierarchy is invalid")]
    Source,
    #[error("creature source material does not match its validated catalog")]
    Material,
    #[error("creature component source masks overlap, omit, or misidentify cells")]
    Mask,
    #[error("creature component frame or motion is invalid")]
    Motion,
    #[error("creature source content hash does not match canonical source bytes")]
    ContentHash,
    #[error("creature lifecycle change is stale or diverges from current owner state")]
    Stale,
    #[error("creature lifecycle change identity was reused with different state")]
    DivergentReplay,
    #[error("creature identity or source-volume counters are exhausted")]
    CounterExhausted,
    #[error("creature allocator snapshot is invalid or discontinuous")]
    Allocator,
    #[error("creature or source-volume identity is already owned")]
    DuplicateOwner,
    #[error("source edits, removals, or fracture creation are not supported by this reducer")]
    UnsupportedMutation,
    #[error("creature state: {0}")]
    State(#[from] CreatureStateError),
    #[error("encoding: {0}")]
    Encoding(String),
}

#[path = "creature_owner_decode.rs"]
mod decode;

impl<'de> Deserialize<'de> for CreatureOwnershipRecordV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        decode::deserialize_record(deserializer)
    }
}
impl<'de> Deserialize<'de> for CreatureSourcePartV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            part_id: u32,
            source_volume_id: VolumeId,
            source_generation: u64,
            source_revision: u64,
            cell_size_code: u8,
            bounds: Option<[[i64; 3]; 2]>,
            source_content_hash: Hash32,
            parent_id: Option<u32>,
            initial_local_pose: CreatureLocalPoseV1,
            frozen_local_pose: CreatureLocalPoseV1,
            bricks: BoundedBricks,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            part_id: wire.part_id,
            source_volume_id: wire.source_volume_id,
            source_generation: wire.source_generation,
            source_revision: wire.source_revision,
            cell_size_code: wire.cell_size_code,
            bounds: wire.bounds,
            source_content_hash: wire.source_content_hash,
            parent_id: wire.parent_id,
            initial_local_pose: wire.initial_local_pose,
            frozen_local_pose: wire.frozen_local_pose,
            bricks: wire.bricks.0.into_iter().map(|brick| brick.0).collect(),
        })
    }
}

impl<'de> Deserialize<'de> for CreatureOwnedComponentV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            owner_key: u128,
            serial: Option<u64>,
            translation_m: [f64; 3],
            rotation_xyzw: [f32; 4],
            linear_velocity_m_s: [f32; 3],
            angular_velocity_rad_s: [f32; 3],
            source_masks: BoundedMasks,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            owner_key: wire.owner_key,
            serial: wire.serial,
            translation_m: wire.translation_m,
            rotation_xyzw: wire.rotation_xyzw,
            linear_velocity_m_s: wire.linear_velocity_m_s,
            angular_velocity_rad_s: wire.angular_velocity_rad_s,
            source_masks: wire.source_masks.0,
        })
    }
}

macro_rules! bounded_vec {
    ($name:ident, $item:ty, $max:expr, $label:literal) => {
        struct $name(Vec<$item>);
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct SeqVisitor;
                impl<'de> Visitor<'de> for SeqVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "at most {} {}", $max, $label)
                    }
                    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
                    where
                        A: SeqAccess<'de>,
                    {
                        if seq.size_hint().is_some_and(|n| n > $max) {
                            return Err(serde::de::Error::custom(concat!(
                                $label,
                                " limit exceeded"
                            )));
                        }
                        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min($max));
                        while let Some(item) = seq.next_element()? {
                            if out.len() == $max {
                                return Err(serde::de::Error::custom(concat!(
                                    $label,
                                    " limit exceeded"
                                )));
                            }
                            out.push(item);
                        }
                        Ok($name(out))
                    }
                }
                deserializer.deserialize_seq(SeqVisitor)
            }
        }
    };
}

bounded_vec!(
    BoundedBricks,
    BaselineBrickBounded,
    MAX_CREATURE_OWNER_BRICKS,
    "source bricks"
);
bounded_vec!(
    BoundedMasks,
    CreatureSourceMaskBrickV1,
    MAX_CREATURE_OWNER_MASK_BRICKS,
    "mask bricks"
);
bounded_vec!(
    BoundedMaskCells,
    CreatureMaskCellV1,
    CELLS_PER_BRICK,
    "mask cells"
);

impl<'de> Deserialize<'de> for CreatureSourceMaskBrickV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            volume_id: VolumeId,
            part_id: u32,
            coord: [i64; 3],
            source_revision: u64,
            brick_revision: u64,
            cells: BoundedMaskCells,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            volume_id: wire.volume_id,
            part_id: wire.part_id,
            coord: wire.coord,
            source_revision: wire.source_revision,
            brick_revision: wire.brick_revision,
            cells: wire.cells.0,
        })
    }
}

impl<'de> Deserialize<'de> for BaselineBrickBounded {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            coord: [i64; 3],
            revision: u64,
            edited: bool,
            cells: BoundedBaselineCells,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self(BaselineBrick {
            coord: wire.coord,
            revision: wire.revision,
            edited: wire.edited,
            cells: wire.cells.0,
        }))
    }
}

struct BaselineBrickBounded(BaselineBrick);
struct BoundedBaselineCells(BaselineCells);

impl<'de> Deserialize<'de> for BoundedBaselineCells {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum Wire {
            Uniform(u16),
            Dense(BoundedMaterials),
            Digest {
                content_hash: Hash32,
                solid_cells: u32,
            },
        }
        Ok(Self(match Wire::deserialize(deserializer)? {
            Wire::Uniform(id) => BaselineCells::Uniform(id),
            Wire::Dense(cells) => BaselineCells::Dense(cells.0),
            Wire::Digest {
                content_hash,
                solid_cells,
            } => BaselineCells::Digest {
                content_hash,
                solid_cells,
            },
        }))
    }
}

struct BoundedMaterials(Vec<u16>);
impl<'de> Deserialize<'de> for BoundedMaterials {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct MaterialsVisitor;
        impl<'de> Visitor<'de> for MaterialsVisitor {
            type Value = BoundedMaterials;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "exactly {CELLS_PER_BRICK} material ids")
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                if seq.size_hint().is_some_and(|n| n != CELLS_PER_BRICK) {
                    return Err(serde::de::Error::custom(
                        "dense brick cell count is invalid",
                    ));
                }
                let mut out = Vec::with_capacity(CELLS_PER_BRICK);
                while let Some(id) = seq.next_element()? {
                    if out.len() == CELLS_PER_BRICK {
                        return Err(serde::de::Error::custom(
                            "dense brick cell count is invalid",
                        ));
                    }
                    out.push(id);
                }
                if out.len() != CELLS_PER_BRICK {
                    return Err(serde::de::Error::custom(
                        "dense brick cell count is invalid",
                    ));
                }
                Ok(BoundedMaterials(out))
            }
        }
        deserializer.deserialize_seq(MaterialsVisitor)
    }
}

impl CreatureLifecycleChangeV1 {
    pub fn canonical_hash(&self) -> Hash32 {
        let mut w = CanonicalWriter::with_domain(CREATURE_CHANGE_DOMAIN);
        w.u16(self.schema)
            .u64(self.transaction_id.get())
            .u32(self.operation_ordinal)
            .blob(&self.expected_before_hash.0)
            .blob(&self.expected_after_hash.0);
        match &self.operation {
            CreatureLifecycleOperationV1::Birth { after } => {
                w.u8(1).blob(&after.canonical_hash().0);
            }
            CreatureLifecycleOperationV1::Death { after } => {
                w.u8(2).blob(&after.canonical_hash().0);
            }
        }
        w.hash()
    }

    pub fn validate(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureOwnershipError> {
        if self.schema != CREATURE_CHANGE_SCHEMA {
            return Err(CreatureOwnershipError::Schema);
        }
        match &self.operation {
            CreatureLifecycleOperationV1::Birth { after }
            | CreatureLifecycleOperationV1::Death { after } => after.validate(catalog)?,
        }
        let size = postcard::experimental::serialized_size(self)
            .map_err(|error| CreatureOwnershipError::Encoding(error.to_string()))?;
        if size > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        Ok(())
    }

    pub fn encode(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Vec<u8>, CreatureOwnershipError> {
        self.validate(catalog)?;
        let bytes = postcard::to_stdvec(self)
            .map_err(|e| CreatureOwnershipError::Encoding(e.to_string()))?;
        if bytes.len() > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        Ok(bytes)
    }

    pub fn decode(
        bytes: &[u8],
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Self, CreatureOwnershipError> {
        if bytes.len() > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        let (change, rest): (Self, &[u8]) = postcard::take_from_bytes(bytes)
            .map_err(|e| CreatureOwnershipError::Encoding(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureOwnershipError::Encoding("trailing bytes".into()));
        }
        change.validate(catalog)?;
        Ok(change)
    }
}

impl CreatureSourcePartV1 {
    /// Hashes only portable source identity and bytes. Hierarchy and moving
    /// state deliberately belong to the enclosing owner hash so death leaves
    /// this digest unchanged.
    pub fn canonical_source_content_hash(&self) -> Hash32 {
        let mut w = CanonicalWriter::with_domain(b"spall.creature-source-content.v1");
        w.u32(self.part_id)
            .u64(self.source_volume_id.get())
            .u64(self.source_generation)
            .u64(self.source_revision)
            .u8(self.cell_size_code);
        write_bounds(self.bounds, &mut w);
        w.u32(self.bricks.len() as u32);
        for brick in &self.bricks {
            for coordinate in brick.coord {
                w.i64(coordinate);
            }
            w.u64(brick.revision).u8(u8::from(brick.edited));
            match &brick.cells {
                BaselineCells::Uniform(id) => {
                    w.u8(0).u16(*id);
                }
                BaselineCells::Dense(ids) => {
                    let first = ids
                        .first()
                        .copied()
                        .filter(|first| ids.iter().all(|id| id == first));
                    if let Some(first) = first {
                        w.u8(0).u16(first);
                    } else {
                        w.u8(1).u32(ids.len() as u32);
                        for id in ids {
                            w.u16(*id);
                        }
                    }
                }
                BaselineCells::Digest {
                    content_hash,
                    solid_cells,
                } => {
                    w.u8(2).blob(&content_hash.0).u32(*solid_cells);
                }
            }
        }
        w.hash()
    }
}

impl CreatureOwnershipRecordV1 {
    pub fn validate(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureOwnershipError> {
        validate_record_work_bounds(self)?;
        if self.schema != CREATURE_OWNER_SCHEMA {
            return Err(CreatureOwnershipError::Schema);
        }
        self.state.validate(catalog)?;
        if self.owner_key == 0
            || self.source_generation == 0
            || self.next_component_serial == 0
            || self.parts.is_empty()
            || self.parts.len() > MAX_CREATURE_OWNER_PARTS
            || self.components.is_empty()
            || self.components.len() > MAX_CREATURE_OWNER_COMPONENTS
            || self
                .parts
                .windows(2)
                .any(|pair| pair[0].part_id >= pair[1].part_id)
        {
            return Err(CreatureOwnershipError::Limit);
        }
        if self.parts.len() != self.state.source_bindings.len() {
            return Err(CreatureOwnershipError::Source);
        }
        let mut sources = BTreeMap::new();
        let mut total_bricks = 0usize;
        let mut dense_cells = 0usize;
        let mut solid_count = 0usize;
        let mut mapped_materials = BTreeSet::new();
        let asset = catalog
            .assets
            .iter()
            .find(|entry| {
                entry.uuid == self.state.asset.uuid
                    && entry.logical_hash == self.state.asset.logical_hash
            })
            .ok_or(CreatureOwnershipError::Material)?;
        for entry in catalog.material_mapping {
            if asset.portable_material_keys.contains(&entry.portable_key) {
                mapped_materials.insert(entry.material_id.0);
            }
        }
        for (part, binding) in self.parts.iter().zip(&self.state.source_bindings) {
            if part.part_id != binding.part_id
                || part.source_volume_id != binding.source_volume_id
                || part.source_revision != binding.source_revision
                || part.source_generation != self.source_generation
                || part.part_id == 0
                || part.source_generation == 0
                || part.source_revision == 0
                || spall_core::CellSizeCode::from_u8(part.cell_size_code).is_none()
                || part.bricks.is_empty()
                || part
                    .bricks
                    .windows(2)
                    .any(|pair| coord_order(pair[0].coord) >= coord_order(pair[1].coord))
                || !valid_bounds(part.bounds)
                || !valid_local_pose(part.initial_local_pose)
                || !valid_local_pose(part.frozen_local_pose)
            {
                return Err(CreatureOwnershipError::Source);
            }
            if let Some(parent) = part.parent_id
                && (parent == part.part_id
                    || self
                        .parts
                        .binary_search_by_key(&parent, |p| p.part_id)
                        .is_err())
            {
                return Err(CreatureOwnershipError::Source);
            }
            if sources.insert(part.source_volume_id, part).is_some() {
                return Err(CreatureOwnershipError::Source);
            }
            total_bricks = total_bricks
                .checked_add(part.bricks.len())
                .ok_or(CreatureOwnershipError::Limit)?;
            if total_bricks > MAX_CREATURE_OWNER_BRICKS {
                return Err(CreatureOwnershipError::Limit);
            }
            for brick in &part.bricks {
                if brick.revision == 0
                    || part
                        .bounds
                        .is_some_and(|bounds| !within_bounds(bounds, brick.coord))
                {
                    return Err(CreatureOwnershipError::Source);
                }
                let len = match &brick.cells {
                    BaselineCells::Uniform(id) => {
                        if *id != 0 {
                            solid_count = solid_count
                                .checked_add(CELLS_PER_BRICK)
                                .ok_or(CreatureOwnershipError::Limit)?;
                            if !mapped_materials.contains(id)
                                || !catalog.materials.contains(MaterialId(*id))
                            {
                                return Err(CreatureOwnershipError::Material);
                            }
                        }
                        0
                    }
                    BaselineCells::Dense(ids) => {
                        if ids.len() != CELLS_PER_BRICK {
                            return Err(CreatureOwnershipError::Source);
                        }
                        dense_cells = dense_cells
                            .checked_add(ids.len())
                            .ok_or(CreatureOwnershipError::Limit)?;
                        if dense_cells > MAX_CREATURE_OWNER_DENSE_CELLS {
                            return Err(CreatureOwnershipError::Limit);
                        }
                        for id in ids {
                            if *id != 0 {
                                solid_count = solid_count
                                    .checked_add(1)
                                    .ok_or(CreatureOwnershipError::Limit)?;
                                if !mapped_materials.contains(id)
                                    || !catalog.materials.contains(MaterialId(*id))
                                {
                                    return Err(CreatureOwnershipError::Material);
                                }
                            }
                        }
                        ids.len()
                    }
                    BaselineCells::Digest { .. } => return Err(CreatureOwnershipError::Source),
                };
                let _ = len;
            }
            if solid_count > MAX_CREATURE_OWNER_SOLID_CELLS
                || part.source_content_hash != part.canonical_source_content_hash()
            {
                return Err(if solid_count > MAX_CREATURE_OWNER_SOLID_CELLS {
                    CreatureOwnershipError::Limit
                } else {
                    CreatureOwnershipError::ContentHash
                });
            }
        }
        // Parent chains must be rooted and acyclic; checking only that each
        // parent exists is insufficient for a portable hierarchy.
        for part in &self.parts {
            let mut visited = BTreeSet::new();
            let mut current = Some(part.part_id);
            while let Some(id) = current {
                if !visited.insert(id) {
                    return Err(CreatureOwnershipError::Source);
                }
                current = self
                    .parts
                    .binary_search_by_key(&id, |entry| entry.part_id)
                    .ok()
                    .and_then(|index| self.parts[index].parent_id);
            }
        }
        if self
            .parts
            .iter()
            .filter(|part| part.parent_id.is_none())
            .count()
            != 1
        {
            return Err(CreatureOwnershipError::Source);
        }
        if self
            .components
            .windows(2)
            .any(|pair| component_order(&pair[0]) >= component_order(&pair[1]))
            || self
                .components
                .iter()
                .any(|component| component.owner_key != self.owner_key)
        {
            return Err(CreatureOwnershipError::Mask);
        }
        let intact = self.components.len() == 1 && self.components[0].serial.is_none();
        let derived = self.components.iter().all(|component| {
            component
                .serial
                .is_some_and(|serial| serial != 0 && serial < self.next_component_serial)
        });
        if !intact && (self.state.lifecycle == crate::CreatureLifecycleState::Alive || !derived) {
            return Err(CreatureOwnershipError::Mask);
        }
        if intact && self.next_component_serial != 1 {
            return Err(CreatureOwnershipError::Mask);
        }
        let mut all_seen = BTreeSet::new();
        let mut total_masks = 0usize;
        let mut total_mask_cells = 0usize;
        for component in &self.components {
            validate_component_motion(component)?;
            if component.source_masks.is_empty()
                || component.source_masks.len() > MAX_CREATURE_OWNER_MASK_BRICKS
                || component
                    .source_masks
                    .windows(2)
                    .any(|pair| mask_order(&pair[0]) >= mask_order(&pair[1]))
            {
                return Err(CreatureOwnershipError::Mask);
            }
            total_masks = total_masks
                .checked_add(component.source_masks.len())
                .ok_or(CreatureOwnershipError::Limit)?;
            if total_masks > MAX_CREATURE_OWNER_MASK_BRICKS {
                return Err(CreatureOwnershipError::Limit);
            }
            for mask in &component.source_masks {
                let part = sources
                    .get(&mask.volume_id)
                    .ok_or(CreatureOwnershipError::Mask)?;
                if part.part_id != mask.part_id
                    || mask.source_revision != part.source_revision
                    || mask.brick_revision == 0
                {
                    return Err(CreatureOwnershipError::Mask);
                }
                let brick = part
                    .bricks
                    .binary_search_by(|brick| {
                        coord_order(brick.coord).cmp(&coord_order(mask.coord))
                    })
                    .ok()
                    .map(|index| &part.bricks[index])
                    .ok_or(CreatureOwnershipError::Mask)?;
                if brick.revision != mask.brick_revision
                    || mask.cells.is_empty()
                    || mask
                        .cells
                        .windows(2)
                        .any(|pair| pair[0].index >= pair[1].index)
                {
                    return Err(CreatureOwnershipError::Mask);
                }
                for cell in &mask.cells {
                    let idx = usize::from(cell.index);
                    if idx >= CELLS_PER_BRICK
                        || global_cell_for_index(mask.coord, idx).is_none()
                        || material_at(&brick.cells, idx) != Some(cell.material_id)
                        || cell.material_id == 0
                        || !all_seen.insert((mask.volume_id, mask.coord, cell.index))
                    {
                        return Err(CreatureOwnershipError::Mask);
                    }
                    total_mask_cells = total_mask_cells
                        .checked_add(1)
                        .ok_or(CreatureOwnershipError::Limit)?;
                    if total_mask_cells > MAX_CREATURE_OWNER_MASK_CELLS {
                        return Err(CreatureOwnershipError::Limit);
                    }
                }
            }
        }
        if total_mask_cells != solid_count || all_seen.len() != solid_count {
            return Err(CreatureOwnershipError::Mask);
        }
        if intact {
            let component = &self.components[0];
            if !same_f64_vec3(component.translation_m, self.state.root_translation_m)
                || !same_f32_array(component.rotation_xyzw, self.state.root_rotation_xyzw)
                || !same_f32_array(
                    component.linear_velocity_m_s,
                    self.state.linear_velocity_m_s,
                )
                || !same_f32_array(
                    component.angular_velocity_rad_s,
                    self.state.angular_velocity_rad_s,
                )
            {
                return Err(CreatureOwnershipError::Motion);
            }
        }
        Ok(())
    }

    pub fn canonical_hash(&self) -> Hash32 {
        let mut w = CanonicalWriter::with_domain(CREATURE_OWNER_DOMAIN);
        w.u16(self.schema);
        write_creature_state(&self.state, &mut w);
        w.u128(self.owner_key)
            .u64(self.source_generation)
            .u64(self.next_component_serial);
        let mut parts: Vec<_> = self.parts.iter().collect();
        parts.sort_by_key(|part| part.part_id);
        w.u32(parts.len() as u32);
        for part in parts {
            w.u32(part.part_id)
                .u64(part.source_volume_id.get())
                .u64(part.source_generation)
                .u64(part.source_revision)
                .u8(part.cell_size_code);
            write_bounds(part.bounds, &mut w);
            w.blob(&part.source_content_hash.0);
            write_option_u32(part.parent_id, &mut w);
            write_local_pose(part.initial_local_pose, &mut w);
            write_local_pose(part.frozen_local_pose, &mut w);
        }
        let mut components: Vec<_> = self.components.iter().collect();
        components.sort_by_key(|component| component_order(component));
        w.u32(components.len() as u32);
        for component in components {
            w.u128(component.owner_key);
            write_option_u64(component.serial, &mut w);
            write_vec3_f64(component.translation_m, &mut w);
            write_quat(component.rotation_xyzw, &mut w);
            write_vec3_f32(component.linear_velocity_m_s, &mut w);
            write_vec3_f32(component.angular_velocity_rad_s, &mut w);
            w.u32(component.source_masks.len() as u32);
            for mask in &component.source_masks {
                w.u64(mask.volume_id.get()).u32(mask.part_id);
                for coordinate in mask.coord {
                    w.i64(coordinate);
                }
                w.u64(mask.source_revision)
                    .u64(mask.brick_revision)
                    .u32(mask.cells.len() as u32);
                for cell in &mask.cells {
                    w.u16(cell.index).u16(cell.material_id);
                }
            }
        }
        w.hash()
    }

    pub fn encode(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Vec<u8>, CreatureOwnershipError> {
        self.validate(catalog)?;
        let bytes = postcard::to_stdvec(self)
            .map_err(|e| CreatureOwnershipError::Encoding(e.to_string()))?;
        if bytes.len() > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        Ok(bytes)
    }

    pub fn decode(
        bytes: &[u8],
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<Self, CreatureOwnershipError> {
        if bytes.len() > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        let (record, rest): (Self, &[u8]) = postcard::take_from_bytes(bytes)
            .map_err(|e| CreatureOwnershipError::Encoding(e.to_string()))?;
        if !rest.is_empty() {
            return Err(CreatureOwnershipError::Encoding("trailing bytes".into()));
        }
        record.validate(catalog)?;
        Ok(record)
    }
}

fn validate_record_work_bounds(
    record: &CreatureOwnershipRecordV1,
) -> Result<(), CreatureOwnershipError> {
    let mut estimated_bytes = 512usize
        .checked_add(record.state.species_key.len())
        .and_then(|value| value.checked_add(record.state.clip_name.len()))
        .and_then(|value| value.checked_add(record.state.source_bindings.len().checked_mul(32)?))
        .ok_or(CreatureOwnershipError::Limit)?;
    let mut bricks = 0usize;
    let mut dense_slots = 0usize;
    let mut solid_upper_bound = 0usize;
    let mut masks = 0usize;
    let mut mask_cells = 0usize;
    for part in &record.parts {
        if part.bricks.len() > MAX_CREATURE_OWNER_BRICKS {
            return Err(CreatureOwnershipError::Limit);
        }
        bricks = bricks
            .checked_add(part.bricks.len())
            .ok_or(CreatureOwnershipError::Limit)?;
        estimated_bytes = estimated_bytes
            .checked_add(256)
            .and_then(|value| value.checked_add(part.bricks.len().checked_mul(64)?))
            .ok_or(CreatureOwnershipError::Limit)?;
        if bricks > MAX_CREATURE_OWNER_BRICKS || estimated_bytes > MAX_CREATURE_OWNER_RECORD_BYTES {
            return Err(CreatureOwnershipError::Limit);
        }
        for brick in &part.bricks {
            match &brick.cells {
                BaselineCells::Uniform(material) => {
                    if *material != 0 {
                        solid_upper_bound = solid_upper_bound
                            .checked_add(CELLS_PER_BRICK)
                            .ok_or(CreatureOwnershipError::Limit)?;
                    }
                    estimated_bytes = estimated_bytes
                        .checked_add(2)
                        .ok_or(CreatureOwnershipError::Limit)?;
                }
                BaselineCells::Dense(ids) => {
                    if ids.len() != CELLS_PER_BRICK {
                        return Err(CreatureOwnershipError::Source);
                    }
                    dense_slots = dense_slots
                        .checked_add(ids.len())
                        .ok_or(CreatureOwnershipError::Limit)?;
                    estimated_bytes = estimated_bytes
                        .checked_add(
                            ids.len()
                                .checked_mul(2)
                                .ok_or(CreatureOwnershipError::Limit)?,
                        )
                        .ok_or(CreatureOwnershipError::Limit)?;
                }
                BaselineCells::Digest { .. } => {
                    estimated_bytes = estimated_bytes
                        .checked_add(40)
                        .ok_or(CreatureOwnershipError::Limit)?;
                }
            }
            if dense_slots > MAX_CREATURE_OWNER_DENSE_CELLS
                || solid_upper_bound > MAX_CREATURE_OWNER_SOLID_CELLS
                || estimated_bytes > MAX_CREATURE_OWNER_RECORD_BYTES
            {
                return Err(CreatureOwnershipError::Limit);
            }
        }
    }
    for component in &record.components {
        if component.source_masks.len() > MAX_CREATURE_OWNER_MASK_BRICKS {
            return Err(CreatureOwnershipError::Limit);
        }
        masks = masks
            .checked_add(component.source_masks.len())
            .ok_or(CreatureOwnershipError::Limit)?;
        if masks > MAX_CREATURE_OWNER_MASK_BRICKS {
            return Err(CreatureOwnershipError::Limit);
        }
        estimated_bytes = estimated_bytes
            .checked_add(128)
            .and_then(|value| value.checked_add(component.source_masks.len().checked_mul(64)?))
            .ok_or(CreatureOwnershipError::Limit)?;
        for mask in &component.source_masks {
            if mask.cells.len() > MAX_CREATURE_OWNER_MASK_CELLS {
                return Err(CreatureOwnershipError::Limit);
            }
            mask_cells = mask_cells
                .checked_add(mask.cells.len())
                .ok_or(CreatureOwnershipError::Limit)?;
            estimated_bytes = estimated_bytes
                .checked_add(
                    mask.cells
                        .len()
                        .checked_mul(4)
                        .ok_or(CreatureOwnershipError::Limit)?,
                )
                .ok_or(CreatureOwnershipError::Limit)?;
            if mask_cells > MAX_CREATURE_OWNER_MASK_CELLS
                || estimated_bytes > MAX_CREATURE_OWNER_RECORD_BYTES
            {
                return Err(CreatureOwnershipError::Limit);
            }
        }
    }
    if estimated_bytes > MAX_CREATURE_OWNER_RECORD_BYTES {
        return Err(CreatureOwnershipError::Limit);
    }
    let encoded_size = postcard::experimental::serialized_size(record)
        .map_err(|error| CreatureOwnershipError::Encoding(error.to_string()))?;
    if encoded_size > MAX_CREATURE_OWNER_RECORD_BYTES {
        return Err(CreatureOwnershipError::Limit);
    }
    Ok(())
}

impl CreatureOwnershipSetV1 {
    pub fn empty() -> Self {
        Self {
            schema: CREATURE_OWNER_SCHEMA,
            records: Vec::new(),
        }
    }

    pub fn canonical_hash(&self) -> Hash32 {
        let mut w = CanonicalWriter::with_domain(CREATURE_OWNER_SET_DOMAIN);
        w.u16(self.schema);
        let mut records: Vec<_> = self.records.iter().collect();
        records.sort_by_key(|record| record.state.key);
        w.u32(records.len() as u32);
        for record in records {
            w.blob(&record.canonical_hash().0);
        }
        w.hash()
    }

    /// Validates a bounded aggregate. `MAX_CREATURE_OWNER_SET_BYTES` is a
    /// conservative work/memory estimate over owned vectors, not an exact
    /// postcard byte ceiling; each encoded record has its own exact limit.
    pub fn validate(
        &self,
        catalog: &CreatureValidationCatalog<'_>,
    ) -> Result<(), CreatureOwnershipError> {
        if self.schema != CREATURE_OWNER_SCHEMA
            || self.records.len() > MAX_CREATURE_OWNER_SET_RECORDS
            || self
                .records
                .windows(2)
                .any(|pair| pair[0].state.key >= pair[1].state.key)
        {
            return Err(CreatureOwnershipError::Schema);
        }
        let mut estimated_bytes = 0usize;
        let mut dense_slots = 0usize;
        let mut solid_upper_bound = 0usize;
        let mut total_bricks = 0usize;
        let mut masks = 0usize;
        let mut mask_cells = 0usize;
        for record in &self.records {
            if record.parts.len() > MAX_CREATURE_OWNER_PARTS
                || record.components.len() > MAX_CREATURE_OWNER_COMPONENTS
            {
                return Err(CreatureOwnershipError::Limit);
            }
            estimated_bytes = estimated_bytes
                .checked_add(256)
                .ok_or(CreatureOwnershipError::Limit)?;
            if estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES {
                return Err(CreatureOwnershipError::Limit);
            }
            for part in &record.parts {
                if part.bricks.len() > MAX_CREATURE_OWNER_BRICKS {
                    return Err(CreatureOwnershipError::Limit);
                }
                total_bricks = total_bricks
                    .checked_add(part.bricks.len())
                    .ok_or(CreatureOwnershipError::Limit)?;
                if total_bricks > MAX_CREATURE_OWNER_BRICKS {
                    return Err(CreatureOwnershipError::Limit);
                }
                estimated_bytes = estimated_bytes
                    .checked_add(128)
                    .ok_or(CreatureOwnershipError::Limit)?;
                if estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES {
                    return Err(CreatureOwnershipError::Limit);
                }
                for brick in &part.bricks {
                    estimated_bytes = estimated_bytes
                        .checked_add(64)
                        .ok_or(CreatureOwnershipError::Limit)?;
                    if estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES {
                        return Err(CreatureOwnershipError::Limit);
                    }
                    match &brick.cells {
                        BaselineCells::Uniform(id) if *id != 0 => {
                            solid_upper_bound = solid_upper_bound
                                .checked_add(CELLS_PER_BRICK)
                                .ok_or(CreatureOwnershipError::Limit)?;
                        }
                        BaselineCells::Dense(ids) => {
                            if ids.len() != CELLS_PER_BRICK {
                                return Err(CreatureOwnershipError::Source);
                            }
                            dense_slots = dense_slots
                                .checked_add(ids.len())
                                .ok_or(CreatureOwnershipError::Limit)?;
                            estimated_bytes = estimated_bytes
                                .checked_add(
                                    ids.len()
                                        .checked_mul(2)
                                        .ok_or(CreatureOwnershipError::Limit)?,
                                )
                                .ok_or(CreatureOwnershipError::Limit)?;
                        }
                        _ => {}
                    }
                    if dense_slots > MAX_CREATURE_OWNER_SET_DENSE_CELLS
                        || solid_upper_bound > MAX_CREATURE_OWNER_SET_SOLID_CELLS
                        || estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES
                    {
                        return Err(CreatureOwnershipError::Limit);
                    }
                }
            }
            for component in &record.components {
                if component.source_masks.len() > MAX_CREATURE_OWNER_MASK_BRICKS {
                    return Err(CreatureOwnershipError::Limit);
                }
                masks = masks
                    .checked_add(component.source_masks.len())
                    .ok_or(CreatureOwnershipError::Limit)?;
                if masks > MAX_CREATURE_OWNER_MASK_BRICKS {
                    return Err(CreatureOwnershipError::Limit);
                }
                for mask in &component.source_masks {
                    if mask.cells.len() > MAX_CREATURE_OWNER_MASK_CELLS {
                        return Err(CreatureOwnershipError::Limit);
                    }
                    mask_cells = mask_cells
                        .checked_add(mask.cells.len())
                        .ok_or(CreatureOwnershipError::Limit)?;
                    estimated_bytes = estimated_bytes
                        .checked_add(64)
                        .ok_or(CreatureOwnershipError::Limit)?;
                    estimated_bytes = estimated_bytes
                        .checked_add(
                            mask.cells
                                .len()
                                .checked_mul(4)
                                .ok_or(CreatureOwnershipError::Limit)?,
                        )
                        .ok_or(CreatureOwnershipError::Limit)?;
                    if mask_cells > MAX_CREATURE_OWNER_SET_SOLID_CELLS
                        || estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES
                    {
                        return Err(CreatureOwnershipError::Limit);
                    }
                }
            }
        }
        if estimated_bytes > MAX_CREATURE_OWNER_SET_BYTES
            || dense_slots > MAX_CREATURE_OWNER_SET_DENSE_CELLS
            || solid_upper_bound > MAX_CREATURE_OWNER_SET_SOLID_CELLS
            || masks > MAX_CREATURE_OWNER_MASK_BRICKS
            || mask_cells > MAX_CREATURE_OWNER_SET_SOLID_CELLS
        {
            return Err(CreatureOwnershipError::Limit);
        }
        // Dense material scanning is allowed only after all aggregate vector
        // lengths and byte estimates have passed their work bounds.
        let mut actual_solids = solid_upper_bound;
        for record in &self.records {
            for part in &record.parts {
                for brick in &part.bricks {
                    if let BaselineCells::Dense(ids) = &brick.cells {
                        actual_solids = actual_solids
                            .checked_add(ids.iter().filter(|id| **id != 0).count())
                            .ok_or(CreatureOwnershipError::Limit)?;
                        if actual_solids > MAX_CREATURE_OWNER_SET_SOLID_CELLS {
                            return Err(CreatureOwnershipError::Limit);
                        }
                    }
                }
            }
        }
        let mut keys = BTreeSet::new();
        let mut owners = BTreeSet::new();
        let mut volumes = BTreeSet::new();
        for record in &self.records {
            record.validate(catalog)?;
            if !keys.insert(record.state.key)
                || !owners.insert(record.owner_key)
                || record
                    .parts
                    .iter()
                    .any(|part| !volumes.insert(part.source_volume_id))
            {
                return Err(CreatureOwnershipError::DuplicateOwner);
            }
        }
        Ok(())
    }
}

pub fn reduce_creature_lifecycle_candidate(
    current: Option<&CreatureOwnershipRecordV1>,
    change: &CreatureLifecycleChangeV1,
    context: CreatureLifecycleContextV1<'_>,
    catalog: &CreatureValidationCatalog<'_>,
) -> Result<CreatureLifecycleCandidateV1, CreatureOwnershipError> {
    if change.schema != CREATURE_CHANGE_SCHEMA
        || change.transaction_id != context.transaction_id
        || change.operation_ordinal != context.operation_ordinal
    {
        return Err(CreatureOwnershipError::Schema);
    }
    // Validate the complete operation value before considering replay. A
    // canonical hash is not a substitute for validating caller-owned fields.
    change.validate(catalog)?;
    validate_reducer_context(current, context)?;
    if let Some(record) = current {
        record.validate(catalog)?;
    }
    let change_hash = change.canonical_hash();
    let current_hash = current
        .map(CreatureOwnershipRecordV1::canonical_hash)
        .unwrap_or(Hash32::ZERO);
    let after = match &change.operation {
        CreatureLifecycleOperationV1::Birth { after }
        | CreatureLifecycleOperationV1::Death { after } => after,
    };
    if let Some(receipt) = context.replay_receipt {
        if receipt.transaction_id == context.transaction_id
            && receipt.operation_ordinal == context.operation_ordinal
            && receipt.change_hash == change_hash
            && current_hash == change.expected_after_hash
            && after.canonical_hash() == change.expected_after_hash
            && current.is_some_and(|record| record.state.key == after.state.key)
        {
            let record = current.ok_or(CreatureOwnershipError::DivergentReplay)?;
            return Ok(CreatureLifecycleCandidateV1 {
                record: record.clone(),
                next_creature_id: context.allocator.next_creature_id,
                next_volume_id: context.next_volume_id,
                outcome: CreatureReductionOutcome::Duplicate,
            });
        }
        return Err(CreatureOwnershipError::DivergentReplay);
    }
    if current_hash != change.expected_before_hash {
        return Err(CreatureOwnershipError::Stale);
    }

    let mut next_creature_id = context.allocator.next_creature_id;
    let mut next_volume_id = context.next_volume_id;
    match &change.operation {
        CreatureLifecycleOperationV1::Birth { after } => {
            if after.state.lifecycle != crate::CreatureLifecycleState::Alive
                || after.state.clip_mode != crate::CreatureClipMode::Playing
                || after.source_generation != 1
                || after.parts.iter().any(|part| part.source_generation != 1)
                || after.components.iter().any(|component| {
                    component
                        .linear_velocity_m_s
                        .iter()
                        .any(|value| *value != 0.0)
                        || component
                            .angular_velocity_rad_s
                            .iter()
                            .any(|value| *value != 0.0)
                })
                || after
                    .state
                    .linear_velocity_m_s
                    .iter()
                    .any(|value| *value != 0.0)
                || after
                    .state
                    .angular_velocity_rad_s
                    .iter()
                    .any(|value| *value != 0.0)
                || after.state.key.world_id != context.allocator.world_id
                || after.state.key.world_epoch.get() != context.allocator.world_epoch
                || after.state.key.creature_id.get() != context.allocator.next_creature_id
                || context.allocator.schema != spall_core::CREATURE_ALLOCATOR_SCHEMA
                || context.allocator.next_creature_id == 0
                || context.allocator.world_id.is_nil()
                || context.allocator.world_epoch == 0
                || context.next_volume_id == 0
                || current.is_some()
            {
                return Err(CreatureOwnershipError::Allocator);
            }
            if context.existing_keys.contains(&after.state.key)
                || context.existing_owner_keys.contains(&after.owner_key)
                || after.parts.iter().any(|part| {
                    context
                        .existing_source_volumes
                        .contains(&part.source_volume_id)
                })
            {
                return Err(CreatureOwnershipError::DuplicateOwner);
            }
            let mut expected_volume = next_volume_id;
            for part in &after.parts {
                if part.source_volume_id.get() != expected_volume || expected_volume == 0 {
                    return Err(CreatureOwnershipError::Allocator);
                }
                expected_volume = expected_volume
                    .checked_add(1)
                    .ok_or(CreatureOwnershipError::CounterExhausted)?;
            }
            next_creature_id = next_creature_id
                .checked_add(1)
                .ok_or(CreatureOwnershipError::CounterExhausted)?;
            next_volume_id = expected_volume;
        }
        CreatureLifecycleOperationV1::Death { after } => {
            let Some(before_record) = current else {
                return Err(CreatureOwnershipError::Stale);
            };
            if before_record.state.lifecycle != crate::CreatureLifecycleState::Alive
                || after.state.lifecycle != crate::CreatureLifecycleState::Corpse
                || after.state.clip_mode != crate::CreatureClipMode::Frozen
                || before_record.owner_key != after.owner_key
                || before_record.source_generation != after.source_generation
                || before_record.next_component_serial != after.next_component_serial
                || !same_source_identity_and_static_definition(before_record, after)
                || before_record.components.len() != 1
                || after.components.len() != 1
                || before_record.components[0].serial != after.components[0].serial
                || before_record.components[0].source_masks != after.components[0].source_masks
                || after.components[0].serial.is_some()
                || !valid_death_transition(
                    before_record,
                    after,
                    context.death_witness.ok_or(CreatureOwnershipError::Stale)?,
                )
            {
                return Err(CreatureOwnershipError::UnsupportedMutation);
            }
        }
    }
    if after.canonical_hash() != change.expected_after_hash {
        return Err(CreatureOwnershipError::Stale);
    }
    after.validate(catalog)?;
    Ok(CreatureLifecycleCandidateV1 {
        record: after.clone(),
        next_creature_id,
        next_volume_id,
        outcome: CreatureReductionOutcome::Applied,
    })
}

fn validate_reducer_context(
    current: Option<&CreatureOwnershipRecordV1>,
    context: CreatureLifecycleContextV1<'_>,
) -> Result<(), CreatureOwnershipError> {
    if context.allocator.schema != spall_core::CREATURE_ALLOCATOR_SCHEMA
        || context.allocator.world_id.is_nil()
        || context.allocator.world_epoch == 0
        || context.allocator.next_creature_id == 0
        || context.next_volume_id == 0
    {
        return Err(CreatureOwnershipError::Allocator);
    }
    if let Some(record) = current {
        record
            .state
            .key
            .validate()
            .map_err(|_| CreatureOwnershipError::Allocator)?;
        if record.state.key.world_id != context.allocator.world_id
            || record.state.key.world_epoch.get() != context.allocator.world_epoch
            || record.state.key.creature_id.get() >= context.allocator.next_creature_id
            || record
                .parts
                .iter()
                .any(|part| part.source_volume_id.get() >= context.next_volume_id)
        {
            return Err(CreatureOwnershipError::Allocator);
        }
    }
    Ok(())
}

fn same_source_identity_and_static_definition(
    left: &CreatureOwnershipRecordV1,
    right: &CreatureOwnershipRecordV1,
) -> bool {
    left.parts.len() == right.parts.len()
        && left.parts.iter().zip(&right.parts).all(|(a, b)| {
            a.part_id == b.part_id
                && a.source_volume_id == b.source_volume_id
                && a.source_generation == b.source_generation
                && a.source_revision == b.source_revision
                && a.cell_size_code == b.cell_size_code
                && a.bounds == b.bounds
                && a.source_content_hash == b.source_content_hash
                && a.parent_id == b.parent_id
                && a.initial_local_pose == b.initial_local_pose
                && a.bricks == b.bricks
        })
}

fn valid_death_transition(
    before: &CreatureOwnershipRecordV1,
    after: &CreatureOwnershipRecordV1,
    witness: &CreatureDeathWitnessV1,
) -> bool {
    let a = &before.state;
    let b = &after.state;
    let Some(next_state) = a.state_revision.checked_add(1) else {
        return false;
    };
    let Some(next_pose) = a.pose_epoch.checked_add(1) else {
        return false;
    };
    let Some(next_clip) = a.clip_epoch.checked_add(1) else {
        return false;
    };
    if witness.key != a.key
        || witness.source_generation != before.source_generation
        || witness.before_state_revision != a.state_revision
        || witness.before_topology_revision != a.aggregate_topology_revision
        || witness.before_pose_epoch != a.pose_epoch
        || witness.frozen_local_poses.len() != after.parts.len()
        || witness
            .frozen_local_poses
            .windows(2)
            .any(|pair| pair[0].0 >= pair[1].0)
        || b.key != a.key
        || b.state_revision != next_state
        || b.lifecycle != crate::CreatureLifecycleState::Corpse
        || b.species_key != a.species_key
        || b.species_version != a.species_version
        || b.asset != a.asset
        || b.material_manifest_hash != a.material_manifest_hash
        || b.material_mapping_version != a.material_mapping_version
        || b.material_mapping_hash != a.material_mapping_hash
        || b.clip_name != a.clip_name
        || b.clip_mode != crate::CreatureClipMode::Frozen
        || b.clip_phase_tick != a.clip_phase_tick
        || b.clip_subtick_remainder != a.clip_subtick_remainder
        || b.clip_epoch != next_clip
        || b.source_bindings != a.source_bindings
        || b.aggregate_topology_revision != a.aggregate_topology_revision
        || b.pose_epoch != next_pose
        || b.locomotion_tag != a.locomotion_tag
        || !same_f64_vec3(b.root_translation_m, witness.root_translation_m)
        || !same_f32_array(b.root_rotation_xyzw, witness.root_rotation_xyzw)
        || !same_f32_array(b.linear_velocity_m_s, witness.installed_linear_velocity_m_s)
        || !same_f32_array(
            b.angular_velocity_rad_s,
            witness.installed_angular_velocity_rad_s,
        )
        || after
            .parts
            .iter()
            .zip(&witness.frozen_local_poses)
            .any(|(part, (part_id, pose))| {
                part.part_id != *part_id || part.frozen_local_pose != *pose
            })
    {
        return false;
    }
    let Some(component) = after.components.first() else {
        return false;
    };
    same_f64_vec3(component.translation_m, witness.root_translation_m)
        && same_f32_array(component.rotation_xyzw, witness.root_rotation_xyzw)
        && same_f32_array(
            component.linear_velocity_m_s,
            witness.installed_linear_velocity_m_s,
        )
        && same_f32_array(
            component.angular_velocity_rad_s,
            witness.installed_angular_velocity_rad_s,
        )
}

fn same_f32_array<const N: usize>(left: [f32; N], right: [f32; N]) -> bool {
    left.into_iter()
        .zip(right)
        .all(|(a, b)| canonical_f32(a) == canonical_f32(b))
}
fn same_f64_vec3(left: [f64; 3], right: [f64; 3]) -> bool {
    left.into_iter()
        .zip(right)
        .all(|(a, b)| canonical_f64(a) == canonical_f64(b))
}

fn material_at(cells: &BaselineCells, index: usize) -> Option<u16> {
    match cells {
        BaselineCells::Uniform(id) => Some(*id),
        BaselineCells::Dense(ids) => ids.get(index).copied(),
        BaselineCells::Digest { .. } => None,
    }
}

fn global_cell_for_index(coord: [i64; 3], index: usize) -> Option<[i32; 3]> {
    if index >= CELLS_PER_BRICK {
        return None;
    }
    let local = [
        (index % 32) as i64,
        ((index / 32) % 32) as i64,
        (index / (32 * 32)) as i64,
    ];
    let mut result = [0i32; 3];
    for axis in 0..3 {
        let cell = coord[axis].checked_mul(32)?.checked_add(local[axis])?;
        result[axis] = i32::try_from(cell).ok()?;
    }
    Some(result)
}

fn coord_order(coord: [i64; 3]) -> (i64, i64, i64) {
    (coord[2], coord[1], coord[0])
}
fn mask_order(mask: &CreatureSourceMaskBrickV1) -> (u64, u32, i64, i64, i64) {
    (
        mask.volume_id.get(),
        mask.part_id,
        mask.coord[2],
        mask.coord[1],
        mask.coord[0],
    )
}
fn component_order(component: &CreatureOwnedComponentV1) -> (u128, u64) {
    (component.owner_key, component.serial.unwrap_or(0))
}
fn within_bounds(bounds: [[i64; 3]; 2], coord: [i64; 3]) -> bool {
    (0..3).all(|axis| bounds[0][axis] <= coord[axis] && coord[axis] <= bounds[1][axis])
}
fn valid_bounds(bounds: Option<[[i64; 3]; 2]>) -> bool {
    bounds.is_none_or(|b| (0..3).all(|axis| b[0][axis] <= b[1][axis]))
}

fn valid_local_pose(pose: CreatureLocalPoseV1) -> bool {
    pose.translation_m
        .iter()
        .all(|v| v.is_finite() && v.abs() <= MAX_CREATURE_POSITION_M as f32)
        && valid_quaternion(pose.rotation_xyzw)
}
fn valid_quaternion(q: [f32; 4]) -> bool {
    if q.iter().any(|v| !v.is_finite()) {
        return false;
    }
    let n = q
        .iter()
        .fold(0.0f64, |sum, v| sum + f64::from(*v) * f64::from(*v))
        .sqrt();
    (n - 1.0).abs() <= 1.0e-4
}
fn validate_component_motion(
    component: &CreatureOwnedComponentV1,
) -> Result<(), CreatureOwnershipError> {
    if component.owner_key == 0
        || component
            .translation_m
            .iter()
            .any(|v| !v.is_finite() || v.abs() > MAX_CREATURE_POSITION_M)
        || !valid_quaternion(component.rotation_xyzw)
        || component
            .linear_velocity_m_s
            .iter()
            .any(|v| !v.is_finite() || v.abs() > MAX_CREATURE_LINEAR_VELOCITY_M_S)
        || component
            .angular_velocity_rad_s
            .iter()
            .any(|v| !v.is_finite() || v.abs() > MAX_CREATURE_ANGULAR_VELOCITY_RAD_S)
    {
        return Err(CreatureOwnershipError::Motion);
    }
    Ok(())
}
fn write_bounds(bounds: Option<[[i64; 3]; 2]>, w: &mut CanonicalWriter) {
    match bounds {
        None => {
            w.u8(0);
        }
        Some(bounds) => {
            w.u8(1);
            for point in bounds {
                for coordinate in point {
                    w.i64(coordinate);
                }
            }
        }
    }
}
fn write_option_u32(value: Option<u32>, w: &mut CanonicalWriter) {
    match value {
        None => {
            w.u8(0);
        }
        Some(v) => {
            w.u8(1).u32(v);
        }
    }
}
fn write_option_u64(value: Option<u64>, w: &mut CanonicalWriter) {
    match value {
        None => {
            w.u8(0);
        }
        Some(v) => {
            w.u8(1).u64(v);
        }
    }
}
fn write_local_pose(pose: CreatureLocalPoseV1, w: &mut CanonicalWriter) {
    for v in pose.translation_m {
        w.u32(canonical_f32(v));
    }
    write_quat(pose.rotation_xyzw, w);
}
fn write_quat(value: [f32; 4], w: &mut CanonicalWriter) {
    for v in value {
        w.u32(canonical_f32(v));
    }
}
fn write_vec3_f32(value: [f32; 3], w: &mut CanonicalWriter) {
    for v in value {
        w.u32(canonical_f32(v));
    }
}
fn write_vec3_f64(value: [f64; 3], w: &mut CanonicalWriter) {
    for v in value {
        w.u64(canonical_f64(v));
    }
}
fn canonical_f32(value: f32) -> u32 {
    if value == 0.0 { 0 } else { value.to_bits() }
}
fn canonical_f64(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}
fn write_creature_state(state: &CreatureStateV1, w: &mut CanonicalWriter) {
    w.u16(state.schema)
        .u128(state.key.world_id.to_u128())
        .u64(state.key.world_epoch.get())
        .u64(state.key.creature_id.get())
        .u32(state.key.generation.get())
        .u64(state.state_revision)
        .u8(state.lifecycle as u8)
        .blob(state.species_key.as_bytes())
        .u32(state.species_version);
    w.blob(&state.asset.uuid)
        .blob(&state.asset.logical_hash.0)
        .blob(&state.material_manifest_hash.0)
        .u32(state.material_mapping_version)
        .blob(&state.material_mapping_hash.0)
        .blob(state.clip_name.as_bytes())
        .u8(state.clip_mode as u8)
        .u64(state.clip_phase_tick)
        .u32(state.clip_subtick_remainder)
        .u64(state.clip_epoch);
    let mut bindings = state.source_bindings.iter().collect::<Vec<_>>();
    bindings.sort_by_key(|entry| entry.part_id);
    w.u32(bindings.len() as u32);
    for entry in bindings {
        w.u32(entry.part_id)
            .u64(entry.source_volume_id.get())
            .u64(entry.source_revision);
    }
    w.u64(state.aggregate_topology_revision)
        .u64(state.pose_epoch);
    write_vec3_f64(state.root_translation_m, w);
    write_quat(state.root_rotation_xyzw, w);
    write_vec3_f32(state.linear_velocity_m_s, w);
    write_vec3_f32(state.angular_velocity_rad_s, w);
    w.u8(state.locomotion_tag);
}
