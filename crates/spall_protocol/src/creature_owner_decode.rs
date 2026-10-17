//! Cumulative, allocation-bounded deserialization for creature ownership.
//!
//! This module is a child of `creature_owner` so it can use the portable DTOs
//! and their private limits. It deliberately does not validate semantic
//! invariants; the caller runs the ordinary record validator after decoding.

use super::*;
use serde::de::{
    self, DeserializeSeed, EnumAccess, Error as _, MapAccess, SeqAccess, VariantAccess, Visitor,
};
use std::{fmt, marker::PhantomData};

const MAX_DECODE_BYTES: usize = MAX_CREATURE_OWNER_RECORD_BYTES;
const MAX_DECODE_WORK: usize = MAX_CREATURE_OWNER_DENSE_CELLS
    + MAX_CREATURE_OWNER_MASK_CELLS
    + MAX_CREATURE_OWNER_PARTS
    + MAX_CREATURE_OWNER_BRICKS
    + MAX_CREATURE_OWNER_COMPONENTS
    + MAX_CREATURE_OWNER_MASK_BRICKS;
const RECORD_FIXED_BYTES: usize = 16 * 1024;

struct ExpectedStruct(&'static str);
impl de::Expected for ExpectedStruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "struct {}", self.0)
    }
}

#[derive(Clone, Copy)]
enum Counter {
    Parts,
    Bricks,
    DenseCells,
    Components,
    Masks,
    MaskCells,
}

#[derive(Default)]
struct DecodeBudget {
    parts: usize,
    bricks: usize,
    dense_cells: usize,
    components: usize,
    masks: usize,
    mask_cells: usize,
    solid_cells: usize,
    bytes: usize,
    work: usize,
}

impl DecodeBudget {
    fn new<E: de::Error>() -> Result<Self, E> {
        let mut budget = Self::default();
        budget.reserve_bytes(RECORD_FIXED_BYTES)?;
        Ok(budget)
    }

    fn count(&self, counter: Counter) -> usize {
        match counter {
            Counter::Parts => self.parts,
            Counter::Bricks => self.bricks,
            Counter::DenseCells => self.dense_cells,
            Counter::Components => self.components,
            Counter::Masks => self.masks,
            Counter::MaskCells => self.mask_cells,
        }
    }

    fn limit(counter: Counter) -> usize {
        match counter {
            Counter::Parts => MAX_CREATURE_OWNER_PARTS,
            Counter::Bricks => MAX_CREATURE_OWNER_BRICKS,
            Counter::DenseCells => MAX_CREATURE_OWNER_DENSE_CELLS,
            Counter::Components => MAX_CREATURE_OWNER_COMPONENTS,
            Counter::Masks => MAX_CREATURE_OWNER_MASK_BRICKS,
            Counter::MaskCells => MAX_CREATURE_OWNER_MASK_CELLS,
        }
    }

    fn remaining(&self, counter: Counter) -> usize {
        Self::limit(counter).saturating_sub(self.count(counter))
    }

    fn add_count(&mut self, counter: Counter, amount: usize) {
        match counter {
            Counter::Parts => self.parts += amount,
            Counter::Bricks => self.bricks += amount,
            Counter::DenseCells => self.dense_cells += amount,
            Counter::Components => self.components += amount,
            Counter::Masks => self.masks += amount,
            Counter::MaskCells => self.mask_cells += amount,
        }
    }

    fn reserve_bytes<E: de::Error>(&mut self, amount: usize) -> Result<(), E> {
        let bytes = self
            .bytes
            .checked_add(amount)
            .filter(|bytes| *bytes <= MAX_DECODE_BYTES)
            .ok_or_else(|| E::custom("creature ownership decoded-byte budget exceeded"))?;
        self.bytes = bytes;
        Ok(())
    }

    fn add_work<E: de::Error>(&mut self, amount: usize) -> Result<(), E> {
        let work = self
            .work
            .checked_add(amount)
            .filter(|work| *work <= MAX_DECODE_WORK)
            .ok_or_else(|| E::custom("creature ownership decode-work budget exceeded"))?;
        self.work = work;
        Ok(())
    }

    fn charge<E: de::Error>(
        &mut self,
        counter: Counter,
        byte_cost: usize,
        work_cost: usize,
    ) -> Result<(), E> {
        if self.remaining(counter) == 0 {
            return Err(E::custom(
                "creature ownership cumulative count limit exceeded",
            ));
        }
        self.reserve_bytes(byte_cost)?;
        self.add_work(work_cost)?;
        self.add_count(counter, 1);
        Ok(())
    }

