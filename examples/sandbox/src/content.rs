//! Versioned, game-owned asset manifest and bounded runtime file loader.
//!
//! The editor's project-local asset IDs identify authoring records. This
//! module's `AssetId` is the stable ID used by shipped game content. A manifest
//! maps those IDs to portable relative paths and content hashes; it never
//! serializes runtime handles or trusts file enumeration order.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use spall_core::{CellSizeCode, GlobalCell, MaterialId};
use thiserror::Error;

pub const CONTENT_MANIFEST_VERSION: u32 = 1;
pub const CONTENT_MANIFEST_FILE: &str = "content-v1.ron";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ASSETS: usize = 4096;
const MAX_SPVOX_CHUNKS: usize = 64;
const MAX_SPVOX_CHUNK_BYTES: usize = 128 * 1024 * 1024;
const MAX_SPVOX_DECODED_BYTES: usize = 512 * 1024 * 1024;
const MAX_RUNTIME_VOXELS: usize = 1_000_000;
const SPVOX_MAJOR_VERSION: u16 = 1;

/// Stable game asset identity. IDs are authored constants, never assigned by
/// directory order or runtime allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetId(pub u64);

pub mod asset_ids {
    use super::AssetId;
    pub const STARTER_CRATE: AssetId = AssetId(1);
}

/// Asset format understood by the sandbox runtime loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetFormat {
    Spvox,
}

/// One manifest entry. The BLAKE3 digest authenticates the exact file bytes
/// against accidental replacement or corruption before they reach a decoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRecord {
    pub id: AssetId,
    pub name: String,
    pub format: AssetFormat,
    /// UTF-8 path using `/` separators, relative to the manifest directory.
    pub path: String,
    pub content_hash: [u8; 32],
}

/// Canonically ordered game content manifest. The RON file is the persisted
/// representation; `canonical_hash` uses an explicit byte contract instead of
/// hashing Rust struct layout or serializer output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentManifest {
    pub format_version: u32,
    pub game_content_version: u32,
    pub assets: BTreeMap<AssetId, AssetRecord>,
}

