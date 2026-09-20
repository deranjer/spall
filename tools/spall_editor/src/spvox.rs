//! Canonical static SPVX (`.spvox`) reader/writer for the editor asset
//! boundary. Runtime voxel bricks remain derived data and are never persisted
//! here. Parts and animation deliberately fail with a clear error until the
//! editor has an authored-data model that can retain them losslessly.

use std::collections::{BTreeMap, BTreeSet};

use crate::{AssetId, EditorError, VoxelAssetFile, VoxelCoord};

const MAGIC: &[u8; 4] = b"SPVX";
const MAJOR: u16 = 1;
const MINOR: u16 = 0;
const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
const MAX_CHUNKS: usize = 64;
const MAX_DECODED_CHUNK_BYTES: usize = 128 * 1024 * 1024;
const MAX_TOTAL_DECODED_BYTES: usize = 512 * 1024 * 1024;
const MAX_TABLE_ENTRIES: usize = 4096;
const MAX_RUNS: usize = 16_000_000;
const MAX_CELLS: u64 = 64_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    x: i32,
    y: i32,
    z: i32,
    length: u32,
    material_slot: u16,
    tint_slot: u16,
}

struct Meta {
    portable_id: [u8; 16],
    cell_size_code: u8,
    pivot_subcells: [i32; 3],
    name: String,
    tags: BTreeMap<String, String>,
}

pub(super) fn encode(asset: &VoxelAssetFile) -> Result<Vec<u8>, EditorError> {
    validate_asset_metadata(asset)?;

    let mut material_keys = BTreeSet::new();
    for material in asset.voxels.values() {
        if *material == 0 {
            return invalid("air must be represented by absent SPVX cells");
        }
        let key = asset.material_keys.get(material).ok_or_else(|| {
            EditorError::Invalid(format!("material {material} has no portable SPVX key"))
        })?;
        material_keys.insert(key.clone());
    }
    if material_keys.len() > MAX_TABLE_ENTRIES {
        return invalid("SPVX material table exceeds 4,096 entries");
    }
    let material_keys: Vec<_> = material_keys.into_iter().collect();
    let material_slots: BTreeMap<_, _> = material_keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), (index + 1) as u16))
        .collect();

    let mut tint_values = BTreeSet::new();
    for cell in asset.voxels.keys() {
        if let Some(tint) = asset.colors.get(cell) {
            tint_values.insert(*tint);
        }
    }
    if tint_values.len() > MAX_TABLE_ENTRIES {
        return invalid("SPVX tint palette exceeds 4,096 entries");
    }
    let tint_values: Vec<_> = tint_values.into_iter().collect();
    let tint_slots: BTreeMap<_, _> = tint_values
        .iter()
        .enumerate()
        .map(|(index, tint)| (*tint, (index + 1) as u16))
        .collect();

    let meta = encode_meta(asset)?;
    let mtrl = encode_materials(&material_keys)?;
    let palt = (!tint_values.is_empty()).then(|| encode_palette(&tint_values));
    let voxl = encode_voxels(asset, &material_slots, &tint_slots)?;

    let mut chunks = vec![(*b"META", meta), (*b"MTRL", mtrl)];
    if let Some(palt) = palt {
        chunks.push((*b"PALT", palt));
    }
    chunks.push((*b"VOXL", voxl));

    let digest = logical_hash(&chunks);
    let mut output = Vec::with_capacity(
        12 + chunks
            .iter()
            .map(|(_, data)| 16 + data.len())
            .sum::<usize>()
            + 48,
    );
    output.extend_from_slice(MAGIC);
    push_u16(&mut output, MAJOR);
    push_u16(&mut output, MINOR);
    push_u32(&mut output, 0);
    for (id, data) in &chunks {
        push_chunk(&mut output, *id, data)?;
    }
    push_chunk(&mut output, *b"HASH", digest.as_bytes())?;
    Ok(output)
}