    fn reserve_dense_cells<E: de::Error>(&mut self) -> Result<(), E> {
        let count = CELLS_PER_BRICK;
        if count > self.remaining(Counter::DenseCells) {
            return Err(E::custom("creature ownership dense-cell limit exceeded"));
        }
        let bytes = count
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| E::custom("creature ownership decoded-byte budget exceeded"))?;
        self.reserve_bytes(bytes)?;
        self.add_work(count)?;
        self.add_count(Counter::DenseCells, count);
        Ok(())
    }

    fn add_solid_cells<E: de::Error>(&mut self, amount: usize) -> Result<(), E> {
        let count = self
            .solid_cells
            .checked_add(amount)
            .filter(|count| *count <= MAX_CREATURE_OWNER_SOLID_CELLS)
            .ok_or_else(|| E::custom("creature ownership solid-cell limit exceeded"))?;
        self.solid_cells = count;
        Ok(())
    }
}

trait BudgetDecode<'de>: Sized {
    fn decode<D: Deserializer<'de>>(
        deserializer: D,
        budget: &mut DecodeBudget,
    ) -> Result<Self, D::Error>;
}

struct ElementSeed<'a, T> {
    budget: &'a mut DecodeBudget,
    counter: Counter,
    byte_cost: usize,
    work_cost: usize,
    marker: PhantomData<T>,
}

impl<'de, 'a, T> DeserializeSeed<'de> for ElementSeed<'a, T>
where
    T: BudgetDecode<'de>,
{
    type Value = T;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<T, D::Error> {
        // SeqAccess invokes this only after it has confirmed an element exists.
        // Thus an exact-cap sequence ending at EOF succeeds; an excess element
        // fails here before its object payload is read.
        self.budget
            .charge(self.counter, self.byte_cost, self.work_cost)?;
        T::decode(deserializer, self.budget)
    }
}

struct CountedVecSeed<'a, T> {
    budget: &'a mut DecodeBudget,
    counter: Counter,
    max: usize,
    label: &'static str,
    marker: PhantomData<T>,
}

impl<'de, 'a, T> DeserializeSeed<'de> for CountedVecSeed<'a, T>
where
    T: BudgetDecode<'de>,
{
    type Value = Vec<T>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<T>, D::Error> {
        deserializer.deserialize_seq(CountedVecVisitor {
            budget: self.budget,
            counter: self.counter,
            max: self.max,
            label: self.label,
            marker: PhantomData,
        })
    }
}

struct CountedVecVisitor<'a, T> {
    budget: &'a mut DecodeBudget,
    counter: Counter,
    max: usize,
    label: &'static str,
    marker: PhantomData<T>,
}

impl<'de, 'a, T> Visitor<'de> for CountedVecVisitor<'a, T>
where
    T: BudgetDecode<'de>,
{
    type Value = Vec<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a bounded sequence of {}", self.label)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
        let remaining = self.budget.remaining(self.counter).min(self.max);
        if seq.size_hint().is_some_and(|declared| declared > remaining) {
            return Err(A::Error::custom(format!(
                "{} cumulative count limit exceeded",
                self.label
            )));
        }

        // Do not trust the sequence's declared size for permission to exceed a
        // shared budget. Capacity and accounting are both checked before any
        // nested element is decoded.
        let mut out = Vec::new();
        if let Some(declared) = seq.size_hint() {
            let reserve = declared.min(remaining);
            if reserve != 0 {
                let bytes = reserve
                    .checked_mul(std::mem::size_of::<T>())
                    .ok_or_else(|| {
                        A::Error::custom("creature ownership decoded-byte budget exceeded")
                    })?;
                self.budget.reserve_bytes(bytes)?;
                out.try_reserve_exact(reserve).map_err(|_| {
                    A::Error::custom("creature ownership allocation limit exceeded")
                })?;
            }
        }

        loop {
            let needs_slot = out.len() == out.capacity();
            let item = seq.next_element_seed(ElementSeed::<T> {
                budget: self.budget,
                counter: self.counter,
                // For unknown-size sequences, account for the inline item
                // before its payload is decoded. Capacity growth follows only
                // after SeqAccess confirms there was an item.
                byte_cost: if needs_slot {
                    std::mem::size_of::<T>()
                } else {
                    0
                },
                work_cost: 1,
                marker: PhantomData,
            })?;
            match item {
                None => break,
                Some(item) => {
                    if needs_slot {
                        let remaining = self.budget.remaining(self.counter).min(self.max);
                        let additional = if out.capacity() == 0 {
                            remaining.saturating_add(1).min(8)
                        } else {
                            remaining.saturating_add(1).min(out.capacity())
                        };
                        let extra_items = additional.saturating_sub(1);
                        let bytes = extra_items
                            .checked_mul(std::mem::size_of::<T>())
                            .ok_or_else(|| {
                                A::Error::custom("creature ownership decoded-byte budget exceeded")
                            })?;
                        self.budget.reserve_bytes(bytes)?;
                        out.try_reserve_exact(additional).map_err(|_| {
                            A::Error::custom("creature ownership allocation limit exceeded")
                        })?;
                    }
                    out.push(item);
                }
            }
        }
        Ok(out)
    }
}