#[derive(Debug, Error)]
pub enum ContentError {
    #[error("content manifest I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("content manifest encoding failed: {0}")]
    RonEncode(#[from] ron::Error),
    #[error("content manifest decoding failed: {0}")]
    RonDecode(#[from] ron::error::SpannedError),
    #[error("content manifest is not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("content manifest has unsupported format version {0}")]
    ManifestVersion(u32),
    #[error("game content version and asset IDs must be nonzero")]
    ZeroIdentity,
    #[error("content manifest has too many assets")]
    TooManyAssets,
    #[error("asset map key {key:?} does not match record ID {record:?}")]
    KeyMismatch { key: AssetId, record: AssetId },
    #[error("asset {0:?} has an invalid name or path")]
    InvalidMetadata(AssetId),
    #[error("asset path escapes the manifest directory: {0}")]
    PathEscape(String),
    #[error("asset {0:?} is larger than the {MAX_ASSET_BYTES} byte limit")]
    AssetTooLarge(AssetId),
    #[error("asset {0:?} has an invalid or unsupported SPVX header")]
    UnsupportedAssetFormat(AssetId),
    #[error("asset {0:?} has malformed SPVX chunks or integrity data")]
    InvalidSpvox(AssetId),
    #[error("asset {0:?} uses SPVX features this static runtime importer cannot preserve")]
    UnsupportedSpvoxFeature(AssetId),
    #[error("asset {0:?} references unmapped portable material key {1:?}")]
    UnmappedMaterial(AssetId, String),
    #[error("asset {0:?} exceeds the runtime import voxel budget")]
    TooManyRuntimeVoxels(AssetId),
    #[error("asset {0:?} does not match its manifest BLAKE3 digest")]
    HashMismatch(AssetId),
    #[error("asset {0:?} is absent from the content manifest")]
    UnknownAsset(AssetId),
    #[error("manifest is larger than the {MAX_MANIFEST_BYTES} byte limit")]
    ManifestTooLarge,
}

impl ContentManifest {
    pub fn new(
        game_content_version: u32,
        records: impl IntoIterator<Item = AssetRecord>,
    ) -> Result<Self, ContentError> {
        if game_content_version == 0 {
            return Err(ContentError::ZeroIdentity);
        }
        let mut assets = BTreeMap::new();
        for record in records {
            validate_record(&record)?;
            let id = record.id;
            if assets.insert(id, record).is_some() {
                return Err(ContentError::InvalidMetadata(id));
            }
        }
        if assets.len() > MAX_ASSETS {
            return Err(ContentError::TooManyAssets);
        }
        let manifest = Self {
            format_version: CONTENT_MANIFEST_VERSION,
            game_content_version,
            assets,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ContentError> {
        if self.format_version != CONTENT_MANIFEST_VERSION {
            return Err(ContentError::ManifestVersion(self.format_version));
        }
        if self.game_content_version == 0 {
            return Err(ContentError::ZeroIdentity);
        }
        if self.assets.len() > MAX_ASSETS {
            return Err(ContentError::TooManyAssets);
        }
        for (key, record) in &self.assets {
            if *key != record.id {
                return Err(ContentError::KeyMismatch {
                    key: *key,
                    record: record.id,
                });
            }
            validate_record(record)?;
        }
        Ok(())
    }

    /// BLAKE3 over stable IDs, versions, metadata and per-file hashes in ID
    /// order. Integer widths and byte order are explicit.
    pub fn canonical_hash(&self) -> Result<[u8; 32], ContentError> {
        self.validate()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"spall.sandbox.content-manifest.v1");
        hasher.update(&self.format_version.to_le_bytes());
        hasher.update(&self.game_content_version.to_le_bytes());
        hasher.update(&(self.assets.len() as u32).to_le_bytes());
        for record in self.assets.values() {
            hasher.update(&record.id.0.to_le_bytes());
            hasher.update(&[match record.format {
                AssetFormat::Spvox => 1,
            }]);
            hash_string(&mut hasher, &record.name);
            hash_string(&mut hasher, &record.path);
            hasher.update(&record.content_hash);
        }
        Ok(*hasher.finalize().as_bytes())
    }

    /// Writes a new immutable, versioned manifest file. `create_new` prevents
    /// accidental replacement of an already-published content version.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<(), ContentError> {
        self.validate()?;
        let path = path.as_ref();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let encoded = ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default())?;
        if encoded.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ContentError::ManifestTooLarge);
        }
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

/// Opened, read-only content root. Assets are loaded by stable ID and checked
/// against the persisted manifest before bytes are returned.
pub struct AssetStore {
    root: PathBuf,
    manifest: ContentManifest,
}

impl AssetStore {
    pub fn open(manifest_path: impl AsRef<Path>) -> Result<Self, ContentError> {
        let manifest_path = manifest_path.as_ref();
        let encoded = String::from_utf8(match read_bounded(manifest_path, MAX_MANIFEST_BYTES) {
            Ok(bytes) => bytes,
            Err(BoundedReadError::TooLarge) => return Err(ContentError::ManifestTooLarge),
            Err(BoundedReadError::Io(error)) => return Err(error.into()),
        })?;
        let manifest: ContentManifest = ron::from_str(&encoded)?;
        manifest.validate()?;
        let root = manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        Ok(Self { root, manifest })
    }

    pub fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }

    /// Loads every declared asset once so startup fails before serving if a
    /// shipped file is missing, corrupt, or no longer matches its digest.
    pub fn verify_all(&self) -> Result<(), ContentError> {
        for id in self.manifest.assets.keys().copied() {
            self.load(id)?;
        }
        Ok(())
    }

    pub fn load(&self, id: AssetId) -> Result<Vec<u8>, ContentError> {
        let record = self
            .manifest
            .assets
            .get(&id)
            .ok_or(ContentError::UnknownAsset(id))?;
        let relative = Path::new(&record.path);
        validate_relative_path(relative)
            .map_err(|()| ContentError::PathEscape(record.path.clone()))?;
        let path = self.root.join(relative).canonicalize()?;
        if !path.starts_with(&self.root) {
            return Err(ContentError::PathEscape(record.path.clone()));
        }
        let bytes = match read_bounded(&path, MAX_ASSET_BYTES) {
            Ok(bytes) => bytes,
            Err(BoundedReadError::TooLarge) => return Err(ContentError::AssetTooLarge(id)),
            Err(BoundedReadError::Io(error)) => return Err(error.into()),
        };
        validate_spvox(id, &bytes)?;
        if blake3::hash(&bytes).as_bytes() != &record.content_hash {
            return Err(ContentError::HashMismatch(id));
        }
        Ok(bytes)
    }