pub(super) fn decode(
    bytes: &[u8],
    id: AssetId,
    material_mapping: &BTreeMap<String, u16>,
) -> Result<VoxelAssetFile, EditorError> {
    if bytes.len() > MAX_FILE_BYTES {
        return invalid("SPVX file exceeds the 256 MiB limit");
    }
    let mut input = Reader::new(bytes);
    if input.array::<4>()? != *MAGIC {
        return invalid("SPVX magic is missing");
    }
    let major = input.u16()?;
    let _minor = input.u16()?;
    if major != MAJOR {
        return invalid(format!("SPVX major version {major} is unsupported"));
    }
    if input.u32()? != 0 {
        return invalid("SPVX v1 header flags must be zero");
    }

    let mut chunks: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    let mut total_raw = 0_usize;
    while !input.is_empty() {
        if chunks.len() == MAX_CHUNKS {
            return invalid("SPVX has more than 64 chunks");
        }
        let chunk_id = input.array::<4>()?;
        let codec = input.u8()?;
        if input.array::<3>()? != [0; 3] {
            return invalid("SPVX chunk reserved bytes must be zero");
        }
        let stored_len = input.u32()? as usize;
        let raw_len = input.u32()? as usize;
        if raw_len > MAX_DECODED_CHUNK_BYTES {
            return invalid("SPVX decoded chunk exceeds the 128 MiB limit");
        }
        total_raw = total_raw
            .checked_add(raw_len)
            .ok_or_else(|| EditorError::Invalid("SPVX decoded length overflow".into()))?;
        if total_raw > MAX_TOTAL_DECODED_BYTES {
            return invalid("SPVX decoded data exceeds the 512 MiB limit");
        }
        let stored = input.take(stored_len)?;
        let raw = match codec {
            0 if stored_len == raw_len => stored.to_vec(),
            0 => return invalid("raw SPVX chunk has different stored/raw lengths"),
            1 => zstd::bulk::decompress(stored, raw_len).map_err(|error| {
                EditorError::Invalid(format!("SPVX zstd chunk cannot be decoded: {error}"))
            })?,
            other => return invalid(format!("SPVX codec {other} is unsupported")),
        };
        if raw.len() != raw_len {
            return invalid("SPVX decoded chunk length does not match its header");
        }
        chunks.push((chunk_id, raw));
    }

    if chunks.len() < 4 || chunks.first().map(|chunk| chunk.0) != Some(*b"META") {
        return invalid("SPVX requires META as its first chunk");
    }
    let Some((hash_id, hash_payload)) = chunks.last() else {
        return invalid("SPVX has no HASH chunk");
    };
    if *hash_id != *b"HASH" || hash_payload.len() != 32 {
        return invalid("SPVX requires one final 32-byte HASH chunk");
    }
    let digest = logical_hash(&chunks[..chunks.len() - 1]);
    if digest.as_bytes() != hash_payload.as_slice() {
        return invalid("SPVX HASH does not match logical chunk contents");
    }

    let mut seen = BTreeSet::new();
    let mut meta = None;
    let mut materials = None;
    let mut palette = Vec::new();
    let mut voxels = None;
    for (chunk_id, payload) in &chunks[..chunks.len() - 1] {
        if !seen.insert(*chunk_id) {
            return invalid(format!("SPVX chunk {:?} is duplicated", fourcc(*chunk_id)));
        }
        match chunk_id {
            b"META" => meta = Some(decode_meta(payload)?),
            b"MTRL" => materials = Some(decode_materials(payload)?),
            b"PALT" => palette = decode_palette(payload)?,
            b"VOXL" => voxels = Some(decode_voxels(payload)?),
            b"PART" | b"ANIM" => {
                return invalid(format!(
                    "SPVX {} is valid but not yet retainable by the static editor",
                    fourcc(*chunk_id)
                ));
            }
            other => return invalid(format!("unknown SPVX chunk {}", fourcc(*other))),
        }
    }
    let Meta {
        portable_id,
        cell_size_code,
        pivot_subcells,
        name,
        tags,
    } = meta.ok_or_else(|| EditorError::Invalid("SPVX is missing META".into()))?;
    let materials = materials.ok_or_else(|| EditorError::Invalid("SPVX is missing MTRL".into()))?;
    let runs = voxels.ok_or_else(|| EditorError::Invalid("SPVX is missing VOXL".into()))?;

    let mut resolved = Vec::with_capacity(materials.len());
    let mut used_ids = BTreeSet::new();
    for key in &materials {
        let material = material_mapping.get(key).copied().ok_or_else(|| {
            EditorError::Invalid(format!(
                "SPVX material key {key:?} is not mapped by this project"
            ))
        })?;
        if material == 0 {
            return invalid(format!("SPVX material key {key:?} maps to air"));
        }
        if !used_ids.insert(material) {
            return invalid(format!(
                "project maps multiple SPVX keys to material {material}; cannot preserve re-export"
            ));
        }
        resolved.push(material);
    }
    let material_keys = materials
        .iter()
        .cloned()
        .zip(resolved.iter().copied())
        .map(|(key, material)| (material, key))
        .collect();

    let mut asset = VoxelAssetFile {
        format_version: crate::VOXEL_FORMAT_VERSION,
        id,
        portable_id,
        name,
        cell_size_code,
        pivot_subcells,
        material_keys,
        tags,
        voxels: BTreeMap::new(),
        colors: BTreeMap::new(),
    };
    let mut expanded = 0_u64;
    for run in runs {
        let material = *resolved
            .get((run.material_slot - 1) as usize)
            .ok_or_else(|| {
                EditorError::Invalid("SPVX run references unknown material slot".into())
            })?;
        let tint = if run.tint_slot == 0 {
            None
        } else {
            Some(*palette.get((run.tint_slot - 1) as usize).ok_or_else(|| {
                EditorError::Invalid("SPVX run references unknown tint slot".into())
            })?)
        };
        expanded = expanded
            .checked_add(run.length as u64)
            .ok_or_else(|| EditorError::Invalid("SPVX expanded cell count overflow".into()))?;
        if expanded > MAX_CELLS {
            return invalid("SPVX expands beyond 64,000,000 occupied cells");
        }
        for offset in 0..run.length {
            let x = run
                .x
                .checked_add(offset as i32)
                .ok_or_else(|| EditorError::Invalid("SPVX run endpoint overflows i32".into()))?;
            let cell = VoxelCoord {
                x,
                y: run.y,
                z: run.z,
            };
            if asset.voxels.insert(cell, material).is_some() {
                return invalid("SPVX runs overlap");
            }
            if let Some(tint) = tint {
                asset.colors.insert(cell, tint);
            }
        }
    }
    Ok(asset)
}