struct PlainSeed<T>(PhantomData<T>);

impl<T> PlainSeed<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'de, T: Deserialize<'de>> DeserializeSeed<'de> for PlainSeed<T> {
    type Value = T;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<T, D::Error> {
        T::deserialize(deserializer)
    }
}

macro_rules! map_field_value {
    ($field:ident, $wire:literal, $access:ident) => {
        $field.ok_or_else(|| $access::Error::missing_field($wire))?
    };
    ($field:ident, $wire:literal, $access:ident, default $default:expr) => {
        $field.unwrap_or($default)
    };
}

macro_rules! seeded_struct {
    (
        $seed:ident, $visitor:ident, $field_enum:ident, $output:ident, $type_name:literal, $budget:ident,
        { $($field:ident => $variant:ident ($wire:literal): $field_ty:ty = $field_seed:expr $(; default $field_default:expr)?),+ $(,)? }
    ) => {
        #[derive(Deserialize)]
        #[serde(field_identifier)]
        enum $field_enum { $(#[serde(rename = $wire)] $variant),+, #[serde(other)] Ignored }


        impl<'de> BudgetDecode<'de> for $output {
            fn decode<D: Deserializer<'de>>(
                deserializer: D,
                budget: &mut DecodeBudget,
            ) -> Result<Self, D::Error> {
                deserializer.deserialize_struct(
                    $type_name,
                    &[$($wire),+],
                    $visitor { budget },
                )
            }
        }

        struct $visitor<'a> { budget: &'a mut DecodeBudget }

        impl<'de, 'a> Visitor<'de> for $visitor<'a> {
            type Value = $output;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "struct {}", $type_name)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let $budget = self.budget;
                let mut index = 0usize;
                $(
                    let $field: Option<$field_ty> = seq.next_element_seed($field_seed)?;
                    if $field.is_none() { return Err(A::Error::invalid_length(index, &ExpectedStruct($type_name))); }
                    index += 1;
                )+
                let _ = index;
                Ok($output { $($field: $field.ok_or_else(|| A::Error::missing_field($wire))?),+ })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let $budget = self.budget;
                $(let mut $field: Option<$field_ty> = None;)+
                while let Some(key) = map.next_key::<$field_enum>()? {
                    match key {
                        $($field_enum::$variant => {
                            if $field.is_some() { return Err(A::Error::duplicate_field($wire)); }
                            $field = Some(map.next_value_seed($field_seed)?);
                        },)+
                        $field_enum::Ignored => { let _: de::IgnoredAny = map.next_value()?; }
                    }
                }
                Ok($output { $($field: map_field_value!($field, $wire, A $(, default $field_default)?)),+ })
            }
        }
    };
}

struct PartsSeed<'a> {
    budget: &'a mut DecodeBudget,
}
struct BricksSeed<'a> {
    budget: &'a mut DecodeBudget,
}
struct ComponentsSeed<'a> {
    budget: &'a mut DecodeBudget,
}
struct MasksSeed<'a> {
    budget: &'a mut DecodeBudget,
}
struct MaskCellsSeed<'a> {
    budget: &'a mut DecodeBudget,
}

