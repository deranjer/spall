//! Deterministic authoring source for the sandbox's small articulated wildlife fixtures.
//! Run with `cargo run -p sandbox --example animal_fixtures -- --write`.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
};

const OUT: &str = "examples/sandbox/assets/animals";
const CELL_SIZE_CM: u8 = 1; // SPVX code 1 = 6.25 cm.
const PALETTE: [[u8; 3]; 5] = [
    [76, 129, 170],
    [205, 179, 123],
    [44, 67, 49],
    [120, 91, 55],
    [224, 220, 191],
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Voxel {
    x: i32,
    y: i32,
    z: i32,
    material: &'static str,
    tint: u16,
}
#[derive(Clone, Debug)]
struct Part {
    id: u32,
    parent: u32,
    name: &'static str,
    at: [i32; 3],
    voxels: Vec<Voxel>,
}
#[derive(Clone, Copy)]
struct Key {
    tick: u32,
    interpolation: u8,
    fields: u8,
    translation: Option<[i32; 3]>,
    rotation: Option<[i16; 4]>,
    scale: Option<[u16; 3]>,
    visibility: Option<u8>,
}
struct Track {
    part: u32,
    keys: Vec<Key>,
}
struct Clip {
    name: &'static str,
    duration: u32,
    mode: u8,
    tracks: Vec<Track>,
}
struct Animal {
    name: &'static str,
    uuid: [u8; 16],
    parts: Vec<Part>,
    clips: Vec<Clip>,
    extent: [i32; 6],
}
type DecodedChunk = ([u8; 4], Vec<u8>);

fn part(id: u32, parent: u32, name: &'static str, at: [i32; 3]) -> Part {
    Part {
        id,
        parent,
        name,
        at,
        voxels: Vec::new(),
    }
}
fn voxel(p: &mut Part, x: i32, y: i32, z: i32, m: &'static str, t: u16) {
    p.voxels.push(Voxel {
        x,
        y,
        z,
        material: m,
        tint: t,
    });
}
fn cuboid(p: &mut Part, bounds: [i32; 6], m: &'static str, t: u16) {
    for z in bounds[4]..=bounds[5] {
        for y in bounds[2]..=bounds[3] {
            for x in bounds[0]..=bounds[1] {
                voxel(p, x, y, z, m, t);
            }
        }
    }
}
fn q(axis: usize, degrees: f64) -> [i16; 4] {
    let a = degrees.to_radians() / 2.0;
    let mut q = [0i16; 4];
    q[axis] = (a.sin() * 32767.0).round() as i16;
    q[3] = (a.cos() * 32767.0).round() as i16;
    q
}
fn key(tick: u32, rotation: Option<[i16; 4]>, translation: Option<[i32; 3]>) -> Key {
    let mut fields = 0;
    if translation.is_some() {
        fields |= 1
    }
    if rotation.is_some() {
        fields |= 2
    }
    Key {
        tick,
        interpolation: if rotation.is_some() { 2 } else { 1 },
        fields,
        translation,
        rotation,
        scale: None,
        visibility: None,
    }
}
fn rot_track(id: u32, axis: usize, angles: &[(u32, f64)]) -> Track {
    Track {
        part: id,
        keys: angles
            .iter()
            .map(|&(t, a)| key(t, Some(q(axis, a)), None))
            .collect(),
    }
}
fn translate_track(id: u32, vals: &[(u32, [i32; 3])]) -> Track {
    Track {
        part: id,
        keys: vals.iter().map(|&(t, v)| key(t, None, Some(v))).collect(),
    }
}
fn anim(name: &'static str, duration: u32, tracks: Vec<Track>) -> Clip {
    Clip {
        name,
        duration,
        mode: 1,
        tracks,
    }
}

fn trout() -> Animal {
    let mut body = part(1, 0, "body", [0, 0, 0]);
    for x in -4i32..=4 {
        let (w, h) = if x.abs() <= 1 {
            (1, 2)
        } else if x.abs() <= 3 {
            (2, 1)
        } else {
            (1, 0)
        };
        for z in -w..=w {
            for y in -h..=h {
                let tint = if y == h && x.abs() < 3 {
                    1
                } else if y == -h && x.abs() < 3 {
                    2
                } else {
                    0
                };
                voxel(&mut body, x, y, z, "stone", tint);
            }
        }
    }
    cuboid(&mut body, [5, 6, 0, 0, -1, 1], "stone", 1); // blunt, distinct head/muzzle
    let mut tail = part(2, 1, "tail_peduncle_and_fork", [-4, 0, 0]);
    cuboid(&mut tail, [-2, 0, 0, 0, -1, 1], "stone", 0);
    cuboid(&mut tail, [-4, -3, -2, 2, 0, 0], "moss", 3); // vertically forked caudal fin
    let mut dorsal = part(3, 1, "dorsal_fin", [-1, 2, 0]);
    cuboid(&mut dorsal, [-1, 1, 0, 2, -1, 1], "moss", 3);
    let mut pectoral = part(4, 1, "pectoral_fin_near", [1, 0, 1]);
    cuboid(&mut pectoral, [-1, 1, -1, 0, 0, 2], "moss", 3);
    let mut far = part(5, 1, "pectoral_fin_far", [1, 0, -1]);
    cuboid(&mut far, [-1, 1, -1, 0, -2, 0], "moss", 3);
    let idle = anim(
        "idle",
        120,
        vec![translate_track(
            1,
            &[
                (0, [0, 0, 0]),
                (30, [0, 24, 0]),
                (60, [0, 0, 0]),
                (90, [0, -24, 0]),
                (120, [0, 0, 0]),
            ],
        )],
    );
    let swim = anim(
        "swim",
        60,
        vec![rot_track(
            2,
            1,
            &[(0, 0.0), (15, 22.0), (30, 0.0), (45, -22.0), (60, 0.0)],
        )],
    );
    Animal {
        name: "Trout",
        uuid: [
            0x5a, 0x87, 0x10, 0x01, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ],
        parts: vec![body, tail, dorsal, pectoral, far],
        clips: vec![idle, swim],
        extent: [-8, 6, -2, 4, -3, 3],
    }
}
fn crow() -> Animal {
    let mut body = part(1, 0, "torso", [0, 0, 0]);
    cuboid(&mut body, [-3, 3, -1, 1, -1, 1], "stone", 3);
    let mut head = part(2, 1, "head", [3, 1, 0]);
    for x in 0..=2 {
        for y in 0..=2 {
            for z in -1i32..=1 {
                if !(x == 1 && y == 1 && z.abs() == 1) {
                    voxel(&mut head, x, y, z, "stone", 3)
                }
            }
        }
    }
    cuboid(&mut head, [3, 4, 0, 0, -1, 1], "wood", 5);
    voxel(&mut head, 1, 1, 1, "snow", 5);
    voxel(&mut head, 1, 1, -1, "snow", 5);
    let mut near = part(3, 1, "wing_near", [0, 1, 1]);
    for x in -2..=2 {
        for z in 0..=5 {
            let y = if z >= 4 { -(z - 3) } else { 1 };
            voxel(&mut near, x, y, z, "stone", 3);
        }
    }
    let mut far = part(4, 1, "wing_far", [0, 1, -1]);
    for x in -2..=2 {
        for z in -5..=0 {
            let y = if z <= -4 { z + 3 } else { 1 };
            voxel(&mut far, x, y, z, "stone", 3);
        }
    }
    let mut tail = part(5, 1, "tail_fan", [-3, 0, 0]);
    cuboid(&mut tail, [-3, -1, -1, 1, -2, 2], "stone", 3);
    let mut legs = part(6, 1, "legs_and_feet", [0, -1, 0]);
    for z in [-1, 1] {
        cuboid(&mut legs, [-1, 0, -4, -2, z, z], "wood", 4);
        cuboid(&mut legs, [-2, 1, -5, -5, z, z], "wood", 4);
    }
    let idle = anim(
        "idle",
        120,
        vec![translate_track(
            1,
            &[
                (0, [0, 0, 0]),
                (30, [0, 16, 0]),
                (60, [0, 0, 0]),
                (90, [0, 16, 0]),
                (120, [0, 0, 0]),
            ],
        )],
    );
    let flap = anim(
        "flap",
        48,
        vec![
            rot_track(
                3,
                0,
                &[(0, -48.0), (12, 38.0), (24, -48.0), (36, 38.0), (48, -48.0)],
            ),
            rot_track(
                4,
                0,
                &[(0, 48.0), (12, -38.0), (24, 48.0), (36, -38.0), (48, 48.0)],
            ),
        ],
    );
    Animal {
        name: "Crow",
        uuid: [
            0x5a, 0x87, 0x10, 0x02, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x02,
        ],
        parts: vec![body, head, near, far, tail, legs],
        clips: vec![idle, flap],
        extent: [-6, 7, -6, 3, -6, 6],
    }
}
fn deer() -> Animal {
    let mut body = part(1, 0, "torso", [0, 0, 0]);
    for x in -4i32..=4 {
        let half = if x.abs() < 3 { 1 } else { 0 };
        cuboid(&mut body, [x, x, -1, 1, -half, half], "wood", 4);
    }
    let mut neck = part(2, 1, "neck", [3, 1, 0]);
    cuboid(&mut neck, [0, 2, 0, 4, -1, 1], "wood", 4);
    let mut head = part(3, 2, "head", [1, 4, 0]);
    for x in 0..=2 {
        for y in 0..=2 {
            for z in -1i32..=1 {
                if !(x == 0 && y == 1 && z.abs() == 1) {
                    voxel(&mut head, x, y, z, "wood", 4)
                }
            }
        }
    }
    cuboid(&mut head, [3, 4, 0, 0, -1, 1], "dirt", 4);
    voxel(&mut head, 0, 1, 1, "snow", 5);
    voxel(&mut head, 0, 1, -1, "snow", 5);
    let mut antlers = part(4, 3, "branched_antlers", [1, 2, 0]);
    for side in [-1, 1] {
        for y in 0..=3 {
            voxel(&mut antlers, 0, y, side, "snow", 5);
        }
        for x in 1..=2 {
            voxel(&mut antlers, x, 2, side, "snow", 5);
            if x < 2 {
                voxel(&mut antlers, x, 3, side, "snow", 5);
            }
        }
        voxel(&mut antlers, 1, 4, side, "snow", 5);
    }
    let mut front = part(5, 1, "forelegs", [2, -1, 0]);
    for z in [-1, 1] {
        cuboid(&mut front, [-1, 0, -5, -1, z, z], "wood", 4);
        cuboid(&mut front, [-1, 1, -6, -6, z, z], "dirt", 4);
    }
    let mut hind = part(6, 1, "hindlegs", [-3, -1, 0]);
    for z in [-1, 1] {
        cuboid(&mut hind, [-1, 0, -5, -1, z, z], "wood", 4);
        cuboid(&mut hind, [-1, 1, -6, -6, z, z], "dirt", 4);
    }
    let mut tail = part(7, 1, "tail", [-4, 1, 0]);
    cuboid(&mut tail, [-1, 0, -1, 0, -1, 1], "wood", 4);
    cuboid(&mut tail, [-1, 0, -2, -2, -1, 1], "snow", 5);
    let idle = anim(
        "idle",
        120,
        vec![translate_track(
            1,
            &[
                (0, [0, 0, 0]),
                (30, [0, 12, 0]),
                (60, [0, 0, 0]),
                (90, [0, 12, 0]),
                (120, [0, 0, 0]),
            ],
        )],
    );
    let walk = anim(
        "walk",
        48,
        vec![
            rot_track(
                5,
                2,
                &[(0, -24.0), (12, 24.0), (24, -24.0), (36, 24.0), (48, -24.0)],
            ),
            rot_track(
                6,
                2,
                &[(0, 24.0), (12, -24.0), (24, 24.0), (36, -24.0), (48, 24.0)],
            ),
        ],
    );
    Animal {
        name: "Deer",
        uuid: [
            0x5a, 0x87, 0x10, 0x03, 0x20, 0x26, 0x40, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x03,
        ],
        parts: vec![body, neck, head, antlers, front, hind, tail],
        clips: vec![idle, walk],
        extent: [-5, 8, -7, 11, -1, 1],
    }
}

fn put_u16(v: &mut Vec<u8>, n: u16) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn put_i32(v: &mut Vec<u8>, n: i32) {
    v.extend_from_slice(&n.to_le_bytes())
}
fn str8(v: &mut Vec<u8>, s: &str) {
    v.push(s.len() as u8);
    v.extend_from_slice(s.as_bytes())
}
fn str16(v: &mut Vec<u8>, s: &str) {
    put_u16(v, s.len() as u16);
    v.extend_from_slice(s.as_bytes())
}
fn quat_identity() -> [i16; 4] {
    [0, 0, 0, 32767]
}
fn meta(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&a.uuid);
    b.extend([CELL_SIZE_CM, 0]);
    for v in [0i32, 0, 0] {
        put_i32(&mut b, v)
    }
    str16(&mut b, a.name);
    b.extend([0; 16]);
    put_u16(&mut b, 0);
    b
}
fn part_chunk(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, a.parts.len() as u16);
    for p in &a.parts {
        put_u32(&mut b, p.id);
        put_u32(&mut b, p.parent);
        str8(&mut b, p.name);
        for n in p.at {
            put_i32(&mut b, n * 256)
        }
        for n in quat_identity() {
            b.extend_from_slice(&n.to_le_bytes())
        }
        for _ in 0..3 {
            put_u16(&mut b, 1024)
        }
        put_u16(&mut b, 0)
    }
    b
}
fn voxels(a: &Animal, slots: &BTreeMap<&str, u16>) -> Vec<u8> {
    let mut rows = Vec::new();
    for p in &a.parts {
        let mut cells = p.voxels.clone();
        cells.sort_by_key(|c| (c.z, c.y, c.x, c.material, c.tint));
        let mut i = 0;
        while i < cells.len() {
            let c = cells[i];
            let mut end = i + 1;
            while end < cells.len()
                && cells[end].z == c.z
                && cells[end].y == c.y
                && cells[end].x == cells[end - 1].x + 1
                && cells[end].material == c.material
                && cells[end].tint == c.tint
            {
                end += 1
            }
            rows.push((
                p.id,
                c.x,
                c.y,
                c.z,
                (end - i) as u32,
                slots[c.material],
                c.tint,
            ));
            i = end;
        }
    }
    rows.sort_by_key(|r| (r.0, r.3, r.2, r.1));
    let mut b = vec![0];
    put_u32(&mut b, rows.len() as u32);
    for (part, x, y, z, len, mat, tint) in rows {
        put_u32(&mut b, part);
        put_i32(&mut b, x);
        put_i32(&mut b, y);
        put_i32(&mut b, z);
        put_u32(&mut b, len);
        put_u16(&mut b, mat);
        put_u16(&mut b, tint);
    }
    b
}
fn anim_chunk(a: &Animal) -> Vec<u8> {
    let mut b = Vec::new();
    put_u16(&mut b, a.clips.len() as u16);
    for c in &a.clips {
        str8(&mut b, c.name);
        put_u32(&mut b, c.duration);
        b.push(c.mode);
        put_u16(&mut b, c.tracks.len() as u16);
        for t in &c.tracks {
            put_u32(&mut b, t.part);
            put_u16(&mut b, t.keys.len() as u16);
            for k in &t.keys {
                put_u32(&mut b, k.tick);
                b.push(k.interpolation);
                b.push(k.fields);
                if let Some(v) = k.translation {
                    for n in v {
                        put_i32(&mut b, n)
                    }
                }
                if let Some(v) = k.rotation {
                    for n in v {
                        b.extend_from_slice(&n.to_le_bytes())
                    }
                }
                if let Some(v) = k.scale {
                    for n in v {
                        put_u16(&mut b, n)
                    }
                }
                if let Some(v) = k.visibility {
                    b.push(v)
                }
            }
        }
        put_u16(&mut b, 0)
    }
    b
}
fn chunk(id: &[u8; 4], payload: &[u8], out: &mut Vec<u8>) {
    out.extend(id);
    out.extend([0, 0, 0, 0]);
    put_u32(out, payload.len() as u32);
    put_u32(out, payload.len() as u32);
    out.extend(payload)
}
fn encode(a: &Animal) -> Vec<u8> {
    let materials: BTreeSet<&str> = a
        .parts
        .iter()
        .flat_map(|p| p.voxels.iter().map(|v| v.material))
        .collect();
    let slots: BTreeMap<&str, u16> = materials
        .iter()
        .enumerate()
        .map(|(i, &m)| (m, (i + 1) as u16))
        .collect();
    let mut chunks = Vec::new();
    let mut mtrl = Vec::new();
    put_u16(&mut mtrl, materials.len() as u16);
    for m in &materials {
        str8(&mut mtrl, m)
    }
    let mut palt = Vec::new();
    put_u16(&mut palt, PALETTE.len() as u16);
    for rgb in PALETTE {
        palt.extend(rgb)
    }
    chunks.push((*b"META", meta(a)));
    chunks.push((*b"MTRL", mtrl));
    chunks.push((*b"PALT", palt));
    chunks.push((*b"PART", part_chunk(a)));
    chunks.push((*b"VOXL", voxels(a, &slots)));
    chunks.push((*b"ANIM", anim_chunk(a)));
    let mut hash = blake3::Hasher::new();
    hash.update(b"spall.asset.v1");
    for (id, payload) in &chunks {
        hash.update(id);
        hash.update(&(payload.len() as u32).to_le_bytes());
        hash.update(payload);
    }
    let digest = hash.finalize();
    let mut out = b"SPVX\x01\x00\x01\x00\0\0\0\0".to_vec();
    for (id, payload) in &chunks {
        chunk(id, payload, &mut out)
    }
    chunk(b"HASH", digest.as_bytes(), &mut out);
    out
}

