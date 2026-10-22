//! Bounded, CPU-only reader and transform sampler for portable SPVX assets.
//!
//! This crate preserves symbolic material keys, sRGB tints, authored part
//! transforms, and exact sparse voxel runs. It does not map game materials or
//! create runtime entities, physics bodies, or GPU resources.

use std::collections::{BTreeMap, BTreeSet};

use glam::{Quat, Vec3};
use thiserror::Error;

const MAGIC: &[u8; 4] = b"SPVX";
const MAX_FILE: usize = 256 * 1024 * 1024;
const MAX_CHUNKS: usize = 64;
const MAX_CHUNK: usize = 128 * 1024 * 1024;
const MAX_TOTAL: usize = 512 * 1024 * 1024;
const MAX_TABLE: usize = 4096;
const MAX_PARTS: usize = 1024;
const MAX_RUNS: usize = 16_000_000;
const MAX_CELLS: u64 = 64_000_000;
const MAX_CLIPS: usize = 256;
const MAX_TRACKS: usize = 1024;
const MAX_KEYS: usize = 65_535;
const FEATURE_STRUCTURAL: u64 = 1;
const FEATURE_LAYERS: u64 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AssetError {
    #[error("invalid SPVX asset: {0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, AssetError>;

#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub uuid: [u8; 16],
    pub name: String,
    pub cell_size_code: u8,
    pub pivot_subcells: [i32; 3],
    pub tags: BTreeMap<String, String>,
    pub material_keys: Vec<String>,
    pub tints_srgb: Vec<[u8; 3]>,
    /// Implicit root part 1 is materialized when `PART` is absent.
    pub parts: Vec<Part>,
    pub runs: Vec<VoxelRun>,
    pub clips: Vec<Clip>,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub id: u32,
    pub parent_id: u32,
    pub name: String,
    pub rest: Transform,
    pub structural_profile_slot: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Translation in cell units (source integer subcells divided by 256).
    pub translation: Vec3,
    /// Normalized `(x, y, z, w)` source quaternion.
    pub rotation: Quat,
    /// Scale components divided by 1024; any authored positive scale is kept.
    pub scale: Vec3,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoxelRun {
    pub part_id: u32,
    pub start: [i32; 3],
    pub length_x: u32,
    /// One-based index into `Asset::material_keys`.
    pub material_slot: u16,
    /// Zero means no tint; otherwise one-based into `Asset::tints_srgb`.
    pub tint_slot: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: String,
    pub duration_ticks: u32,
    pub loop_mode: LoopMode,
    pub tracks: Vec<Track>,
    pub events: Vec<AnimationEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    Once,
    Loop,
    PingPong,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub part_id: u32,
    pub keys: Vec<Keyframe>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Keyframe {
    pub tick: u32,
    pub interpolation: Interpolation,
    pub translation: Option<[i32; 3]>,
    pub rotation: Option<[i16; 4]>,
    pub scale: Option<[u16; 3]>,
    pub visibility: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    Step,
    Linear,
    Spherical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimationEvent {
    pub tick: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartPose {
    pub transform: Transform,
    pub visible: bool,
}

/// Decode and validate one complete v1.0/v1.1 SPVX asset.
pub fn decode(bytes: &[u8]) -> Result<Asset> {
    if bytes.len() > MAX_FILE {
        return invalid("file exceeds 256 MiB");
    }
    let mut input = Reader::new(bytes);
    if input.array::<4>()? != *MAGIC {
        return invalid("magic is missing");
    }
    let major = input.u16()?;
    let minor = input.u16()?;
    if major != 1 {
        return invalid(format!("major version {major} is unsupported"));
    }
    if minor > 1 { /* unknown minor is allowed only with understood feature bits */ }
    if input.u32()? != 0 {
        return invalid("header flags must be zero");
    }

    let mut chunks = Vec::<([u8; 4], Vec<u8>)>::new();
    let mut total = 0usize;
    while !input.is_empty() {
        if chunks.len() == MAX_CHUNKS {
            return invalid("more than 64 chunks");
        }
        let id = input.array::<4>()?;
        let codec = input.u8()?;
        if (id == *b"META" || id == *b"HASH") && codec != 0 {
            return invalid("META and HASH chunks must use the raw codec");
        }
        if input.array::<3>()? != [0; 3] {
            return invalid("chunk reserved bytes are nonzero");
        }
        let stored_len = input.u32()? as usize;
        let raw_len = input.u32()? as usize;
        if raw_len > MAX_CHUNK {
            return invalid("decoded chunk exceeds 128 MiB");
        }
        total = total
            .checked_add(raw_len)
            .ok_or_else(|| err("decoded length overflow"))?;
        if total > MAX_TOTAL {
            return invalid("decoded chunks exceed 512 MiB");
        }
        let stored = input.take(stored_len)?;
        let payload = match codec {
            0 if stored_len == raw_len => stored.to_vec(),
            0 => return invalid("raw chunk stored/raw lengths differ"),
            1 => zstd::bulk::decompress(stored, raw_len)
                .map_err(|e| err(format!("zstd decode failed: {e}")))?,
            _ => return invalid(format!("unsupported codec {codec}")),
        };
        if payload.len() != raw_len {
            return invalid("decoded chunk length differs from header");
        }
        chunks.push((id, payload));
    }
    if chunks.len() < 4 || chunks[0].0 != *b"META" {
        return invalid("META must be first and required chunks must be present");
    }
    if chunks.last().map(|c| c.0) != Some(*b"HASH") || chunks.last().unwrap().1.len() != 32 {
        return invalid("one final 32-byte HASH is required");
    }
    validate_chunk_order(&chunks)?;
    let hash = logical_hash(&chunks[..chunks.len() - 1]);
    if hash.as_bytes() != chunks.last().unwrap().1.as_slice() {
        return invalid("HASH mismatch");
    }

    let mut seen = BTreeSet::new();
    let (mut meta, mut materials, mut tints, mut parts, mut runs, mut clips) =
        (None, None, None, None, None, None);
    for (id, payload) in &chunks[..chunks.len() - 1] {
        if !seen.insert(*id) {
            return invalid(format!("duplicate chunk {}", fourcc(*id)));
        }
        match id {
            b"META" => meta = Some(decode_meta(payload)?),
            b"MTRL" => materials = Some(decode_materials(payload)?),
            b"PALT" => tints = Some(decode_palette(payload)?),
            b"PART" => parts = Some(decode_parts(payload)?),
            b"VOXL" => runs = Some(decode_runs(payload)?),
            b"ANIM" => clips = Some(decode_clips(payload)?),
            b"STRC" => return invalid("STRC is unsupported by this animated consumer"),
            b"LAYR" => return invalid("LAYR is unsupported by this animated consumer"),
            other => return invalid(format!("unknown chunk {}", fourcc(*other))),
        }
    }
    let meta = meta.ok_or_else(|| err("META is missing"))?;
    let materials = materials.ok_or_else(|| err("MTRL is missing"))?;
    let tints = tints.unwrap_or_default();
    let parts = parts.unwrap_or_else(|| {
        vec![Part {
            id: 1,
            parent_id: 0,
            name: "root".into(),
            rest: Transform::IDENTITY,
            structural_profile_slot: 0,
        }]
    });
    let runs = runs.ok_or_else(|| err("VOXL is missing"))?;
    let clips = clips.unwrap_or_default();
    if meta.required_features & !FEATURE_STRUCTURAL != 0 {
        return invalid("unknown required feature bit");
    }
    if meta.optional_features & !FEATURE_LAYERS != 0 {
        return invalid("unknown optional feature bit");
    }
    if meta.optional_features & FEATURE_LAYERS != 0 {
        return invalid("LAYR feature is unsupported by this animated consumer");
    }
    if minor == 0 && meta.required_features & FEATURE_STRUCTURAL != 0 {
        return invalid("structural profiles require SPVX minor version 1");
    }
    let has_profiles = parts.iter().any(|p| p.structural_profile_slot != 0);
    if has_profiles != (meta.required_features & FEATURE_STRUCTURAL != 0) {
        return invalid("STRUCTURAL_PROFILES feature bit disagrees with PART data");
    }
    if has_profiles {
        return invalid("PART structural profiles require unsupported STRC");
    }
    validate_parts(&parts)?;
    validate_runs(&runs, &parts, materials.len(), tints.len())?;
    validate_clips(&clips, &parts)?;
    Ok(Asset {
        uuid: meta.uuid,
        name: meta.name,
        cell_size_code: meta.cell_size,
        pivot_subcells: meta.pivot,
        tags: meta.tags,
        material_keys: materials,
        tints_srgb: tints,
        parts,
        runs,
        clips,
        content_hash: hash.into(),
    })
}

/// Sample a named clip at a time expressed in 60 Hz ticks. Each part begins at
/// its rest transform and visible state; tracks override only authored fields.
pub fn sample_clip(
    asset: &Asset,
    clip_name: &str,
    time_ticks: f64,
) -> Result<BTreeMap<u32, PartPose>> {
    if !time_ticks.is_finite() {
        return invalid("animation time must be finite");
    }
    validate_parts(&asset.parts)?;
    validate_clips(&asset.clips, &asset.parts)?;
    let clip = asset
        .clips
        .iter()
        .find(|c| c.name == clip_name)
        .ok_or_else(|| err(format!("clip {clip_name:?} not found")))?;
    let tick = playback_time(clip.duration_ticks, clip.loop_mode, time_ticks.max(0.0));
    let mut poses: BTreeMap<_, _> = asset
        .parts
        .iter()
        .map(|p| {
            (
                p.id,
                PartPose {
                    transform: p.rest,
                    visible: true,
                },
            )
        })
        .collect();
    for track in &clip.tracks {
        let part = asset
            .parts
            .iter()
            .find(|p| p.id == track.part_id)
            .ok_or_else(|| err("animation track references unknown part"))?;
        let pose = poses
            .get_mut(&track.part_id)
            .ok_or_else(|| err("animation pose references unknown part"))?;
        pose.transform = sample_transform(part.rest, &track.keys, tick);
        pose.visible = sample_visibility(&track.keys, tick);
    }
    Ok(poses)
}

fn playback_time(duration: u32, mode: LoopMode, time: f64) -> f64 {
    let d = duration as f64;
    if d == 0.0 {
        return 0.0;
    }
    match mode {
        LoopMode::Once => time.min(d),
        LoopMode::Loop => time.rem_euclid(d),
        LoopMode::PingPong => {
            let p = time.rem_euclid(2.0 * d);
            if p <= d { p } else { 2.0 * d - p }
        }
    }
}

fn sample_transform(rest: Transform, keys: &[Keyframe], time: f64) -> Transform {
    let mut out = rest;
    if let Some((x, y, t, interpolation)) = sample_field(keys, time, |key| key.translation) {
        out.translation = subcell_vec(x);
        // Both Linear and Spherical key modes linearly interpolate
        // translation/scale; Spherical additionally applies to rotation.
        if let Some(y) = y
            && interpolation != Interpolation::Step
        {
            out.translation = out.translation.lerp(subcell_vec(y), t);
        }
    }
    if let Some((x, y, t, interpolation)) = sample_field(keys, time, |key| key.rotation) {
        out.rotation = quat_i16(x);
        if let Some(y) = y
            && interpolation == Interpolation::Spherical
        {
            out.rotation = shortest_slerp(out.rotation, quat_i16(y), t);
        }
    }
    if let Some((x, y, t, interpolation)) = sample_field(keys, time, |key| key.scale) {
        out.scale = scale_vec(x);
        if let Some(y) = y
            && interpolation != Interpolation::Step
        {
            out.scale = out.scale.lerp(scale_vec(y), t);
        }
    }
    out
}

fn sample_visibility(keys: &[Keyframe], time: f64) -> bool {
    keys.iter()
        .rev()
        .filter(|key| key.tick as f64 <= time)
        .filter_map(|key| key.visibility)
        .next()
        .unwrap_or(true)
}

fn sample_field<T: Copy>(
    keys: &[Keyframe],
    time: f64,
    field: impl Fn(&Keyframe) -> Option<T>,
) -> Option<(T, Option<T>, f32, Interpolation)> {
    let mut before: Option<(&Keyframe, T)> = None;
    for key in keys {
        if let Some(value) = field(key) {
            if key.tick as f64 <= time {
                before = Some((key, value));
            } else if let Some((previous, previous_value)) = before {
                let t = ((time - previous.tick as f64) / (key.tick - previous.tick) as f64) as f32;
                return Some((previous_value, Some(value), t, previous.interpolation));
            } else {
                return None;
            }
        }
    }
    before.map(|(key, value)| (value, None, 0.0, key.interpolation))
}

fn subcell_vec(v: [i32; 3]) -> Vec3 {
    Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32) / 256.0
}
fn scale_vec(v: [u16; 3]) -> Vec3 {
    Vec3::new(v[0] as f32, v[1] as f32, v[2] as f32) / 1024.0
}
fn quat_i16(v: [i16; 4]) -> Quat {
    Quat::from_xyzw(v[0] as f32, v[1] as f32, v[2] as f32, v[3] as f32).normalize()
}
fn shortest_slerp(a: Quat, mut b: Quat, t: f32) -> Quat {
    if a.dot(b) < 0.0 {
        b = -b;
    }
    a.slerp(b, t).normalize()
}

struct Meta {
    uuid: [u8; 16],
    cell_size: u8,
    pivot: [i32; 3],
    name: String,
    required_features: u64,
    optional_features: u64,
    tags: BTreeMap<String, String>,
}
fn decode_meta(bytes: &[u8]) -> Result<Meta> {
    let mut r = Reader::new(bytes);
    let uuid = r.array::<16>()?;
    if uuid == [0; 16] {
        return invalid("META UUID is zero");
    }
    let cell_size = r.u8()?;
    if cell_size > 1 {
        return invalid("unsupported cell-size code");
    }
    if r.u8()? != 0 {
        return invalid("axis convention is unsupported");
    }
    let pivot = [r.i32()?, r.i32()?, r.i32()?];
    let name = r.string_u16()?;
    validate_name(&name)?;
    let required_features = r.u64()?;
    let optional_features = r.u64()?;
    let n = r.u16()? as usize;
    preflight(&r, n, 4, "META tags")?;
    let mut tags = BTreeMap::new();
    let mut last: Option<String> = None;
    for _ in 0..n {
        let k = r.string_u8()?;
        let v = r.string_u16()?;
        if !valid_tag_key(&k) || last.as_ref().is_some_and(|p| p >= &k) {
            return invalid("META tags invalid or noncanonical");
        }
        last = Some(k.clone());
        tags.insert(k, v);
    }
    r.finish()?;
    Ok(Meta {
        uuid,
        cell_size,
        pivot,
        name,
        required_features,
        optional_features,
        tags,
    })
}
fn decode_materials(bytes: &[u8]) -> Result<Vec<String>> {
    let mut r = Reader::new(bytes);
    let n = r.u16()? as usize;
    if n > MAX_TABLE {
        return invalid("too many materials");
    }
    preflight(&r, n, 2, "MTRL entries")?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let s = r.string_u8()?;
        if s.is_empty() || s.len() > 255 || v.last().is_some_and(|p: &String| p >= &s) {
            return invalid("material keys must be nonempty, unique and bytewise sorted");
        }
        v.push(s);
    }
    r.finish()?;
    Ok(v)
}
fn decode_palette(bytes: &[u8]) -> Result<Vec<[u8; 3]>> {
    let mut r = Reader::new(bytes);
    let n = r.u16()? as usize;
    if n > MAX_TABLE {
        return invalid("too many palette entries");
    }
    preflight(&r, n, 3, "PALT entries")?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(r.array::<3>()?);
    }
    r.finish()?;
    Ok(v)
}
fn decode_parts(bytes: &[u8]) -> Result<Vec<Part>> {
    let mut r = Reader::new(bytes);
    let n = r.u16()? as usize;
    if n == 0 || n > MAX_PARTS {
        return invalid("PART count must be 1..=1024");
    }
    preflight(&r, n, 38, "PART records")?;
    let mut parts = Vec::with_capacity(n);
    let mut ids = BTreeSet::new();
    let mut siblings = BTreeSet::new();
    for i in 0..n {
        let id = r.u32()?;
        let parent_id = r.u32()?;
        let name = r.string_u8()?;
        validate_name(&name)?;
        let translation = [r.i32()?, r.i32()?, r.i32()?];
        let q = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
        let scale = [r.u16()?, r.u16()?, r.u16()?];
        let structural_profile_slot = r.u16()?;
        if id == 0 || !ids.insert(id) {
            return invalid("PART IDs must be nonzero and unique");
        }
        if parent_id == 0 {
            if id != 1 || i != 0 {
                return invalid("only first root part 1 may have parent 0");
            }
        } else if !ids.contains(&parent_id) || parent_id == id {
            return invalid("PART parent must precede child");
        }
        if scale.contains(&0) {
            return invalid("PART scale component is zero");
        }
        let qf = q.map(|x| x as f32 / 32767.0);
        let norm = qf.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm == 0.0 || !norm.is_finite() || (norm - 1.0).abs() > 0.002 {
            return invalid("PART quaternion is not normalized");
        }
        if !siblings.insert((parent_id, name.clone())) {
            return invalid("PART sibling names must be unique");
        }
        parts.push(Part {
            id,
            parent_id,
            name,
            rest: Transform {
                translation: subcell_vec(translation),
                rotation: quat_i16(q),
                scale: scale_vec(scale),
            },
            structural_profile_slot,
        });
    }
    r.finish()?;
    validate_parts(&parts)?;
    Ok(parts)
}
fn decode_runs(bytes: &[u8]) -> Result<Vec<VoxelRun>> {
    let mut r = Reader::new(bytes);
    if r.u8()? != 0 {
        return invalid("unsupported VOXL encoding");
    }
    let n = r.u32()? as usize;
    if n > MAX_RUNS {
        return invalid("too many voxel runs");
    }
    preflight(&r, n, 24, "VOXL runs")?;
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        runs.push(VoxelRun {
            part_id: r.u32()?,
            start: [r.i32()?, r.i32()?, r.i32()?],
            length_x: r.u32()?,
            material_slot: r.u16()?,
            tint_slot: r.u16()?,
        });
    }
    r.finish()?;
    Ok(runs)
}
fn validate_runs(runs: &[VoxelRun], parts: &[Part], materials: usize, tints: usize) -> Result<()> {
    let ids: BTreeSet<_> = parts.iter().map(|p| p.id).collect();
    let mut total = 0u64;
    let mut last: Option<(u32, i32, i32, i32, i64, u16, u16)> = None;
    for r in runs {
        if r.length_x == 0 {
            return invalid("zero-length voxel run");
        }
        if !ids.contains(&r.part_id) {
            return invalid("voxel run references unknown part");
        }
        if r.material_slot == 0 || r.material_slot as usize > materials {
            return invalid("voxel run references unknown material slot");
        }
        if r.tint_slot as usize > tints {
            return invalid("voxel run references unknown tint slot");
        }
        let end = (r.start[0] as i64) + (r.length_x as i64) - 1;
        if end > i32::MAX as i64 {
            return invalid("voxel run endpoint exceeds i32");
        }
        let key = (
            r.part_id,
            r.start[2],
            r.start[1],
            r.start[0],
            end,
            r.material_slot,
            r.tint_slot,
        );
        if let Some(prev) = last {
            if (r.part_id, r.start[2], r.start[1], r.start[0]) <= (prev.0, prev.1, prev.2, prev.3) {
                return invalid("voxel runs are not strictly sorted");
            }
            if r.part_id == prev.0
                && r.start[2] == prev.1
                && r.start[1] == prev.2
                && r.material_slot == prev.5
                && r.tint_slot == prev.6
                && r.start[0] as i64 <= prev.4 + 1
            {
                return invalid("compatible voxel runs overlap or touch");
            }
            if r.part_id == prev.0
                && r.start[2] == prev.1
                && r.start[1] == prev.2
                && r.start[0] as i64 <= prev.4
            {
                return invalid("voxel runs overlap");
            }
        }
        last = Some(key);
        total = total
            .checked_add(r.length_x as u64)
            .ok_or_else(|| err("cell count overflow"))?;
        if total > MAX_CELLS {
            return invalid("expanded voxel count exceeds 64,000,000");
        }
    }
    Ok(())
}