impl<'de, 'a> DeserializeSeed<'de> for PartsSeed<'a> {
    type Value = Vec<CreatureSourcePartV1>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        CountedVecSeed::<CreatureSourcePartV1> {
            budget: self.budget,
            counter: Counter::Parts,
            max: MAX_CREATURE_OWNER_PARTS,
            label: "source parts",
            marker: PhantomData,
        }
        .deserialize(d)
    }
}
impl<'de, 'a> DeserializeSeed<'de> for BricksSeed<'a> {
    type Value = Vec<BaselineBrick>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        CountedVecSeed::<BaselineBrick> {
            budget: self.budget,
            counter: Counter::Bricks,
            max: MAX_CREATURE_OWNER_BRICKS,
            label: "source bricks",
            marker: PhantomData,
        }
        .deserialize(d)
    }
}
impl<'de, 'a> DeserializeSeed<'de> for ComponentsSeed<'a> {
    type Value = Vec<CreatureOwnedComponentV1>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        CountedVecSeed::<CreatureOwnedComponentV1> {
            budget: self.budget,
            counter: Counter::Components,
            max: MAX_CREATURE_OWNER_COMPONENTS,
            label: "components",
            marker: PhantomData,
        }
        .deserialize(d)
    }
}
impl<'de, 'a> DeserializeSeed<'de> for MasksSeed<'a> {
    type Value = Vec<CreatureSourceMaskBrickV1>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        CountedVecSeed::<CreatureSourceMaskBrickV1> {
            budget: self.budget,
            counter: Counter::Masks,
            max: MAX_CREATURE_OWNER_MASK_BRICKS,
            label: "mask bricks",
            marker: PhantomData,
        }
        .deserialize(d)
    }
}
impl<'de, 'a> DeserializeSeed<'de> for MaskCellsSeed<'a> {
    type Value = Vec<CreatureMaskCellV1>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        CountedVecSeed::<CreatureMaskCellV1> {
            budget: self.budget,
            counter: Counter::MaskCells,
            max: CELLS_PER_BRICK,
            label: "mask cells",
            marker: PhantomData,
        }
        .deserialize(d)
    }
}

seeded_struct!(
    RecordSeed, RecordVisitor, RecordField, CreatureOwnershipRecordV1, "CreatureOwnershipRecordV1", budget,
    {
        schema => Schema ("schema"): u16 = PlainSeed::<u16>::new(),
        state => State ("state"): CreatureStateV1 = PlainSeed::<CreatureStateV1>::new(),
        owner_key => OwnerKey ("owner_key"): u128 = PlainSeed::<u128>::new(),
        source_generation => SourceGeneration ("source_generation"): u64 = PlainSeed::<u64>::new(),
        next_component_serial => NextComponentSerial ("next_component_serial"): u64 = PlainSeed::<u64>::new(),
        parts => Parts ("parts"): Vec<CreatureSourcePartV1> = PartsSeed { budget: &mut *budget },
        components => Components ("components"): Vec<CreatureOwnedComponentV1> = ComponentsSeed { budget: &mut *budget },
    }
);

seeded_struct!(
    PartSeed, PartVisitor, PartField, CreatureSourcePartV1, "CreatureSourcePartV1", budget,
    {
        part_id => PartId ("part_id"): u32 = PlainSeed::<u32>::new(),
        source_volume_id => SourceVolumeId ("source_volume_id"): VolumeId = PlainSeed::<VolumeId>::new(),
        source_generation => SourceGeneration ("source_generation"): u64 = PlainSeed::<u64>::new(),
        source_revision => SourceRevision ("source_revision"): u64 = PlainSeed::<u64>::new(),
        cell_size_code => CellSizeCode ("cell_size_code"): u8 = PlainSeed::<u8>::new(),
        bounds => Bounds ("bounds"): Option<[[i64; 3]; 2]> = PlainSeed::<Option<[[i64; 3]; 2]>>::new(); default None,
        source_content_hash => SourceContentHash ("source_content_hash"): Hash32 = PlainSeed::<Hash32>::new(),
        parent_id => ParentId ("parent_id"): Option<u32> = PlainSeed::<Option<u32>>::new(); default None,
        initial_local_pose => InitialLocalPose ("initial_local_pose"): CreatureLocalPoseV1 = PlainSeed::<CreatureLocalPoseV1>::new(),
        frozen_local_pose => FrozenLocalPose ("frozen_local_pose"): CreatureLocalPoseV1 = PlainSeed::<CreatureLocalPoseV1>::new(),
        bricks => Bricks ("bricks"): Vec<BaselineBrick> = BricksSeed { budget: &mut *budget },
    }
);