fn validate(a: &Animal, bytes: &[u8]) -> Result<(), String> {
    if a.uuid.iter().all(|&v| v == 0) {
        return Err(format!("{} has zero UUID", a.name));
    }
    if a.parts.len() < 4 {
        return Err(format!("{} hierarchy too small", a.name));
    }
    let ids: BTreeSet<u32> = a.parts.iter().map(|p| p.id).collect();
    if !ids.contains(&1)
        || a.parts
            .iter()
            .any(|p| p.id != 1 && (!ids.contains(&p.parent) || p.parent >= p.id))
    {
        return Err(format!("{} invalid parent order", a.name));
    }
    let occupied: usize = a.parts.iter().map(|p| p.voxels.len()).sum();
    if occupied < 35 {
        return Err(format!("{} has only {occupied} authored cells", a.name));
    }
    let names: BTreeSet<_> = a.clips.iter().map(|c| c.name).collect();
    if !names.contains("idle") {
        return Err(format!("{} lacks idle", a.name));
    }
    let action = match a.name {
        "Trout" => "swim",
        "Crow" => "flap",
        "Deer" => "walk",
        _ => return Err("unknown animal".into()),
    };
    let clip = a
        .clips
        .iter()
        .find(|c| c.name == action)
        .ok_or("missing action clip")?;
    if clip.tracks.is_empty()
        || clip.tracks.iter().any(|t| t.keys.len() < 3)
        || clip
            .tracks
            .iter()
            .any(|t| t.keys.last().unwrap().tick != clip.duration)
    {
        return Err(format!("{} {action} clip lacks cyclic end keys", a.name));
    }
    let expected_parts: &[u32] = match a.name {
        "Trout" => &[2],
        "Crow" => &[3, 4],
        "Deer" => &[5, 6],
        _ => unreachable!(),
    };
    if clip.tracks.iter().map(|t| t.part).collect::<Vec<_>>() != expected_parts
        || clip.tracks.iter().any(|t| {
            t.keys.iter().all(|k| k.rotation.is_none())
                || t.keys.iter().all(|k| k.rotation == t.keys[0].rotation)
        })
    {
        return Err(format!(
            "{} {action} does not animate the expected articulated parts",
            a.name
        ));
    }
    let idle = a.clips.iter().find(|c| c.name == "idle").unwrap();
    if idle.tracks.iter().all(|t| {
        t.keys
            .iter()
            .all(|k| k.translation.is_none() && k.rotation.is_none())
    }) {
        return Err(format!("{} idle clip has no subtle motion", a.name));
    }
    if a.parts.iter().any(|p| p.voxels.is_empty()) {
        return Err(format!("{} has empty part", a.name));
    }
    let mut seen = BTreeSet::new();
    for p in &a.parts {
        for v in &p.voxels {
            if !seen.insert((p.id, v.x, v.y, v.z)) {
                return Err(format!(
                    "{} duplicate cell in {} at {},{},{}",
                    a.name, p.name, v.x, v.y, v.z
                ));
            }
        }
    }
    let mut origins = BTreeMap::new();
    let mut bounds = [i32::MAX, i32::MIN, i32::MAX, i32::MIN, i32::MAX, i32::MIN];
    for p in &a.parts {
        let parent = if p.parent == 0 {
            [0; 3]
        } else {
            *origins.get(&p.parent).ok_or("missing parent origin")?
        };
        let at = [
            parent[0] + p.at[0],
            parent[1] + p.at[1],
            parent[2] + p.at[2],
        ];
        origins.insert(p.id, at);
        for v in &p.voxels {
            let xyz = [v.x + at[0], v.y + at[1], v.z + at[2]];
            for axis in 0..3 {
                bounds[axis * 2] = bounds[axis * 2].min(xyz[axis]);
                bounds[axis * 2 + 1] = bounds[axis * 2 + 1].max(xyz[axis]);
            }
        }
    }
    if bounds != a.extent {
        return Err(format!(
            "{} bounds {:?}, expected {:?}",
            a.name, bounds, a.extent
        ));
    }
    let decoded = decode_chunks(bytes)?;
    if decoded.len() != 7
        || !decoded.iter().any(|(id, _)| id == b"PART")
        || !decoded.iter().any(|(id, _)| id == b"ANIM")
    {
        return Err("chunk set invalid".into());
    }
    Ok(())
}
fn decode_chunks(b: &[u8]) -> Result<Vec<DecodedChunk>, String> {
    if b.len() < 12 || &b[..4] != b"SPVX" {
        return Err("bad SPVX header".into());
    }
    let mut i = 12;
    let mut chunks = Vec::new();
    let mut hash = blake3::Hasher::new();
    hash.update(b"spall.asset.v1");
    while i < b.len() {
        if b.len() - i < 16 {
            return Err("short chunk header".into());
        }
        let id: [u8; 4] = b[i..i + 4].try_into().unwrap();
        let codec = b[i + 4];
        let stored = u32::from_le_bytes(b[i + 8..i + 12].try_into().unwrap()) as usize;
        let raw = u32::from_le_bytes(b[i + 12..i + 16].try_into().unwrap()) as usize;
        i += 16;
        if codec != 0 || stored != raw || i + stored > b.len() {
            return Err("bad raw chunk length/codec".into());
        }
        let payload = b[i..i + stored].to_vec();
        i += stored;
        if &id == b"HASH" {
            if payload.len() != 32
                || i != b.len()
                || payload.as_slice() != hash.finalize().as_bytes()
            {
                return Err("SPVX BLAKE3 mismatch".into());
            }
        } else {
            hash.update(&id);
            hash.update(&(raw as u32).to_le_bytes());
            hash.update(&payload);
        }
        chunks.push((id, payload));
    }
    if chunks.last().map(|x| x.0) != Some(*b"HASH") {
        return Err("missing final HASH".into());
    }
    Ok(chunks)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    let check = args.iter().any(|a| a == "--check");
    let root =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?).join("../..");
    let dir = root.join(OUT);
    let animals = [trout(), crow(), deer()];
    let mut uuids = BTreeSet::new();
    let manifest = sandbox::game::manifest();
    let manifest_names: BTreeSet<&str> = manifest
        .entries()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    for a in &animals {
        if !uuids.insert(a.uuid) {
            return Err(format!("{} reuses an animal UUID", a.name));
        }
        for material in a
            .parts
            .iter()
            .flat_map(|p| p.voxels.iter().map(|v| v.material))
        {
            if !manifest_names.contains(material) {
                return Err(format!(
                    "{} references unknown sandbox material {material}",
                    a.name
                ));
            }
        }
        let first = encode(a);
        let second = encode(a);
        if first != second {
            return Err(format!("{} generation is nondeterministic", a.name));
        }
        validate(a, &first)?;
        let path = dir.join(format!("{}.spvox", a.name.to_lowercase()));
        if check {
            let actual = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if actual != first {
                return Err(format!(
                    "{} differs; regenerate with --write",
                    path.display()
                ));
            }
        } else {
            fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            fs::write(&path, &first).map_err(|e| e.to_string())?;
            println!(
                "wrote {} ({} bytes, {} cells, {} parts)",
                path.display(),
                first.len(),
                a.parts.iter().map(|p| p.voxels.len()).sum::<usize>(),
                a.parts.len()
            )
        }
    }
    if check {
        println!("animal SPVX fixtures deterministic and internally validated")
    };
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("animal fixture generator: {e}");
        std::process::exit(1)
    }
}