fn encode_meta(asset: &VoxelAssetFile) -> Result<Vec<u8>, EditorError> {
    let mut output = Vec::new();
    output.extend_from_slice(&asset.portable_id);
    output.push(asset.cell_size_code);
    output.push(0); // Spall right-handed, +Y-up axes.
    for value in asset.pivot_subcells {
        push_i32(&mut output, value);
    }
    push_string_u16(&mut output, &asset.name)?;
    push_u64(&mut output, 0); // required features
    push_u64(&mut output, 0); // optional features
    if asset.tags.len() > u16::MAX as usize {
        return invalid("SPVX has too many META tags");
    }
    push_u16(&mut output, asset.tags.len() as u16);
    for (key, value) in &asset.tags {
        if !valid_tag_key(key) || value.len() > u16::MAX as usize {
            return invalid(format!("invalid SPVX META tag {key:?}"));
        }
        push_string_u8(&mut output, key)?;
        push_string_u16(&mut output, value)?;
    }
    Ok(output)
}

fn decode_meta(payload: &[u8]) -> Result<Meta, EditorError> {
    let mut input = Reader::new(payload);
    let portable_id = input.array::<16>()?;
    if portable_id == [0; 16] {
        return invalid("SPVX META portable UUID may not be zero");
    }
    let cell_size_code = input.u8()?;
    if !matches!(cell_size_code, 0 | 1) {
        return invalid(format!(
            "SPVX cell size code {cell_size_code} is unsupported"
        ));
    }
    if input.u8()? != 0 {
        return invalid("SPVX axis convention is not Spall +Y-up");
    }
    let pivot_subcells = [input.i32()?, input.i32()?, input.i32()?];
    let name = input.string_u16()?;
    validate_name(&name)?;
    if input.u64()? != 0 || input.u64()? != 0 {
        return invalid("SPVX required/optional feature bits are unsupported");
    }
    let tag_count = input.u16()? as usize;
    let mut tags = BTreeMap::new();
    let mut prior = None;
    for _ in 0..tag_count {
        let key = input.string_u8()?;
        let value = input.string_u16()?;
        if !valid_tag_key(&key) || prior.as_ref().is_some_and(|last: &String| last >= &key) {
            return invalid("SPVX META tags are invalid or not canonically ordered");
        }
        prior = Some(key.clone());
        tags.insert(key, value);
    }
    input.finish()?;
    Ok(Meta {
        portable_id,
        cell_size_code,
        pivot_subcells,
        name,
        tags,
    })
}