seeded_struct!(
    ComponentSeed, ComponentVisitor, ComponentField, CreatureOwnedComponentV1, "CreatureOwnedComponentV1", budget,
    {
        owner_key => OwnerKey ("owner_key"): u128 = PlainSeed::<u128>::new(),
        serial => Serial ("serial"): Option<u64> = PlainSeed::<Option<u64>>::new(); default None,
        translation_m => TranslationM ("translation_m"): [f64; 3] = PlainSeed::<[f64; 3]>::new(),
        rotation_xyzw => RotationXyzw ("rotation_xyzw"): [f32; 4] = PlainSeed::<[f32; 4]>::new(),
        linear_velocity_m_s => LinearVelocityMS ("linear_velocity_m_s"): [f32; 3] = PlainSeed::<[f32; 3]>::new(),
        angular_velocity_rad_s => AngularVelocityRadS ("angular_velocity_rad_s"): [f32; 3] = PlainSeed::<[f32; 3]>::new(),
        source_masks => SourceMasks ("source_masks"): Vec<CreatureSourceMaskBrickV1> = MasksSeed { budget: &mut *budget },
    }
);

seeded_struct!(
    MaskSeed, MaskVisitor, MaskField, CreatureSourceMaskBrickV1, "CreatureSourceMaskBrickV1", budget,
    {
        volume_id => VolumeId ("volume_id"): VolumeId = PlainSeed::<VolumeId>::new(),
        part_id => PartId ("part_id"): u32 = PlainSeed::<u32>::new(),
        coord => Coord ("coord"): [i64; 3] = PlainSeed::<[i64; 3]>::new(),
        source_revision => SourceRevision ("source_revision"): u64 = PlainSeed::<u64>::new(),
        brick_revision => BrickRevision ("brick_revision"): u64 = PlainSeed::<u64>::new(),
        cells => Cells ("cells"): Vec<CreatureMaskCellV1> = MaskCellsSeed { budget: &mut *budget },
    }
);

seeded_struct!(
    BrickSeed, BrickVisitor, BrickField, BaselineBrick, "BaselineBrick", budget,
    {
        coord => Coord ("coord"): [i64; 3] = PlainSeed::<[i64; 3]>::new(),
        revision => Revision ("revision"): u64 = PlainSeed::<u64>::new(),
        edited => Edited ("edited"): bool = PlainSeed::<bool>::new(),
        cells => Cells ("cells"): BaselineCells = CellsSeed { budget: &mut *budget },
    }
);

impl<'de> BudgetDecode<'de> for CreatureMaskCellV1 {
    fn decode<D: Deserializer<'de>>(d: D, _budget: &mut DecodeBudget) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            index: u16,
            material_id: u16,
        }
        let wire = Wire::deserialize(d)?;
        Ok(Self {
            index: wire.index,
            material_id: wire.material_id,
        })
    }
}

#[derive(Deserialize)]
enum CellsVariant {
    Uniform,
    Dense,
    Digest,
}

struct CellsSeed<'a> {
    budget: &'a mut DecodeBudget,
}
impl<'de, 'a> DeserializeSeed<'de> for CellsSeed<'a> {
    type Value = BaselineCells;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_enum(
            "BaselineCells",
            &["Uniform", "Dense", "Digest"],
            CellsVisitor {
                budget: self.budget,
            },
        )
    }
}
struct CellsVisitor<'a> {
    budget: &'a mut DecodeBudget,
}
impl<'de, 'a> Visitor<'de> for CellsVisitor<'a> {
    type Value = BaselineCells;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("baseline cell storage")
    }
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        let (variant, access) = data.variant::<CellsVariant>()?;
        match variant {
            CellsVariant::Uniform => {
                let id = access.newtype_variant::<u16>()?;
                if id != 0 {
                    self.budget.add_solid_cells(CELLS_PER_BRICK)?;
                }
                Ok(BaselineCells::Uniform(id))
            }
            CellsVariant::Dense => access
                .newtype_variant_seed(DenseSeed {
                    budget: self.budget,
                })
                .map(BaselineCells::Dense),
            CellsVariant::Digest => access
                .struct_variant(&["content_hash", "solid_cells"], DigestVisitor)
                .map(|(content_hash, solid_cells)| BaselineCells::Digest {
                    content_hash,
                    solid_cells,
                }),
        }
    }
}