fn decode_clips(bytes: &[u8]) -> Result<Vec<Clip>> {
    let mut r = Reader::new(bytes);
    let n = r.u16()? as usize;
    if n > MAX_CLIPS {
        return invalid("too many clips");
    }
    preflight(&r, n, 11, "ANIM clips")?;
    let mut clips = Vec::with_capacity(n);
    let mut names = BTreeSet::new();
    for _ in 0..n {
        let name = r.string_u8()?;
        validate_name(&name)?;
        if !names.insert(name.clone()) {
            return invalid("clip names are duplicated");
        }
        let duration_ticks = r.u32()?;
        let loop_mode = match r.u8()? {
            0 => LoopMode::Once,
            1 => LoopMode::Loop,
            2 => LoopMode::PingPong,
            x => return invalid(format!("unknown clip loop mode {x}")),
        };
        let tn = r.u16()? as usize;
        if tn > MAX_TRACKS {
            return invalid("too many tracks in clip");
        }
        preflight(&r, tn, 6, "ANIM tracks")?;
        let mut tracks = Vec::with_capacity(tn);
        for _ in 0..tn {
            let part_id = r.u32()?;
            let kn = r.u16()? as usize;
            if kn > MAX_KEYS {
                return invalid("too many keys in track");
            }
            preflight(&r, kn, 7, "ANIM keys")?;
            let mut keys = Vec::with_capacity(kn);
            for _ in 0..kn {
                let tick = r.u32()?;
                let interpolation = match r.u8()? {
                    0 => Interpolation::Step,
                    1 => Interpolation::Linear,
                    2 => Interpolation::Spherical,
                    x => return invalid(format!("unknown interpolation {x}")),
                };
                let fields = r.u8()?;
                if fields == 0 || fields & !0x0f != 0 {
                    return invalid("invalid animation field mask");
                }
                let translation = if fields & 1 != 0 {
                    Some([r.i32()?, r.i32()?, r.i32()?])
                } else {
                    None
                };
                let rotation = if fields & 2 != 0 {
                    let q = [r.i16()?, r.i16()?, r.i16()?, r.i16()?];
                    validate_quat(q, "animation")?;
                    Some(q)
                } else {
                    None
                };
                let scale = if fields & 4 != 0 {
                    let s = [r.u16()?, r.u16()?, r.u16()?];
                    if s.contains(&0) {
                        return invalid("animation scale component is zero");
                    }
                    Some(s)
                } else {
                    None
                };
                let visibility = if fields & 8 != 0 {
                    match r.u8()? {
                        0 => Some(false),
                        1 => Some(true),
                        _ => return invalid("visibility must be 0 or 1"),
                    }
                } else {
                    None
                };
                keys.push(Keyframe {
                    tick,
                    interpolation,
                    translation,
                    rotation,
                    scale,
                    visibility,
                });
            }
            tracks.push(Track { part_id, keys });
        }
        let en = r.u16()? as usize;
        preflight(&r, en, 6, "ANIM events")?;
        let mut events = Vec::with_capacity(en);
        for _ in 0..en {
            let tick = r.u32()?;
            let name = r.string_u8()?;
            validate_name(&name)?;
            events.push(AnimationEvent { tick, name });
        }
        clips.push(Clip {
            name,
            duration_ticks,
            loop_mode,
            tracks,
            events,
        });
    }
    r.finish()?;
    Ok(clips)
}
fn validate_parts(parts: &[Part]) -> Result<()> {
    if parts.len() > MAX_PARTS {
        return invalid("part count exceeds 1,024");
    }
    if parts.is_empty() || parts[0].id != 1 || parts[0].parent_id != 0 {
        return invalid("part hierarchy must begin with root part 1");
    }
    let mut ids = BTreeSet::new();
    let mut siblings = BTreeSet::new();
    for (index, part) in parts.iter().enumerate() {
        if part.id == 0 || !ids.insert(part.id) {
            return invalid("part IDs must be nonzero and unique");
        }
        validate_name(&part.name)?;
        if !siblings.insert((part.parent_id, part.name.as_str())) {
            return invalid("part sibling names must be unique");
        }
        let q = part.rest.rotation;
        let q_norm = q.length_squared().sqrt();
        if !part.rest.translation.is_finite()
            || !part.rest.scale.is_finite()
            || part.rest.scale.min_element() <= 0.0
            || ![q.x, q.y, q.z, q.w].iter().all(|v| v.is_finite())
            || !q_norm.is_finite()
            || (q_norm - 1.0).abs() > 0.002
        {
            return invalid("part rest transform is invalid");
        }
        if index == 0 {
            continue;
        }
        if part.parent_id == 0 || !parts[..index].iter().any(|p| p.id == part.parent_id) {
            return invalid("part parent must precede child");
        }
    }
    // Canonical topological order: among parts whose parent has already been
    // emitted, the smallest part_id comes next. This preserves arbitrary
    // stable IDs while making parent-before-child ordering deterministic.
    let mut emitted = BTreeSet::from([parts[0].id]);
    for i in 1..parts.len() {
        let next = parts[i..]
            .iter()
            .filter(|p| emitted.contains(&p.parent_id))
            .map(|p| p.id)
            .min()
            .ok_or_else(|| err("part hierarchy contains a cycle or disconnected root"))?;
        if parts[i].id != next {
            return invalid(
                "PART records are not canonical parent-before-child, ascending part_id",
            );
        }
        emitted.insert(next);
    }
    Ok(())
}