fn encode_materials(keys: &[String]) -> Result<Vec<u8>, EditorError> {
    let mut output = Vec::new();
    push_u16(&mut output, keys.len() as u16);
    for key in keys {
        validate_material_key(key)?;
        push_string_u8(&mut output, key)?;
    }
    Ok(output)
}

fn decode_materials(payload: &[u8]) -> Result<Vec<String>, EditorError> {
    let mut input = Reader::new(payload);
    let count = input.u16()? as usize;
    if count > MAX_TABLE_ENTRIES {
        return invalid("SPVX material table exceeds 4,096 entries");
    }
    let mut result = Vec::with_capacity(count);
    let mut prior = None;
    for _ in 0..count {
        let key = input.string_u8()?;
        validate_material_key(&key)?;
        if prior.as_ref().is_some_and(|last: &String| last >= &key) {
            return invalid("SPVX material keys are not bytewise sorted and unique");
        }
        prior = Some(key.clone());
        result.push(key);
    }
    input.finish()?;
    Ok(result)
}

fn encode_palette(tints: &[[u8; 3]]) -> Vec<u8> {
    let mut output = Vec::with_capacity(2 + tints.len() * 3);
    push_u16(&mut output, tints.len() as u16);
    for tint in tints {
        output.extend_from_slice(tint);
    }
    output
}

fn decode_palette(payload: &[u8]) -> Result<Vec<[u8; 3]>, EditorError> {
    let mut input = Reader::new(payload);
    let count = input.u16()? as usize;
    if count > MAX_TABLE_ENTRIES {
        return invalid("SPVX tint palette exceeds 4,096 entries");
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        result.push(input.array::<3>()?);
    }
    input.finish()?;
    Ok(result)
}

fn encode_voxels(
    asset: &VoxelAssetFile,
    material_slots: &BTreeMap<&str, u16>,
    tint_slots: &BTreeMap<[u8; 3], u16>,
) -> Result<Vec<u8>, EditorError> {
    let mut cells: Vec<_> = asset
        .voxels
        .iter()
        .map(|(cell, material)| {
            let key = asset.material_keys.get(material).ok_or_else(|| {
                EditorError::Invalid(format!("material {material} has no portable SPVX key"))
            })?;
            let material_slot = *material_slots.get(key.as_str()).ok_or_else(|| {
                EditorError::Invalid(format!("SPVX material key {key:?} was not tabled"))
            })?;
            let tint_slot = asset
                .colors
                .get(cell)
                .map(|tint| {
                    tint_slots
                        .get(tint)
                        .copied()
                        .ok_or_else(|| EditorError::Invalid("SPVX tint was not tabled".into()))
                })
                .transpose()?
                .unwrap_or(0);
            Ok((*cell, material_slot, tint_slot))
        })
        .collect::<Result<_, EditorError>>()?;
    cells.sort_by_key(|(cell, _, _)| (cell.z, cell.y, cell.x));

    let mut runs: Vec<Run> = Vec::new();
    for (cell, material_slot, tint_slot) in cells {
        if let Some(run) = runs.last_mut()
            && run.y == cell.y
            && run.z == cell.z
            && run.material_slot == material_slot
            && run.tint_slot == tint_slot
            && run.x.checked_add(run.length as i32) == Some(cell.x)
        {
            run.length = run
                .length
                .checked_add(1)
                .ok_or_else(|| EditorError::Invalid("SPVX run length overflows u32".into()))?;
            continue;
        }
        runs.push(Run {
            x: cell.x,
            y: cell.y,
            z: cell.z,
            length: 1,
            material_slot,
            tint_slot,
        });
    }
    if runs.len() > MAX_RUNS {
        return invalid("SPVX voxel run count exceeds 16,000,000");
    }
    let mut output = Vec::with_capacity(5 + runs.len() * 24);
    output.push(0); // +X runs
    push_u32(&mut output, runs.len() as u32);
    for run in runs {
        push_u32(&mut output, 1); // implicit root part
        push_i32(&mut output, run.x);
        push_i32(&mut output, run.y);
        push_i32(&mut output, run.z);
        push_u32(&mut output, run.length);
        push_u16(&mut output, run.material_slot);
        push_u16(&mut output, run.tint_slot);
    }
    Ok(output)
}