struct DenseSeed<'a> {
    budget: &'a mut DecodeBudget,
}
impl<'de, 'a> DeserializeSeed<'de> for DenseSeed<'a> {
    type Value = Vec<u16>;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Vec<u16>, D::Error> {
        self.budget.reserve_dense_cells::<D::Error>()?;
        d.deserialize_seq(DenseVisitor {
            budget: self.budget,
        })
    }
}
struct DenseVisitor<'a> {
    budget: &'a mut DecodeBudget,
}
impl<'de, 'a> Visitor<'de> for DenseVisitor<'a> {
    type Value = Vec<u16>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exactly {CELLS_PER_BRICK} material ids")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u16>, A::Error> {
        if seq.size_hint().is_some_and(|n| n != CELLS_PER_BRICK) {
            return Err(A::Error::custom("dense brick cell count is invalid"));
        }
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(CELLS_PER_BRICK)
            .map_err(|_| A::Error::custom("creature ownership allocation limit exceeded"))?;
        while cells.len() < CELLS_PER_BRICK {
            let Some(id) = seq.next_element::<u16>()? else {
                break;
            };
            if id != 0 {
                self.budget.add_solid_cells::<A::Error>(1)?;
            }
            cells.push(id);
        }
        if cells.len() != CELLS_PER_BRICK {
            return Err(A::Error::custom("dense brick cell count is invalid"));
        }
        if seq.next_element_seed(RejectExtraSeed)?.is_some() {
            return Err(A::Error::custom("dense brick cell count is invalid"));
        }
        Ok(cells)
    }
}

struct RejectExtraSeed;
impl<'de> DeserializeSeed<'de> for RejectExtraSeed {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, _d: D) -> Result<(), D::Error> {
        Err(D::Error::custom("sequence exceeds its declared limit"))
    }
}

struct DigestVisitor;
impl<'de> Visitor<'de> for DigestVisitor {
    type Value = (Hash32, u32);
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("digest baseline cells")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let hash = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(0, &self))?;
        let cells = seq
            .next_element()?
            .ok_or_else(|| A::Error::invalid_length(1, &self))?;
        Ok((hash, cells))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut hash = None;
        let mut cells = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "content_hash" => {
                    if hash.is_some() {
                        return Err(A::Error::duplicate_field("content_hash"));
                    }
                    hash = Some(map.next_value()?);
                }
                "solid_cells" => {
                    if cells.is_some() {
                        return Err(A::Error::duplicate_field("solid_cells"));
                    }
                    cells = Some(map.next_value()?);
                }
                _ => {
                    let _: de::IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok((
            hash.ok_or_else(|| A::Error::missing_field("content_hash"))?,
            cells.ok_or_else(|| A::Error::missing_field("solid_cells"))?,
        ))
    }
}