fn validate_clips(clips: &[Clip], parts: &[Part]) -> Result<()> {
    if clips.len() > MAX_CLIPS {
        return invalid("clip count exceeds 256");
    }
    let ids: BTreeSet<_> = parts.iter().map(|p| p.id).collect();
    let mut clip_names = BTreeSet::new();
    for c in clips {
        if c.tracks.len() > MAX_TRACKS {
            return invalid("track count exceeds 1,024");
        }
        validate_name(&c.name)?;
        if !clip_names.insert(&c.name) {
            return invalid("clip names are duplicated");
        }
        let mut prev = None;
        for t in &c.tracks {
            if t.keys.len() > MAX_KEYS {
                return invalid("key count exceeds 65,535");
            }
            if !ids.contains(&t.part_id) {
                return invalid("animation track references unknown part");
            }
            if prev.is_some_and(|p| p >= t.part_id) {
                return invalid("animation tracks must be sorted by unique part ID");
            }
            prev = Some(t.part_id);
            let mut tick = None;
            for k in &t.keys {
                if k.tick > c.duration_ticks || tick.is_some_and(|p| p >= k.tick) {
                    return invalid("animation keys must be strictly ordered within clip duration");
                }
                tick = Some(k.tick);
                if k.translation.is_none()
                    && k.rotation.is_none()
                    && k.scale.is_none()
                    && k.visibility.is_none()
                {
                    return invalid("animation key has no fields");
                }
                if k.interpolation == Interpolation::Spherical && k.rotation.is_none() {
                    return invalid("spherical interpolation key lacks rotation");
                }
                if let Some(q) = k.rotation {
                    validate_quat(q, "animation")?;
                }
                if k.scale.is_some_and(|scale| scale.contains(&0)) {
                    return invalid("animation scale component is zero");
                }
            }
        }
        let mut event_key = None;
        if c.events.len() > u16::MAX as usize {
            return invalid("event count exceeds 65,535");
        }
        for e in &c.events {
            if e.tick > c.duration_ticks {
                return invalid("event tick exceeds clip duration");
            }
            validate_name(&e.name)?;
            let key = (e.tick, e.name.as_str());
            if event_key.is_some_and(|p| p >= key) {
                return invalid("events must be sorted by (tick,name)");
            }
            event_key = Some(key);
        }
    }
    Ok(())
}
fn validate_quat(q: [i16; 4], kind: &str) -> Result<()> {
    let norm = q
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt()
        / 32767.0;
    if !norm.is_finite() || norm == 0.0 || (norm - 1.0).abs() > 0.002 {
        return invalid(format!("{kind} quaternion is not normalized"));
    }
    Ok(())
}