fn decode_voxels(payload: &[u8]) -> Result<Vec<Run>, EditorError> {
    let mut input = Reader::new(payload);
    if input.u8()? != 0 {
        return invalid("SPVX VOXL encoding is not +X runs");
    }
    let count = input.u32()? as usize;
    if count > MAX_RUNS {
        return invalid("SPVX voxel run count exceeds 16,000,000");
    }
    let mut result = Vec::with_capacity(count);
    let mut prior: Option<Run> = None;
    for _ in 0..count {
        let part_id = input.u32()?;
        if part_id != 1 {
            return invalid("SPVX part data is not yet supported by the static editor");
        }
        let run = Run {
            x: input.i32()?,
            y: input.i32()?,
            z: input.i32()?,
            length: input.u32()?,
            material_slot: input.u16()?,
            tint_slot: input.u16()?,
        };
        if run.length == 0 || run.material_slot == 0 {
            return invalid("SPVX VOXL run has a zero length or material slot");
        }
        let endpoint = i64::from(run.x)
            .checked_add(i64::from(run.length) - 1)
            .ok_or_else(|| EditorError::Invalid("SPVX VOXL run endpoint overflows i32".into()))?;
        if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&endpoint) {
            return invalid("SPVX VOXL run endpoint overflows i32");
        }
        if let Some(last) = prior {
            let sorted = (last.z, last.y, last.x) < (run.z, run.y, run.x);
            if !sorted {
                return invalid("SPVX VOXL runs are not canonically sorted");
            }
            if last.z == run.z && last.y == run.y {
                let last_end = i64::from(last.x)
                    .checked_add(i64::from(last.length))
                    .ok_or_else(|| {
                        EditorError::Invalid("SPVX VOXL run endpoint overflows i32".into())
                    })?;
                if i64::from(run.x) < last_end {
                    return invalid("SPVX VOXL runs overlap");
                }
                if i64::from(run.x) == last_end
                    && last.material_slot == run.material_slot
                    && last.tint_slot == run.tint_slot
                {
                    return invalid("SPVX adjacent compatible runs must be merged");
                }
            }
        }
        prior = Some(run);
        result.push(run);
    }
    input.finish()?;
    Ok(result)
}

fn validate_asset_metadata(asset: &VoxelAssetFile) -> Result<(), EditorError> {
    if asset.portable_id == [0; 16] {
        return invalid("SPVX portable UUID may not be zero");
    }
    if !matches!(asset.cell_size_code, 0 | 1) {
        return invalid(format!(
            "unsupported cell size code {}",
            asset.cell_size_code
        ));
    }
    validate_name(&asset.name)?;
    Ok(())
}

fn validate_name(name: &str) -> Result<(), EditorError> {
    if name.is_empty() || name.len() > 255 {
        return invalid("SPVX asset name must contain 1 through 255 UTF-8 bytes");
    }
    Ok(())
}

fn validate_material_key(key: &str) -> Result<(), EditorError> {
    if key.is_empty() || key.len() > u8::MAX as usize {
        return invalid(format!("invalid SPVX material key {key:?}"));
    }
    Ok(())
}

fn valid_tag_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= u8::MAX as usize
        && key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn logical_hash(chunks: &[([u8; 4], Vec<u8>)]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"spall.asset.v1");
    for (id, payload) in chunks {
        hasher.update(id);
        hasher.update(&(payload.len() as u32).to_le_bytes());
        hasher.update(payload);
    }
    hasher.finalize()
}