/// Decode one portable record while sharing count, work, and decoded-memory
/// budgets across every nested sequence.
pub(super) fn deserialize_record<'de, D>(
    deserializer: D,
) -> Result<CreatureOwnershipRecordV1, D::Error>
where
    D: Deserializer<'de>,
{
    let mut budget = DecodeBudget::new::<D::Error>()?;
    <CreatureOwnershipRecordV1 as BudgetDecode>::decode(deserializer, &mut budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::value;
    use std::cell::Cell;
    use std::rc::Rc;

    struct HintDeserializer {
        declared: usize,
        accesses: Rc<Cell<usize>>,
    }

    impl<'de> Deserializer<'de> for HintDeserializer {
        type Error = value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.deserialize_seq(visitor)
        }

        fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            visitor.visit_seq(ProbeSeq {
                hint: Some(self.declared),
                present: true,
                accesses: self.accesses,
                payload_reads: Rc::new(Cell::new(0)),
            })
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct tuple tuple_struct
            map struct enum identifier ignored_any
        }
    }

    struct ProbeSeq {
        hint: Option<usize>,
        present: bool,
        accesses: Rc<Cell<usize>>,
        payload_reads: Rc<Cell<usize>>,
    }

    impl<'de> SeqAccess<'de> for ProbeSeq {
        type Error = value::Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>, Self::Error> {
            self.accesses.set(self.accesses.get() + 1);
            if !self.present {
                return Ok(None);
            }
            self.present = false;
            seed.deserialize(ProbeDeserializer(self.payload_reads.clone()))
                .map(Some)
        }

        fn size_hint(&self) -> Option<usize> {
            self.hint
        }
    }

    struct ProbeDeserializer(Rc<Cell<usize>>);
    impl<'de> Deserializer<'de> for ProbeDeserializer {
        type Error = value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.0.set(self.0.get() + 1);
            visitor.visit_unit()
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
            map struct enum identifier ignored_any
        }
    }

    #[test]
    fn declared_cumulative_nested_count_rejects_before_reading_elements() {
        let accesses = Rc::new(Cell::new(0));
        let mut budget = DecodeBudget {
            parts: MAX_CREATURE_OWNER_PARTS,
            ..DecodeBudget::default()
        };
        let result = CountedVecSeed::<CreatureSourcePartV1> {
            budget: &mut budget,
            counter: Counter::Parts,
            max: MAX_CREATURE_OWNER_PARTS,
            label: "source parts",
            marker: PhantomData,
        }
        .deserialize(HintDeserializer {
            declared: 1,
            accesses: accesses.clone(),
        });

        assert!(result.is_err());
        assert_eq!(
            accesses.get(),
            0,
            "the declared excess must fail before SeqAccess reads an element"
        );
    }

    #[test]
    fn unknown_size_exact_cap_eof_succeeds_and_excess_payload_is_not_decoded() {
        let mut budget = DecodeBudget {
            parts: MAX_CREATURE_OWNER_PARTS,
            ..DecodeBudget::default()
        };
        let accesses = Rc::new(Cell::new(0));
        let payload_reads = Rc::new(Cell::new(0));
        let exact = CountedVecSeed::<CreatureSourcePartV1> {
            budget: &mut budget,
            counter: Counter::Parts,
            max: MAX_CREATURE_OWNER_PARTS,
            label: "source parts",
            marker: PhantomData,
        }
        .deserialize(ProbeDeserializerForSeq {
            hint: None,
            present: false,
            accesses: accesses.clone(),
            payload_reads: payload_reads.clone(),
        });
        assert_eq!(exact.unwrap(), Vec::<CreatureSourcePartV1>::new());
        assert_eq!(payload_reads.get(), 0);

        let excess = CountedVecSeed::<CreatureSourcePartV1> {
            budget: &mut budget,
            counter: Counter::Parts,
            max: MAX_CREATURE_OWNER_PARTS,
            label: "source parts",
            marker: PhantomData,
        }
        .deserialize(ProbeDeserializerForSeq {
            hint: None,
            present: true,
            accesses: accesses.clone(),
            payload_reads: payload_reads.clone(),
        });
        assert!(excess.is_err());
        assert_eq!(
            payload_reads.get(),
            0,
            "the excess element seed must reject before decoding its payload"
        );
    }

    struct ProbeDeserializerForSeq {
        hint: Option<usize>,
        present: bool,
        accesses: Rc<Cell<usize>>,
        payload_reads: Rc<Cell<usize>>,
    }
    impl<'de> Deserializer<'de> for ProbeDeserializerForSeq {
        type Error = value::Error;
        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.deserialize_seq(visitor)
        }
        fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            visitor.visit_seq(ProbeSeq {
                hint: self.hint,
                present: self.present,
                accesses: self.accesses,
                payload_reads: self.payload_reads,
            })
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct tuple tuple_struct
            map struct enum identifier ignored_any
        }
    }

    #[test]
    fn cumulative_brick_total_rejects_across_parts_during_deserialization() {
        let bricks = |count: usize| {
            (0..count)
                .map(|x| {
                    serde_json::json!({
                        "coord": [x as i64, 0, 0],
                        "revision": 1,
                        "edited": false,
                        "cells": {"Uniform": 0}
                    })
                })
                .collect::<Vec<_>>()
        };
        let part = |part_id: u32, volume_id: u64, count: usize| {
            serde_json::json!({
                "part_id": part_id,
                "source_volume_id": volume_id,
                "source_generation": 1,
                "source_revision": 1,
                "cell_size_code": 1,
                "bounds": null,
                "source_content_hash": vec![0_u8; 32],
                "parent_id": null,
                "initial_local_pose": {"translation_m": [0.0,0.0,0.0], "rotation_xyzw": [0.0,0.0,0.0,1.0]},
                "frozen_local_pose": {"translation_m": [0.0,0.0,0.0], "rotation_xyzw": [0.0,0.0,0.0,1.0]},
                "bricks": bricks(count)
            })
        };
        let state = serde_json::json!({
            "schema": 1,
            "key": {"world_id": 9, "world_epoch": 1, "creature_id": 1, "generation": 1},
            "state_revision": 1,
            "lifecycle": "Alive",
            "species_key": "test",
            "species_version": 1,
            "asset": {"uuid": vec![1_u8; 16], "logical_hash": vec![0_u8; 32]},
            "material_manifest_hash": vec![0_u8; 32],
            "material_mapping_version": 1,
            "material_mapping_hash": vec![0_u8; 32],
            "clip_name": "idle",
            "clip_mode": "Playing",
            "clip_phase_tick": 1,
            "clip_subtick_remainder": 0,
            "clip_epoch": 1,
            "source_bindings": [
                {"part_id": 1, "source_volume_id": 100, "source_revision": 1},
                {"part_id": 2, "source_volume_id": 101, "source_revision": 1}
            ],
            "aggregate_topology_revision": 1,
            "pose_epoch": 1,
            "root_translation_m": [0.0,0.0,0.0],
            "root_rotation_xyzw": [0.0,0.0,0.0,1.0],
            "linear_velocity_m_s": [0.0,0.0,0.0],
            "angular_velocity_rad_s": [0.0,0.0,0.0],
            "locomotion_tag": 0
        });
        let input = serde_json::json!({
            "schema": 1,
            "state": state,
            "owner_key": 7,
            "source_generation": 1,
            "next_component_serial": 1,
            "parts": [part(1, 100, 2049), part(2, 101, 2048)],
            "components": []
        });
        let text = serde_json::to_string(&input).unwrap();
        let error = deserialize_record(&mut serde_json::Deserializer::from_str(&text)).unwrap_err();
        assert!(
            error.to_string().contains("cumulative count limit"),
            "unexpected error: {error}"
        );
    }

    fn optional_field_record_json() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "state": {
                "schema": 1,
                "key": {"world_id": 9, "world_epoch": 1, "creature_id": 1, "generation": 1},
                "state_revision": 1,
                "lifecycle": "Alive",
                "species_key": "test",
                "species_version": 1,
                "asset": {"uuid": vec![1_u8; 16], "logical_hash": vec![0_u8; 32]},
                "material_manifest_hash": vec![0_u8; 32],
                "material_mapping_version": 1,
                "material_mapping_hash": vec![0_u8; 32],
                "clip_name": "idle",
                "clip_mode": "Playing",
                "clip_phase_tick": 1,
                "clip_subtick_remainder": 0,
                "clip_epoch": 1,
                "source_bindings": [],
                "aggregate_topology_revision": 1,
                "pose_epoch": 1,
                "root_translation_m": [0.0, 0.0, 0.0],
                "root_rotation_xyzw": [0.0, 0.0, 0.0, 1.0],
                "linear_velocity_m_s": [0.0, 0.0, 0.0],
                "angular_velocity_rad_s": [0.0, 0.0, 0.0],
                "locomotion_tag": 0
            },
            "owner_key": 7,
            "source_generation": 1,
            "next_component_serial": 1,
            "parts": [{
                "part_id": 1,
                "source_volume_id": 100,
                "source_generation": 1,
                "source_revision": 1,
                "cell_size_code": 1,
                "source_content_hash": vec![0_u8; 32],
                "initial_local_pose": {"translation_m": [0.0,0.0,0.0], "rotation_xyzw": [0.0,0.0,0.0,1.0]},
                "frozen_local_pose": {"translation_m": [0.0,0.0,0.0], "rotation_xyzw": [0.0,0.0,0.0,1.0]},
                "bricks": []
            }],
            "components": [{
                "owner_key": 7,
                "translation_m": [0.0, 0.0, 0.0],
                "rotation_xyzw": [0.0, 0.0, 0.0, 1.0],
                "linear_velocity_m_s": [0.0, 0.0, 0.0],
                "angular_velocity_rad_s": [0.0, 0.0, 0.0],
                "source_masks": []
            }]
        })
    }

    #[test]
    fn optional_map_fields_default_to_none_but_required_fields_still_fail() {
        let decoded: CreatureOwnershipRecordV1 =
            serde_json::from_value(optional_field_record_json()).unwrap();
        assert_eq!(decoded.parts[0].bounds, None);
        assert_eq!(decoded.parts[0].parent_id, None);
        assert_eq!(decoded.components[0].serial, None);

        let mut missing_required = optional_field_record_json();
        missing_required["parts"][0]
            .as_object_mut()
            .unwrap()
            .remove("part_id");
        let error =
            serde_json::from_value::<CreatureOwnershipRecordV1>(missing_required).unwrap_err();
        assert!(
            error.to_string().contains("part_id"),
            "unexpected error: {error}"
        );
    }
}