    /// Validates and decodes a bounded static, single-root SPVX asset into
    /// game material IDs. Assemblies, animation, structural profiles, and
    /// display tints are rejected until runtime import can preserve them.
    pub fn load_voxel_asset(
        &self,
        id: AssetId,
        material_mapping: &BTreeMap<String, MaterialId>,
    ) -> Result<LoadedVoxelAsset, ContentError> {
        let bytes = self.load(id)?;
        decode_static_voxels(id, &bytes, material_mapping)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedVoxelCell {
    pub cell: GlobalCell,
    pub material: MaterialId,
    /// The authored display tint (sRGB), if the cell has one. The static game
    /// importer rejects tints; the editor-scene importer maps it to an
    /// appearance variant of `material` (see `crate::appearance`).
    pub tint: Option<[u8; 3]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedVoxelAsset {
    pub id: AssetId,
    pub name: String,
    pub cell_size: CellSizeCode,
    pub pivot_subcells: [i32; 3],
    pub cells: Vec<LoadedVoxelCell>,
}

/// Builds a manifest record from an existing SPVX file after checking its
/// supported major format version and size.
pub fn record_spvox_file(
    id: AssetId,
    name: impl Into<String>,
    relative_path: impl Into<String>,
    bytes: &[u8],
) -> Result<AssetRecord, ContentError> {
    let record = AssetRecord {
        id,
        name: name.into(),
        format: AssetFormat::Spvox,
        path: relative_path.into(),
        content_hash: *blake3::hash(bytes).as_bytes(),
    };
    validate_record(&record)?;
    if bytes.len() as u64 > MAX_ASSET_BYTES {
        return Err(ContentError::AssetTooLarge(id));
    }
    validate_spvox(id, bytes)?;
    Ok(record)
}

fn validate_record(record: &AssetRecord) -> Result<(), ContentError> {
    if record.id.0 == 0
        || record.name.trim().is_empty()
        || record.name.len() > 256
        || record.path.len() > 1024
        || record.path.contains('\\')
        || record.path.contains(':')
        || record
            .path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || validate_relative_path(Path::new(&record.path)).is_err()
    {
        return Err(ContentError::InvalidMetadata(record.id));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), ()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(());
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(());
        }
    }
    Ok(())
}

fn validate_spvox(id: AssetId, bytes: &[u8]) -> Result<(), ContentError> {
    if bytes.len() < 12
        || &bytes[..4] != b"SPVX"
        || u16::from_le_bytes([bytes[4], bytes[5]]) != SPVOX_MAJOR_VERSION
    {
        return Err(ContentError::UnsupportedAssetFormat(id));
    }
    if u32::from_le_bytes(bytes[8..12].try_into().expect("4-byte slice")) != 0 {
        return Err(ContentError::InvalidSpvox(id));
    }
    let mut offset = 12_usize;
    let mut chunk_count = 0_usize;
    let mut decoded_total = 0_usize;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"spall.asset.v1");
    let mut saw_meta = false;
    let mut saw_materials = false;
    let mut saw_voxels = false;
    let mut saw_palette = false;
    let mut saw_parts = false;
    let mut saw_structure = false;
    let mut saw_animation = false;
    let mut saw_layers = false;
    let mut meta_payload = None;
    let mut saw_hash = false;
    while offset < bytes.len() {
        if chunk_count >= MAX_SPVOX_CHUNKS
            || offset.checked_add(16).is_none_or(|end| end > bytes.len())
        {
            return Err(ContentError::InvalidSpvox(id));
        }
        let header = &bytes[offset..offset + 16];
        let chunk_id = &header[..4];
        let codec = header[4];
        if header[5..8] != [0, 0, 0] {
            return Err(ContentError::InvalidSpvox(id));
        }
        let stored_len =
            u32::from_le_bytes(header[8..12].try_into().expect("4-byte slice")) as usize;
        let raw_len = u32::from_le_bytes(header[12..16].try_into().expect("4-byte slice")) as usize;
        let data_start = offset + 16;
        let data_end = data_start
            .checked_add(stored_len)
            .ok_or(ContentError::InvalidSpvox(id))?;
        if raw_len > MAX_SPVOX_CHUNK_BYTES || data_end > bytes.len() {
            return Err(ContentError::InvalidSpvox(id));
        }
        decoded_total = decoded_total
            .checked_add(raw_len)
            .ok_or(ContentError::InvalidSpvox(id))?;
        if decoded_total > MAX_SPVOX_DECODED_BYTES {
            return Err(ContentError::InvalidSpvox(id));
        }
        let stored = &bytes[data_start..data_end];
        let raw = match codec {
            0 if stored_len == raw_len => stored.to_vec(),
            0 => return Err(ContentError::InvalidSpvox(id)),
            1 => zstd::bulk::decompress(stored, raw_len)
                .map_err(|_| ContentError::InvalidSpvox(id))?,
            _ => return Err(ContentError::InvalidSpvox(id)),
        };
        if raw.len() != raw_len {
            return Err(ContentError::InvalidSpvox(id));
        }
        if chunk_id == b"HASH" {
            if saw_hash || raw.len() != 32 || data_end != bytes.len() || codec != 0 {
                return Err(ContentError::InvalidSpvox(id));
            }
            saw_hash = true;
            if hasher.finalize().as_bytes() != raw.as_slice() {
                return Err(ContentError::InvalidSpvox(id));
            }
        } else {
            if saw_hash {
                return Err(ContentError::InvalidSpvox(id));
            }
            match chunk_id {
                b"META" if !saw_meta && chunk_count == 0 => {
                    saw_meta = true;
                    meta_payload = Some(raw.clone());
                }
                b"MTRL" if !saw_materials => saw_materials = true,
                b"VOXL" if !saw_voxels => saw_voxels = true,
                b"PALT" if !saw_palette => saw_palette = true,
                b"PART" if !saw_parts => saw_parts = true,
                b"STRC" if !saw_structure => saw_structure = true,
                b"ANIM" if !saw_animation => saw_animation = true,
                b"LAYR" if !saw_layers && u16::from_le_bytes([bytes[6], bytes[7]]) >= 1 => {
                    saw_layers = true;
                }
                b"META" | b"MTRL" | b"VOXL" | b"HASH" => {
                    return Err(ContentError::InvalidSpvox(id));
                }
                _ => return Err(ContentError::InvalidSpvox(id)),
            }
            hasher.update(chunk_id);
            hasher.update(&(raw_len as u32).to_le_bytes());
            hasher.update(&raw);
        }
        offset = data_end;
        chunk_count += 1;
    }
    if !saw_meta || !saw_materials || !saw_voxels || !saw_hash {
        return Err(ContentError::InvalidSpvox(id));
    }
    let meta = meta_payload.ok_or(ContentError::InvalidSpvox(id))?;
    let mut input = ByteReader::new(&meta);
    input.take(16)?;
    input.u8()?;
    input.u8()?;
    for _ in 0..3 {
        input.i32()?;
    }
    input.string_u16()?;
    let required = input.u64()?;
    let optional = input.u64()?;
    if required & !1 != 0 || optional & !1 != 0 || (optional & 1 != 0) != saw_layers {
        return Err(ContentError::UnsupportedSpvoxFeature(id));
    }
    Ok(())
}