fn logical_hash(chunks: &[([u8; 4], Vec<u8>)]) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update(b"spall.asset.v1");
    for (id, data) in chunks {
        h.update(id);
        h.update(&(data.len() as u32).to_le_bytes());
        h.update(data);
    }
    h.finalize()
}
fn validate_chunk_order(chunks: &[([u8; 4], Vec<u8>)]) -> Result<()> {
    let mut prior_rank = 0u8;
    let mut prior_extension: Option<[u8; 4]> = None;
    for (index, (id, _)) in chunks.iter().enumerate() {
        if index == 0 {
            continue;
        }
        if *id == *b"HASH" {
            continue;
        }
        let rank = match id {
            b"META" => 0,
            b"MTRL" => 1,
            b"PALT" => 2,
            b"PART" => 3,
            b"VOXL" => 4,
            b"STRC" => 5,
            b"ANIM" => 6,
            _ => 7,
        };
        if rank < prior_rank {
            return invalid("chunks are not in canonical FourCC order");
        }
        if rank == 7 {
            if prior_rank == 7 && prior_extension.is_some_and(|p| p >= *id) {
                return invalid("extension chunks are not bytewise sorted");
            }
            prior_extension = Some(*id);
        } else if rank != prior_rank {
            prior_extension = None;
        }
        prior_rank = rank;
    }
    Ok(())
}
fn valid_tag_key(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}
fn validate_name(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 255 {
        return invalid("name must contain 1..=255 UTF-8 bytes");
    }
    Ok(())
}
fn fourcc(v: [u8; 4]) -> String {
    String::from_utf8_lossy(&v).into_owned()
}
fn err(s: impl Into<String>) -> AssetError {
    AssetError::Invalid(s.into())
}
fn invalid<T>(s: impl Into<String>) -> Result<T> {
    Err(err(s))
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or_else(|| err("length overflow"))?;
        let s = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| err("truncated payload"))?;
        self.offset = end;
        Ok(s)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| err("truncated fixed field"))
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn string_u8(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        self.string_bytes(n)
    }
    fn string_u16(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        self.string_bytes(n)
    }
    fn string_bytes(&mut self, n: usize) -> Result<String> {
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| err("invalid UTF-8"))
    }
    fn finish(&self) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            invalid("trailing bytes in chunk")
        }
    }
}