fn push_chunk(output: &mut Vec<u8>, id: [u8; 4], data: &[u8]) -> Result<(), EditorError> {
    let len = u32::try_from(data.len())
        .map_err(|_| EditorError::Invalid("SPVX chunk is too large".into()))?;
    output.extend_from_slice(&id);
    output.push(0); // raw
    output.extend_from_slice(&[0; 3]);
    push_u32(output, len);
    push_u32(output, len);
    output.extend_from_slice(data);
    Ok(())
}

fn push_string_u8(output: &mut Vec<u8>, value: &str) -> Result<(), EditorError> {
    let len = u8::try_from(value.len())
        .map_err(|_| EditorError::Invalid("SPVX string exceeds u8 length".into()))?;
    output.push(len);
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn push_string_u16(output: &mut Vec<u8>, value: &str) -> Result<(), EditorError> {
    let len = u16::try_from(value.len())
        .map_err(|_| EditorError::Invalid("SPVX string exceeds u16 length".into()))?;
    push_u16(output, len);
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
fn push_i32(output: &mut Vec<u8>, value: i32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn fourcc(id: [u8; 4]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}
fn invalid<T>(message: impl Into<String>) -> Result<T, EditorError> {
    Err(EditorError::Invalid(message.into()))
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], EditorError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| EditorError::Invalid("SPVX cursor overflow".into()))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| EditorError::Invalid("SPVX is truncated".into()))?;
        self.position = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], EditorError> {
        self.take(N)?
            .try_into()
            .map_err(|_| EditorError::Invalid("SPVX fixed field is truncated".into()))
    }
    fn u8(&mut self) -> Result<u8, EditorError> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, EditorError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, EditorError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, EditorError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32, EditorError> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    fn string_u8(&mut self) -> Result<String, EditorError> {
        let len = self.u8()? as usize;
        self.string(len)
    }
    fn string_u16(&mut self) -> Result<String, EditorError> {
        let len = self.u16()? as usize;
        self.string(len)
    }
    fn string(&mut self, len: usize) -> Result<String, EditorError> {
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| EditorError::Invalid("SPVX string is not UTF-8".into()))
    }
    fn finish(&self) -> Result<(), EditorError> {
        if self.is_empty() {
            Ok(())
        } else {
            invalid("SPVX chunk has trailing bytes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset() -> VoxelAssetFile {
        let mut asset = VoxelAssetFile::new(AssetId(7), "Oak Tree");
        asset.portable_id = [7; 16];
        asset.pivot_subcells = [0, 256, -128];
        asset.tags.insert("author".into(), "Spall".into());
        asset.voxels.insert(VoxelCoord { x: -1, y: 0, z: 2 }, 2);
        asset.voxels.insert(VoxelCoord { x: 0, y: 0, z: 2 }, 2);
        asset.voxels.insert(VoxelCoord { x: 1, y: 0, z: 2 }, 3);
        asset
            .colors
            .insert(VoxelCoord { x: 1, y: 0, z: 2 }, [54, 151, 62]);
        asset
    }

    #[test]
    fn static_asset_round_trip_is_canonical_and_preserves_tints() {
        let asset = asset();
        let first = encode(&asset).unwrap();
        let loaded = decode(&first, AssetId(99), &crate::default_material_mapping()).unwrap();
        let second = encode(&loaded).unwrap();
        assert_eq!(first, second);
        assert_eq!(loaded.portable_id, [7; 16]);
        assert_eq!(loaded.id, AssetId(99));
        assert_eq!(loaded.pivot_subcells, [0, 256, -128]);
        assert_eq!(loaded.voxels, asset.voxels);
        assert_eq!(loaded.colors, asset.colors);
    }

    #[test]
    fn corrupt_hash_and_unmapped_material_are_rejected() {
        let mut bytes = encode(&asset()).unwrap();
        bytes[20] ^= 0x01;
        assert!(decode(&bytes, AssetId(1), &crate::default_material_mapping()).is_err());

        let bytes = encode(&asset()).unwrap();
        let mapping = BTreeMap::from([("stone.granite".to_owned(), 1)]);
        let error = decode(&bytes, AssetId(1), &mapping).unwrap_err();
        assert!(error.to_string().contains("not mapped"));
    }
}