fn decode_static_voxels(
    id: AssetId,
    bytes: &[u8],
    material_mapping: &BTreeMap<String, MaterialId>,
) -> Result<LoadedVoxelAsset, ContentError> {
    decode_static_voxels_with(id, bytes, material_mapping, false)
}

/// Decodes an editor-authored static SPVX asset for scene playback. Display
/// tints are accepted and dropped: runtime cells carry only a material, so the
/// appearance at play time comes from the mapped material.
pub fn decode_editor_voxels(
    id: AssetId,
    bytes: &[u8],
    material_mapping: &BTreeMap<String, MaterialId>,
) -> Result<LoadedVoxelAsset, ContentError> {
    decode_static_voxels_with(id, bytes, material_mapping, true)
}

fn decode_static_voxels_with(
    id: AssetId,
    bytes: &[u8],
    material_mapping: &BTreeMap<String, MaterialId>,
    allow_display_tints: bool,
) -> Result<LoadedVoxelAsset, ContentError> {
    validate_spvox(id, bytes)?;
    let mut chunks = BTreeMap::<[u8; 4], Vec<u8>>::new();
    let mut cursor = 12_usize;
    while cursor < bytes.len() {
        let header = &bytes[cursor..cursor + 16];
        let chunk_id: [u8; 4] = header[..4].try_into().expect("4-byte chunk id");
        let codec = header[4];
        let stored_len =
            u32::from_le_bytes(header[8..12].try_into().expect("4-byte length")) as usize;
        let raw_len =
            u32::from_le_bytes(header[12..16].try_into().expect("4-byte length")) as usize;
        let start = cursor + 16;
        let end = start + stored_len;
        if chunk_id != *b"HASH" {
            let stored = &bytes[start..end];
            let raw = match codec {
                0 => stored.to_vec(),
                1 => zstd::bulk::decompress(stored, raw_len)
                    .map_err(|_| ContentError::InvalidSpvox(id))?,
                _ => return Err(ContentError::InvalidSpvox(id)),
            };
            if chunks.insert(chunk_id, raw).is_some() {
                return Err(ContentError::InvalidSpvox(id));
            }
        }
        cursor = end;
    }
    if chunks.contains_key(b"PART") || chunks.contains_key(b"STRC") || chunks.contains_key(b"ANIM")
    {
        return Err(ContentError::UnsupportedSpvoxFeature(id));
    }

    let meta = chunks.get(b"META").ok_or(ContentError::InvalidSpvox(id))?;
    let mut input = ByteReader::new(meta);
    let portable_id = input.take(16)?;
    if portable_id.iter().all(|byte| *byte == 0) {
        return Err(ContentError::InvalidSpvox(id));
    }
    let cell_size =
        CellSizeCode::from_u8(input.u8()?).ok_or(ContentError::UnsupportedAssetFormat(id))?;
    if input.u8()? != 0 {
        return Err(ContentError::UnsupportedSpvoxFeature(id));
    }
    let pivot_subcells = [input.i32()?, input.i32()?, input.i32()?];
    let name = input.string_u16()?;
    if name.is_empty()
        || input.u64()? != 0
        || input.u64()? != u64::from(chunks.contains_key(b"LAYR"))
    {
        return Err(ContentError::UnsupportedSpvoxFeature(id));
    }
    let tag_count = input.u16()? as usize;
    for _ in 0..tag_count {
        input.skip_string_u8()?;
        input.skip_string_u16()?;
    }
    input.finish()?;

    let material_payload = chunks.get(b"MTRL").ok_or(ContentError::InvalidSpvox(id))?;
    let mut input = ByteReader::new(material_payload);
    let material_count = input.u16()? as usize;
    if material_count > 4096 {
        return Err(ContentError::InvalidSpvox(id));
    }
    let mut materials = Vec::with_capacity(material_count);
    let mut previous_key: Option<String> = None;
    for _ in 0..material_count {
        let key = input.string_u8()?;
        if key.is_empty() || previous_key.as_ref().is_some_and(|prior| prior >= &key) {
            return Err(ContentError::InvalidSpvox(id));
        }
        materials.push(key.clone());
        previous_key = Some(key);
    }
    input.finish()?;

    let palette: Vec<[u8; 3]> = chunks
        .get(b"PALT")
        .map(|payload| {
            let mut reader = ByteReader::new(payload);
            let count = reader.u16()? as usize;
            if count > 4096 {
                return Err(ContentError::InvalidSpvox(id));
            }
            let colors =
                reader.take(count.checked_mul(3).ok_or(ContentError::InvalidSpvox(id))?)?;
            reader.finish()?;
            Ok::<_, ContentError>(colors.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect())
        })
        .transpose()?
        .unwrap_or_default();

    let voxel_payload = chunks.get(b"VOXL").ok_or(ContentError::InvalidSpvox(id))?;
    let mut input = ByteReader::new(voxel_payload);
    if input.u8()? != 0 {
        return Err(ContentError::UnsupportedSpvoxFeature(id));
    }
    let run_count = input.u32()? as usize;
    if run_count > MAX_RUNTIME_VOXELS {
        return Err(ContentError::TooManyRuntimeVoxels(id));
    }
    let mut cells = Vec::new();
    let mut previous: Option<(i32, i32, i32, i64, u16, u16)> = None;
    for _ in 0..run_count {
        let part_id = input.u32()?;
        let x = input.i32()?;
        let y = input.i32()?;
        let z = input.i32()?;
        let length = input.u32()?;
        let material_slot = input.u16()?;
        let tint_slot = input.u16()?;
        if part_id != 1 || length == 0 || material_slot == 0 {
            return Err(ContentError::UnsupportedSpvoxFeature(id));
        }
        if tint_slot != 0 && !allow_display_tints {
            return Err(ContentError::UnsupportedSpvoxFeature(id));
        }
        let key = materials
            .get(usize::from(material_slot - 1))
            .ok_or(ContentError::InvalidSpvox(id))?;
        let material = material_mapping
            .get(key)
            .copied()
            .filter(|material| material.0 != 0)
            .ok_or_else(|| ContentError::UnmappedMaterial(id, key.clone()))?;
        // Tint slots are one-based palette references; zero means "the
        // material's own appearance".
        let tint = match tint_slot {
            0 => None,
            slot => Some(
                *palette
                    .get(usize::from(slot) - 1)
                    .ok_or(ContentError::InvalidSpvox(id))?,
            ),
        };
        let endpoint = i64::from(x) + i64::from(length) - 1;
        if endpoint > i64::from(i32::MAX) {
            return Err(ContentError::InvalidSpvox(id));
        }
        if cells
            .len()
            .checked_add(length as usize)
            .is_none_or(|count| count > MAX_RUNTIME_VOXELS)
        {
            return Err(ContentError::TooManyRuntimeVoxels(id));
        }
        if let Some((pz, py, px, prior_end, prior_material, prior_tint)) = previous {
            if (z, y, x) <= (pz, py, px) || (z == pz && y == py && i64::from(x) < prior_end) {
                return Err(ContentError::InvalidSpvox(id));
            }
            if z == pz
                && y == py
                && i64::from(x) == prior_end
                && material_slot == prior_material
                && tint_slot == prior_tint
            {
                return Err(ContentError::InvalidSpvox(id));
            }
        }
        for dx in 0..length {
            cells.push(LoadedVoxelCell {
                cell: GlobalCell::new(i64::from(x) + i64::from(dx), i64::from(y), i64::from(z)),
                material,
                tint,
            });
        }
        previous = Some((z, y, x, endpoint + 1, material_slot, tint_slot));
    }
    input.finish()?;
    if cells.is_empty() {
        return Err(ContentError::InvalidSpvox(id));
    }
    Ok(LoadedVoxelAsset {
        id,
        name,
        cell_size,
        pivot_subcells,
        cells,
    })
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u32).to_le_bytes());
    hasher.update(value.as_bytes());
}