fn preflight(reader: &Reader<'_>, count: usize, minimum_bytes: usize, label: &str) -> Result<()> {
    let required = count
        .checked_mul(minimum_bytes)
        .ok_or_else(|| err(format!("{label} count overflows")))?;
    if required > reader.remaining() {
        return invalid(format!("{label} count exceeds remaining payload"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn u16v(o: &mut Vec<u8>, x: u16) {
        o.extend(x.to_le_bytes())
    }
    fn u32v(o: &mut Vec<u8>, x: u32) {
        o.extend(x.to_le_bytes())
    }
    fn u64v(o: &mut Vec<u8>, x: u64) {
        o.extend(x.to_le_bytes())
    }
    fn i32v(o: &mut Vec<u8>, x: i32) {
        o.extend(x.to_le_bytes())
    }
    fn i16v(o: &mut Vec<u8>, x: i16) {
        o.extend(x.to_le_bytes())
    }
    fn chunk(o: &mut Vec<u8>, id: &[u8; 4], p: &[u8]) {
        o.extend(id);
        o.push(0);
        o.extend([0; 3]);
        u32v(o, p.len() as u32);
        u32v(o, p.len() as u32);
        o.extend(p);
    }
    fn compress_chunk(file: &mut Vec<u8>, target: &[u8; 4]) {
        let mut offset = 12;
        while offset + 16 <= file.len() {
            let id: [u8; 4] = file[offset..offset + 4].try_into().unwrap();
            let stored_len =
                u32::from_le_bytes(file[offset + 8..offset + 12].try_into().unwrap()) as usize;
            let raw_len = u32::from_le_bytes(file[offset + 12..offset + 16].try_into().unwrap());
            if &id == target {
                let compressed =
                    zstd::bulk::compress(&file[offset + 16..offset + 16 + stored_len], 1).unwrap();
                let mut replacement = id.to_vec();
                replacement.push(1);
                replacement.extend([0; 3]);
                u32v(&mut replacement, compressed.len() as u32);
                u32v(&mut replacement, raw_len);
                replacement.extend(compressed);
                file.splice(offset..offset + 16 + stored_len, replacement);
                return;
            }
            offset += 16 + stored_len;
        }
        panic!("test chunk not found");
    }
    fn make(
        parts: Option<Vec<u8>>,
        vox: Vec<u8>,
        anim: Option<Vec<u8>>,
        extra: Option<(&'static [u8; 4], Vec<u8>)>,
    ) -> Vec<u8> {
        let mut chunks = vec![];
        let mut m = vec![1; 16];
        m.push(0);
        m.push(0);
        for _ in 0..3 {
            i32v(&mut m, 0);
        }
        u16v(&mut m, 4);
        m.extend(b"test");
        u64v(&mut m, 0);
        u64v(&mut m, 0);
        u16v(&mut m, 0);
        chunks.push((*b"META", m));
        chunks.push((*b"MTRL", vec![1, 0, 1, b'a']));
        if let Some(p) = parts {
            chunks.push((*b"PART", p));
        }
        chunks.push((*b"VOXL", vox));
        if let Some(a) = anim {
            chunks.push((*b"ANIM", a));
        }
        if let Some((id, p)) = extra {
            chunks.push((*id, p));
        }
        let h = logical_hash(&chunks);
        let mut out = b"SPVX\x01\0\x01\0\0\0\0\0".to_vec();
        for (id, p) in &chunks {
            chunk(&mut out, id, p);
        }
        chunk(&mut out, b"HASH", h.as_bytes());
        out
    }
    fn empty_vox() -> Vec<u8> {
        vec![0, 0, 0, 0, 0]
    }
    fn vox_runs(runs: &[(i32, u32, u16)]) -> Vec<u8> {
        let mut bytes = vec![0];
        u32v(&mut bytes, runs.len() as u32);
        for (x, length, material_slot) in runs {
            u32v(&mut bytes, 1);
            i32v(&mut bytes, *x);
            i32v(&mut bytes, 0);
            i32v(&mut bytes, 0);
            u32v(&mut bytes, *length);
            u16v(&mut bytes, *material_slot);
            u16v(&mut bytes, 0);
        }
        bytes
    }
    fn parts_root() -> Vec<u8> {
        let mut p = vec![];
        u16v(&mut p, 1);
        u32v(&mut p, 1);
        u32v(&mut p, 0);
        p.push(4);
        p.extend(b"root");
        for _ in 0..3 {
            i32v(&mut p, 0);
        }
        for q in [0, 0, 0, 32767] {
            i16v(&mut p, q);
        }
        for _ in 0..3 {
            u16v(&mut p, 1024);
        }
        u16v(&mut p, 0);
        p
    }
    fn push_part(p: &mut Vec<u8>, id: u32, parent_id: u32, name: &str) {
        u32v(p, id);
        u32v(p, parent_id);
        p.push(name.len() as u8);
        p.extend(name.as_bytes());
        for _ in 0..3 {
            i32v(p, 0);
        }
        for q in [0, 0, 0, 32767] {
            i16v(p, q);
        }
        for _ in 0..3 {
            u16v(p, 1024);
        }
        u16v(p, 0);
    }
    fn parts_nonmonotonic_deep() -> Vec<u8> {
        let mut p = vec![];
        u16v(&mut p, 4);
        push_part(&mut p, 1, 0, "root");
        push_part(&mut p, 3, 1, "sibling");
        push_part(&mut p, 9, 1, "parent");
        push_part(&mut p, 2, 9, "child");
        p
    }
    fn anim_scale_two_keys() -> Vec<u8> {
        let mut a = vec![];
        u16v(&mut a, 1);
        a.extend([4, b't', b'e', b's', b't']);
        u32v(&mut a, 60);
        a.push(1);
        u16v(&mut a, 1);
        u32v(&mut a, 1);
        u16v(&mut a, 2);
        for (tick, scale) in [(0, 1024), (60, 2048)] {
            u32v(&mut a, tick);
            a.extend([1, 4]);
            for _ in 0..3 {
                u16v(&mut a, scale);
            }
        }
        u16v(&mut a, 0);
        a
    }
    fn anim_two_keys(q0: [i16; 4], q1: [i16; 4]) -> Vec<u8> {
        let mut a = vec![];
        u16v(&mut a, 1);
        a.push(4);
        a.extend(b"turn");
        u32v(&mut a, 60);
        a.push(1);
        u16v(&mut a, 1);
        u32v(&mut a, 1);
        u16v(&mut a, 2);
        u32v(&mut a, 0);
        a.push(1);
        a.push(3);
        for x in [0, 0, 0] {
            i32v(&mut a, x);
        }
        for x in q0 {
            i16v(&mut a, x);
        }
        u32v(&mut a, 60);
        a.push(1);
        a.push(3);
        for x in [256, 0, 0] {
            i32v(&mut a, x);
        }
        for x in q1 {
            i16v(&mut a, x);
        }
        u16v(&mut a, 0);
        a
    }
    fn anim_spherical(q0: [i16; 4], q1: [i16; 4]) -> Vec<u8> {
        let mut animation = anim_two_keys(q0, q1);
        animation[24] = 2;
        animation
    }
    fn truncated_anim_count(kind: &str) -> Vec<u8> {
        let mut animation = vec![];
        u16v(&mut animation, 1);
        animation.extend([1, b'x']);
        u32v(&mut animation, 1);
        animation.push(0);
        match kind {
            "tracks" => u16v(&mut animation, MAX_TRACKS as u16),
            "keys" => {
                u16v(&mut animation, 1);
                u32v(&mut animation, 1);
                u16v(&mut animation, MAX_KEYS as u16);
            }
            "events" => {
                u16v(&mut animation, 0);
                u16v(&mut animation, u16::MAX);
            }
            _ => unreachable!(),
        }
        animation
    }
    #[test]
    fn static_implicit_root_and_symbolic_runs_are_preserved() {
        let mut v = vec![0];
        u32v(&mut v, 1);
        u32v(&mut v, 1);
        for x in [-2, 0, 4] {
            i32v(&mut v, x);
        }
        u32v(&mut v, 2);
        u16v(&mut v, 1);
        u16v(&mut v, 0);
        let asset = decode(&make(None, v, None, None)).unwrap();
        assert_eq!(asset.parts[0].id, 1);
        assert_eq!(asset.runs[0].start, [-2, 0, 4]);
        assert_eq!(asset.runs[0].length_x, 2);
        assert_eq!(asset.material_keys, vec!["a"]);
    }
    #[test]
    fn samples_rest_pose_and_sixty_hz_linear_motion() {
        let a = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        ))
        .unwrap();
        let mid = sample_clip(&a, "turn", 30.0).unwrap();
        assert!((mid[&1].transform.translation.x - 0.5).abs() < 1e-6);
        assert_eq!(mid[&1].transform.scale, Vec3::ONE);
        let scaled = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_scale_two_keys()),
            None,
        ))
        .unwrap();
        let midpoint = sample_clip(&scaled, "test", 30.0).unwrap();
        assert!((midpoint[&1].transform.scale.x - 1.5).abs() < 1e-6);
        assert!(midpoint[&1].transform.scale.is_finite());
    }
    #[test]
    fn fields_keep_rest_values_until_their_first_key() {
        let mut part = parts_root();
        part[15..19].copy_from_slice(&512i32.to_le_bytes());
        let mut animation = anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767]);
        animation[20..24].copy_from_slice(&10u32.to_le_bytes());
        let asset = decode(&make(Some(part), empty_vox(), Some(animation), None)).unwrap();
        let pose = sample_clip(&asset, "turn", 0.0).unwrap();
        assert_eq!(pose[&1].transform.translation.x, 2.0);
    }
    #[test]
    fn quaternion_sampling_takes_shortest_path() {
        let a = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_spherical([0, 0, 0, 32767], [0, 0, 0, -32767])),
            None,
        ))
        .unwrap();
        let mid = sample_clip(&a, "turn", 30.0).unwrap();
        assert!(mid[&1].transform.rotation.dot(Quat::IDENTITY).abs() > 0.999);
        assert!((mid[&1].transform.translation.x - 0.5).abs() < 1e-6);
    }
    #[test]
    fn canonical_hierarchy_allows_deep_parent_before_child_with_arbitrary_ids() {
        let mut part = vec![];
        u16v(&mut part, 4);
        push_part(&mut part, 1, 0, "root");
        push_part(&mut part, 10, 1, "left");
        push_part(&mut part, 2, 10, "left-child");
        push_part(&mut part, 20, 1, "right");
        let asset = decode(&make(Some(part), empty_vox(), None, None)).unwrap();
        assert_eq!(
            asset.parts.iter().map(|p| p.id).collect::<Vec<_>>(),
            [1, 10, 2, 20]
        );
        let authored_order = decode(&make(
            Some(parts_nonmonotonic_deep()),
            empty_vox(),
            None,
            None,
        ))
        .unwrap();
        assert_eq!(
            authored_order
                .parts
                .iter()
                .map(|p| p.id)
                .collect::<Vec<_>>(),
            [1, 3, 9, 2]
        );
    }
    #[test]
    fn truncated_declared_counts_and_compressed_meta_hash_reject_before_decode() {
        let mut huge_runs = vec![0];
        u32v(&mut huge_runs, MAX_RUNS as u32);
        assert!(decode(&make(None, huge_runs, None, None)).is_err());

        let mut compressed_meta = make(None, empty_vox(), None, None);
        compress_chunk(&mut compressed_meta, b"META");
        assert!(
            decode(&compressed_meta)
                .unwrap_err()
                .to_string()
                .contains("raw codec")
        );

        let mut compressed_hash = make(None, empty_vox(), None, None);
        compress_chunk(&mut compressed_hash, b"HASH");
        assert!(
            decode(&compressed_hash)
                .unwrap_err()
                .to_string()
                .contains("raw codec")
        );
        for kind in ["tracks", "keys", "events"] {
            let malformed = make(None, empty_vox(), Some(truncated_anim_count(kind)), None);
            assert!(decode(&malformed).is_err(), "{kind} count must preflight");
        }
    }
    #[test]
    fn minor_zero_accepts_documented_part_and_anim_layout() {
        let mut bytes = make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        );
        bytes[6] = 0;
        let asset = decode(&bytes).unwrap();
        assert_eq!(asset.parts[0].structural_profile_slot, 0);
        assert_eq!(
            sample_clip(&asset, "turn", 30.0).unwrap()[&1]
                .transform
                .translation
                .x,
            0.5
        );

        // The repository has no historical v1.0 PART record layout; the
        // documented PART schema includes structural_profile_slot.
        let mut undocumented = parts_root();
        undocumented.truncate(undocumented.len() - 2);
        let mut bytes = make(Some(undocumented), empty_vox(), None, None);
        bytes[6] = 0;
        assert!(decode(&bytes).is_err());
    }
    #[test]
    fn mutated_public_asset_returns_sampling_error_without_panicking() {
        let mut asset = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        ))
        .unwrap();
        asset.clips[0].tracks[0].part_id = 99;
        assert!(sample_clip(&asset, "turn", 1.0).is_err());

        let mut asset = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        ))
        .unwrap();
        asset.clips[0].tracks[0].keys[0].rotation = Some([0; 4]);
        assert!(sample_clip(&asset, "turn", 1.0).is_err());

        let mut asset = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        ))
        .unwrap();
        asset.clips[0].tracks[0].keys[0].translation = None;
        asset.clips[0].tracks[0].keys[0].rotation = None;
        assert!(sample_clip(&asset, "turn", 1.0).is_err());
    }
    #[test]
    fn pingpong_keeps_both_endpoints() {
        let mut a = decode(&make(
            Some(parts_root()),
            empty_vox(),
            Some(anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767])),
            None,
        ))
        .unwrap();
        a.clips[0].loop_mode = LoopMode::PingPong;
        let end = sample_clip(&a, "turn", 60.0).unwrap();
        let back = sample_clip(&a, "turn", 90.0).unwrap();
        assert_eq!(end[&1].transform.translation.x, 1.0);
        assert!((back[&1].transform.translation.x - 0.5).abs() < 1e-6);
        a.clips[0].loop_mode = LoopMode::Loop;
        let wrapped = sample_clip(&a, "turn", 60.0).unwrap();
        assert_eq!(wrapped[&1].transform.translation.x, 0.0);
        a.clips[0].loop_mode = LoopMode::Once;
        let clamped = sample_clip(&a, "turn", 90.0).unwrap();
        assert_eq!(clamped[&1].transform.translation.x, 1.0);
    }
    #[test]
    fn rejects_bad_hash_hierarchy_and_animation_reference() {
        let mut b = make(None, empty_vox(), None, None);
        *b.last_mut().unwrap() ^= 1;
        assert!(decode(&b).is_err());
        let mut bad = parts_root();
        bad[4..8].copy_from_slice(&9u32.to_le_bytes());
        assert!(decode(&make(Some(bad), empty_vox(), None, None)).is_err());
        let mut anim = anim_two_keys([0, 0, 0, 32767], [0, 0, 0, 32767]);
        anim[14..18].copy_from_slice(&9u32.to_le_bytes());
        assert!(decode(&make(Some(parts_root()), empty_vox(), Some(anim), None)).is_err());
    }
    #[test]
    fn rejects_run_endpoint_overflow_unknown_slots_and_noncanonical_adjacency() {
        assert!(decode(&make(None, vox_runs(&[(i32::MAX, 2, 1)]), None, None)).is_err());
        assert!(decode(&make(None, vox_runs(&[(0, 1, 2)]), None, None)).is_err());
        assert!(decode(&make(None, vox_runs(&[(0, 1, 1), (1, 1, 1)]), None, None)).is_err());
    }
    #[test]
    fn explicitly_rejects_strc_and_layr() {
        assert!(
            decode(&make(None, empty_vox(), None, Some((b"LAYR", vec![]))))
                .unwrap_err()
                .to_string()
                .contains("LAYR")
        );
        assert!(
            decode(&make(None, empty_vox(), None, Some((b"STRC", vec![]))))
                .unwrap_err()
                .to_string()
                .contains("STRC")
        );
    }
    #[test]
    fn rejects_nonfinite_time() {
        let a = decode(&make(
            None,
            empty_vox(),
            Some({
                let mut x = vec![];
                u16v(&mut x, 0);
                x
            }),
            None,
        ))
        .unwrap();
        assert!(sample_clip(&a, "x", f64::NAN).is_err());
    }
}