struct ByteReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8], ContentError> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or(ContentError::InvalidSpvox(AssetId(0)))?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(ContentError::InvalidSpvox(AssetId(0)))?;
        self.cursor = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, ContentError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ContentError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    fn u32(&mut self) -> Result<u32, ContentError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn i32(&mut self) -> Result<i32, ContentError> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64, ContentError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn string_u8(&mut self) -> Result<String, ContentError> {
        let len = self.u8()? as usize;
        Ok(String::from_utf8(self.take(len)?.to_vec())?)
    }
    fn string_u16(&mut self) -> Result<String, ContentError> {
        let len = self.u16()? as usize;
        Ok(String::from_utf8(self.take(len)?.to_vec())?)
    }
    fn skip_string_u8(&mut self) -> Result<(), ContentError> {
        let len = self.u8()? as usize;
        self.take(len)?;
        Ok(())
    }
    fn skip_string_u16(&mut self) -> Result<(), ContentError> {
        let len = self.u16()? as usize;
        self.take(len)?;
        Ok(())
    }
    fn finish(&self) -> Result<(), ContentError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(ContentError::InvalidSpvox(AssetId(0)))
        }
    }
}

enum BoundedReadError {
    TooLarge,
    Io(std::io::Error),
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, BoundedReadError> {
    let file = File::open(path).map_err(BoundedReadError::Io)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod layer_tests {
    use super::*;

    fn chunk(bytes: &mut Vec<u8>, hasher: &mut blake3::Hasher, id: &[u8; 4], payload: &[u8]) {
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        hasher.update(id);
        hasher.update(&(payload.len() as u32).to_le_bytes());
        hasher.update(payload);
    }

    fn layered_asset() -> Vec<u8> {
        let mut bytes = b"SPVX".to_vec();
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"spall.asset.v1");
        let mut meta = vec![1; 16];
        meta.extend_from_slice(&[0; 14]); // cell size, axes, pivot
        meta.extend_from_slice(&1_u16.to_le_bytes());
        meta.push(b'x');
        meta.extend_from_slice(&0_u64.to_le_bytes());
        meta.extend_from_slice(&1_u64.to_le_bytes());
        meta.extend_from_slice(&0_u16.to_le_bytes());
        chunk(&mut bytes, &mut hasher, b"META", &meta);
        let mut materials = 2_u16.to_le_bytes().to_vec();
        materials.push(13);
        materials.extend_from_slice(b"stone.granite");
        materials.push(8);
        materials.extend_from_slice(b"wood.oak");
        chunk(&mut bytes, &mut hasher, b"MTRL", &materials);
        let mut voxels = vec![0];
        voxels.extend_from_slice(&1_u32.to_le_bytes());
        voxels.extend_from_slice(&1_u32.to_le_bytes()); // root part
        voxels.extend_from_slice(&[0; 12]); // xyz
        voxels.extend_from_slice(&1_u32.to_le_bytes()); // length
        voxels.extend_from_slice(&2_u16.to_le_bytes()); // material slot
        voxels.extend_from_slice(&0_u16.to_le_bytes()); // tint slot
        chunk(&mut bytes, &mut hasher, b"VOXL", &voxels);
        let mut layers = 2_u16.to_le_bytes().to_vec();
        layers.extend_from_slice(&1_u32.to_le_bytes()); // layer id
        layers.push(4);
        layers.extend_from_slice(b"Base");
        layers.push(1); // visible
        layers.extend_from_slice(&1_u32.to_le_bytes());
        layers.extend_from_slice(&[0; 12]);
        layers.extend_from_slice(&2_u16.to_le_bytes());
        layers.extend_from_slice(&0_u16.to_le_bytes());
        layers.extend_from_slice(&2_u32.to_le_bytes()); // hidden layer id
        layers.push(6);
        layers.extend_from_slice(b"Hidden");
        layers.push(0);
        layers.extend_from_slice(&1_u32.to_le_bytes());
        layers.extend_from_slice(&1_i32.to_le_bytes()); // x
        layers.extend_from_slice(&0_i32.to_le_bytes()); // y
        layers.extend_from_slice(&0_i32.to_le_bytes()); // z
        layers.extend_from_slice(&1_u16.to_le_bytes()); // hidden-only material
        layers.extend_from_slice(&0_u16.to_le_bytes());
        chunk(&mut bytes, &mut hasher, b"LAYR", &layers);
        let digest = hasher.finalize();
        bytes.extend_from_slice(b"HASH");
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(digest.as_bytes());
        bytes
    }

    #[test]
    fn static_import_uses_voxl_and_accepts_layer_metadata() {
        let bytes = layered_asset();
        let mapping = BTreeMap::from([("wood.oak".to_owned(), MaterialId(2))]);
        let loaded = decode_static_voxels(AssetId(7), &bytes, &mapping).unwrap();
        assert_eq!(loaded.cells.len(), 1);
        assert_eq!(loaded.cells[0].material, MaterialId(2));
        let mut missing_layer = bytes.clone();
        missing_layer[6..8].copy_from_slice(&0_u16.to_le_bytes());
        assert!(validate_spvox(AssetId(7), &missing_layer).is_err());
    }
}
