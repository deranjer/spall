# Spall Voxel Asset (`.spvox`) format

**Status:** approved format contract. The editor currently implements canonical
static-asset `META`/`MTRL`/`PALT`/`VOXL`/`HASH` reading and writing; `PART`,
`ANIM`, and the MagicaVoxel bridge remain pending.  
**Version:** 1.1  
**File extension:** `.spvox`  
**Magic:** `SPVX`

## 1. Purpose and scope

Spall Voxel Asset (SPVX) is the portable, authored-asset format for reusable
voxel objects: props, modular building pieces, destructible set pieces, and
later animated assemblies. It is intentionally distinct from all of the
following:

- **Editor project/scene documents.** The editor's current RON files own
  project-local `AssetId` values, scene placement, and undo history. SPVX owns
  one reusable asset, not a project.
- **Authoritative world saves, baselines, and network records.** SPVX contains
  no `WorldId`, runtime entity ID, volume ID, revision, body pose, ownership,
  physics handle, journal record, or protocol transaction. Those formats keep
  their own versioning and recovery contracts.
- **Render or collision caches.** Meshes, merged cuboids, lighting clipmaps,
  ambient occlusion, and generated LODs are derived after import and are never
  SPVX source data.

The format preserves authored topology, portable material references, display
tints, a part hierarchy, optional structural-profile intent, and optional
transform animation. It does not make a claim that every preserved feature
already has a Spall runtime implementation.

## 2. Design rules

1. **Portable materials use names, not engine numeric IDs.** A file names
   material keys such as `"stone.granite"`; an importing project maps them to
   its validated world manifest. `MaterialId` values are deliberately not
   serialized into an interchange asset.
2. **Materials are the physical baseline; profiles are authored intent.** A
   material supplies density, tool hardness, and baseline bond strength. An
   optional part or joint profile names how an assembly is built — for example,
   a steel frame, wooden planking, a weld, or glue — and is resolved by the
   importing game's structural catalog. SPVX never carries unbounded raw
   toughness values.
3. **Geometry is exact and sparse.** Every occupied cell is explicit through
   deterministic runs. Air is represented by its absence.
4. **Coordinates are integer cell coordinates.** Geometry never depends on a
   floating-point voxel centre or an implicit image origin.
5. **Animation is opt-in.** A static asset does not pay for an animation graph.
   Transform clips are supported in v1; animated voxel topology and skeletal
   skinning are not.
6. **Readers fail clearly.** Unknown required features, unknown cell-size
   codes, malformed ordering, resource-limit breaches, and unmapped material
   keys reject the import. An importer must not silently substitute air or
   change geometry.
7. **Writers are deterministic.** Given the same logical asset, a canonical
   writer emits identical uncompressed chunk payloads and the same asset hash.

## 3. Coordinate, colour, and unit conventions

SPVX coordinates use Spall's right-handed cell axes with **+Y up**. A cell
`(x, y, z)` identifies the unit cell spanning that integer grid location in
its owning part's local space. Coordinates are signed `i32`; negative local
coordinates are valid.

`cell_size_code` is one of the engine's stable codes:

| Code | Cell edge | Intended use |
| ---: | ---: | --- |
| `0` | 0.25 m | Terrain-scale and standard construction assets |
| `1` | 0.0625 m | Detailed local assets |

A single asset has one cell-size code. A part never mixes cell sizes. Any
resample is an explicit import/export conversion, not an implicit load-time
operation.

Pivots and animated translations use integer **subcell units**: `256` units
equal one cell. This matches Spall's existing fixed-point brush granularity.
Palette tint channels are 8-bit **sRGB** values; importing converts them to
linear colour for rendering. Tint is authored display data only until the
runtime's per-cell tint path exists.

## 4. Container

All multibyte integers are little-endian. There is no native-endian field,
pointer, Rust layout, or platform-sized integer in the format.

The file starts with this 12-byte header:

| Offset | Field | Meaning |
| ---: | --- | --- |
| 0 | `u8[4] magic` | ASCII `SPVX` |
| 4 | `u16 major` | `1` for this specification |
| 6 | `u16 minor` | `1` for this specification |
| 8 | `u32 flags` | Must be zero in v1 |

A reader rejects a newer major version. It may accept a newer minor version
only if every advertised required feature is known.

The header is followed by a sequence of chunks. Each chunk has this 16-byte
header:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `u8[4]` | ASCII FourCC |
| `codec` | `u8` | `0` = raw; `1` = zstd |
| `reserved` | `u8[3]` | Must be zero |
| `stored_len` | `u32` | Bytes physically present after this header |
| `raw_len` | `u32` | Bytes after decoding; equal to `stored_len` for raw |

For `codec = 1`, the stored bytes are a zstd frame that must decode to exactly
`raw_len` bytes. A reader checks both lengths before allocating or
decompressing. Chunks cannot nest.

Unknown chunks may be skipped only when all of their advertised features are
optional. A canonical v1 writer orders known chunks as `META`, `MTRL`, `PALT`,
`PART`, `VOXL`, `STRC`, `ANIM`, then optional extension chunks in bytewise
FourCC order, and finally `HASH`.

## 5. Required and optional chunks

| Chunk | Cardinality | Purpose |
| --- | --- | --- |
| `META` | exactly one, first | Identity, name, cell size, pivot, and feature bits |
| `MTRL` | exactly one | Portable material-key table |
| `PALT` | zero or one | sRGB tint palette |
| `PART` | zero or one | Named part hierarchy and rest transforms |
| `VOXL` | exactly one | Sparse voxel runs |
| `STRC` | zero or one | Symbolic part toughness and inter-part joint profiles |
| `ANIM` | zero or one | Named transform-animation clips |
| `HASH` | exactly one, final chunk | Content integrity digest |

An empty asset is valid: it has an empty `MTRL` table and zero `VOXL` runs.
`HASH` remains required so an empty asset has an unambiguous identity.

### 5.1 `META`

`META` is raw in v1 and has the following payload:

```text
asset_uuid[16]             // opaque, non-zero UUID bytes
cell_size_code: u8         // section 3
axis_convention: u8        // 0 = Spall right-handed, +Y-up cell axes
pivot_subcells: i32[3]    // 1/256 cell, may be outside the occupied bounds
name_len: u16
name_utf8: u8[name_len]   // 1..=255 bytes
required_features: u64
optional_features: u64
tag_count: u16
tags: tag_count × { key_len: u8, key_utf8, value_len: u16, value_utf8 }
```

Tags are non-authoritative UTF-8 metadata. Keys are ASCII lowercase with
`-`, `_`, and `.` allowed; they are unique and bytewise sorted. Values are
valid UTF-8. A feature that changes how geometry, material identity, or
animation must be interpreted sets a required bit; cosmetic or ignorable data
sets an optional bit.

Defined required-feature bits:

| Bit | Name | Set when |
| ---: | --- | --- |
| `0` | `STRUCTURAL_PROFILES` | `STRC` is present or a part has a non-zero structural profile slot |

A 1.0 reader therefore rejects an asset with structural profiles instead of
misreading the extended `PART` record. A v1.1 writer sets no optional bits and
sets required bit `0` exactly when structural-profile data is present.

The UUID identifies the portable asset. The editor imports it into a
project-local `AssetId`; the two identities are intentionally separate.

### 5.2 `MTRL`

`MTRL` contains the material symbols used by `VOXL`:

```text
count: u16
entries: count × { key_len: u8, key_utf8 }
```

`count` is at most 4,096. Keys are non-empty, valid UTF-8, unique, and
bytewise sorted. A voxel run references its one-based table position; zero is
invalid because air is absence, not a material.

The importer resolves every key against a project-supplied material mapping and
then the validated Spall material manifest. The default policy is **reject on
an unmapped key**. An import UI or command may offer an explicit mapping or
material-creation workflow, but it must record that choice and must not guess
from an RGB value.

### 5.3 `PALT`

`PALT` stores display tints:

```text
count: u16
entries: count × { red_srgb: u8, green_srgb: u8, blue_srgb: u8 }
```

`count` is at most 4,096. Tint references are one-based; zero means “use the
mapped material's ordinary appearance.” Alpha is intentionally absent in v1:
opacity and collision semantics belong to a Spall material, not a palette
entry.

### 5.4 `PART`

`PART` is optional. If absent, the asset has the implicit root part `1`, named
`"root"`, with an identity rest transform. If present, it contains that root
and any named child parts:

```text
count: u16
parts: count × {
  part_id: u32, parent_id: u32, name_len: u8, name_utf8,
  rest_translation_subcells: i32[3],
  rest_rotation: i16[4],      // x, y, z, w; normalized, non-zero
  rest_scale_1024: u16[3],    // 1024 = 1.0; no component may be zero
  structural_profile_slot: u16 // zero = material baseline; otherwise STRC profile
}
```

Part IDs are non-zero and unique. `parent_id = 0` is allowed only for root
part `1`; all other parents must precede their children. Names are non-empty
and unique among siblings. Part records are canonical in parent-before-child,
then ascending `part_id` order.

Parts describe an authored assembly. They do not create engine entities or
physics bodies. Until a game feature gives a particular part collision or
destruction behavior, an importer treats the assembly as authored geometry and
does not infer gameplay semantics from the hierarchy.

### 5.5 `VOXL`

`VOXL` is raw or zstd-compressed and contains a canonical sparse run stream:

```text
encoding: u8                 // 0 = +X cell runs; other values are unsupported
run_count: u32
runs: run_count × {
  part_id: u32,
  start_x: i32, y: i32, z: i32,
  length_x: u32,             // greater than zero
  material_slot: u16,        // one-based MTRL index
  tint_slot: u16             // zero or one-based PALT index
}
```

Each run fills `length_x` consecutive cells from `start_x` through
`start_x + length_x - 1` at its fixed `y` and `z`. The endpoint must fit in
`i32`. Runs are sorted by `(part_id, z, y, start_x)`, must not overlap or
touch when their part/material/tint are equal, and must reference known part,
material, and tint slots. Writers merge adjacent compatible runs.

This stream is authoritative asset topology. Importers may partition it into
Spall's internal 32³ bricks, but that partitioning is not a source-format
property and must not change cells or material assignments.

### 5.6 `STRC`

`STRC` is optional structural intent for destructible assemblies. It lets an
artist say that a part is a steel frame, wooden board, or glass pane and that
two parts meet through a weld, nail, glue, or loose connection. It does **not**
serialize a second physics model, arbitrary strength numbers, or runtime damage
state.

```text
part_profile_count: u16
part_profiles: count × { key_len: u8, key_utf8 }
joint_profile_count: u16
joint_profiles: count × { key_len: u8, key_utf8 }
joint_count: u32
joints: count × {
  lower_part_id: u32, higher_part_id: u32,
  joint_profile_slot: u16
}
```

Part-profile and joint-profile keys are non-empty, unique valid UTF-8 strings,
bytewise sorted within their own table, and portable names such as
`"metal.frame"`, `"wood.plank"`, `"joint.welded"`, or `"joint.glued"`.
The `structural_profile_slot` in `PART` is one-based into `part_profiles`; zero
means that every voxel in that part uses its mapped material's unmodified
baseline. A joint profile slot is one-based into `joint_profiles`.

A joint record is canonical only when `lower_part_id < higher_part_id`; no pair
may occur twice. Both parts must exist and, for structural activation, must
share at least one face-adjacent cell in the asset's static, grid-aligned rest
configuration. A stored joint that has no such interface is retained as
authored metadata but has no destructibility effect until a future feature
defines one.

At import, a game resolves the two profile tables through its own structural
catalog. A part profile may supply a bounded rational multiplier for tool
hardness and for **internal** same-part bond capacity; a joint profile may
supply a bounded rational multiplier for **interface** bond capacity. The
material manifest still supplies density, collision/opacity flags, baseline
hardness, and baseline bond strength. In particular, a profile never changes a
voxel's material identity, mass, or rendering material.

For an activated joint, the current material-based interface capacity is first
computed from the two voxel materials (the weaker material governs), then the
resolved joint multiplier is applied using checked integer arithmetic. This
keeps SPVX compatible with Spall's fixed-integer structural algorithm. A
project with no mapping for an assigned profile rejects structural activation
with an actionable error; editor-only import may preserve it as inactive data.

`STRC` must not contain `DamageState`, broken bonds, current health, repair
state, runtime entity IDs, or body state. Those are authoritative mutable world
data and must survive splitting, replication, and save/recovery through the
engine's normal structural layers.

### 5.7 `ANIM`

`ANIM` is optional and records only part-transform animation. It is the v1
answer for rotating doors, mechanical assemblies, articulated voxel props, and
similar authored motion:

```text
clip_count: u16
clips: clip_count × {
  name_len: u8, name_utf8,
  duration_ticks: u32,
  loop_mode: u8,             // 0 once, 1 loop, 2 ping-pong
  track_count: u16,
  tracks: track_count × {
    part_id: u32,
    key_count: u16,
    keys: key_count × {
      tick: u32,
      interpolation: u8,     // 0 step, 1 linear, 2 spherical rotation
      fields: u8,            // bit 0 translation, 1 rotation, 2 scale, 3 visibility
      values for fields, in bit order
    }
  },
  event_count: u16,
  events: event_count × { tick: u32, name_len: u8, name_utf8 }
}
```

Animation time is 60 ticks per second. Keys in a track have strictly
increasing ticks in `0..=duration_ticks`; a key with no fields is invalid.
Translation, rotation, and scale use exactly the `PART` encodings. Translation
and scale interpolate linearly; rotation interpolation uses the shortest-path
normalized spherical interpolation. Visibility is a `u8` value of `0` or `1`
and always steps. Clip names are unique, tracks are sorted by `part_id`, and
events are sorted by `(tick, name)`.

Events are named signals, not scripts or code. A game may map a known event to
behavior; unknown event names are preserved without execution.

**Not in v1:** per-frame voxel add/remove/material deltas, morphing, skeletal
weights, arbitrary vertex animation, and client-authoritative collision
changes. Those require an explicit later extension because animated topology
that affects collision or destruction must be server-authoritative and be
replicated as validated topology changes, not replayed independently by each
client. Scale is preserved as authored animation data, but an engine instance
must bake or reject non-unit scale for collision until that behavior is
separately specified.

### 5.8 `HASH`

`HASH` is raw and has a 32-byte BLAKE3 digest. It covers the logical decoded
file payload:

```text
BLAKE3("spall.asset.v1" ||
       for every non-HASH chunk in file order:
         chunk_id || raw_len_as_u32_le || decoded_raw_payload)
```

The header, compression choice, and chunk headers are excluded. This lets a
writer recompress an asset without changing its logical identity. A reader
verifies the digest after decoding every preceding chunk. It rejects a `HASH`
chunk that is missing, duplicated, not the final chunk, or does not match.

## 6. Required validation limits

These limits apply before derived meshes, brick conversion, or runtime objects
are created:

| Limit | v1 maximum |
| --- | ---: |
| Whole file | 256 MiB stored bytes |
| Chunks | 64 |
| One decoded chunk | 128 MiB |
| Total decoded chunk bytes | 512 MiB |
| Material entries / tint entries | 4,096 each |
| Part profiles / joint profiles | 4,096 each |
| Parts | 1,024 |
| Joint records | 65,535 |
| Voxel runs | 16,000,000 |
| Expanded occupied cells | 64,000,000 |
| Clips / tracks per clip / keys per track | 256 / 1,024 / 65,535 |
| UTF-8 name | 255 bytes |
| Tag or event name | 255 bytes |

An application may impose smaller project-level limits, but it may not accept
an invalid file by truncating a run, silently dropping a part, or converting an
unknown material to air.

## 7. Import, runtime, and export behavior

### Import into Spall

1. Read and validate the container, chunks, hash, limits, and canonical run
   rules before creating an editor asset.
2. Resolve `MTRL` keys using an explicit project mapping and validate every
   resulting material against the active manifest.
3. Convert tints from sRGB to linear colour while retaining the original sRGB
   palette for lossless SPVX re-export.
4. Preserve `STRC` profiles and joints as authored data. To activate them in a
   destructible runtime instance, resolve every assigned profile through the
   project's structural catalog, retain each voxel's source part identity as an
   authoritative structural layer, and include resolved interface bonds in the
   same revision/hash/checkpoint/replication path as other structural data.
5. Copy exact cells into the editor asset and partition into engine bricks only
   in derived/runtime storage.
6. Preserve part and animation data even where the current runtime has no
   executor for it. Do not advertise per-cell tint or animation as rendered or
   simulated until those paths are implemented.

Instantiating a SPVX asset into a scene is a separate editor operation. The
scene stores its own stable local asset reference and transform. A runtime that
uses an animated part for gameplay collision/destruction must define server
authority, motion replication, and topology behavior before enabling that
feature.

A structural profile does not bypass the existing destruction model. Tool
damage still uses the resolved voxel hardness; support failure still uses the
deterministic support forest and bonds; a broken bond remains authoritative
`DamageState`. The extra part/joint identity exists so a split, late join, or
restart cannot turn a glued seam into an ordinary material boundary.

### Export from Spall

The exporter emits the canonical ordering and recomputes `HASH`. It converts
project material IDs back through an explicit export mapping to portable keys.
It must reject an asset whose material has no requested portable key rather
than inventing one. A project may export an editor-only empty asset, but the
result still requires a valid `META`, `MTRL`, `VOXL`, and `HASH` set.

The current editor RON asset format can be migrated by assigning a portable
UUID, choosing an explicit cell-size code and pivot, and supplying a material
mapping. Its project-local `AssetId` is not exported as the SPVX UUID.

## 8. MagicaVoxel `.vox` bridge

`.vox` is a supported interoperability bridge, not Spall's native format. The
base MagicaVoxel format has `SIZE` and `XYZI` model chunks plus a 256-entry
`RGBA` palette; each `XYZI` record carries one-byte `x`, `y`, `z`, and palette
index fields. The base format is therefore useful for compact voxel art, but it
does not express Spall's portable material identity, signed local coordinates,
unbounded asset extents, or authoritative destruction semantics.

### `.vox` import

- Support base `SIZE`, `XYZI`, and `RGBA` models. Scene-node, layer, and
  material extensions may be imported when implemented, but unsupported chunks
  are listed in the import report rather than silently claimed as preserved.
- Map the source Z-up gravity axis to Spall +Y up with
  `spall_cell = (vox_x, vox_z, vox_y)`. For a scene-node transform, use the
  corresponding basis conversion `M × transform × M⁻¹`.
- Convert each palette colour to an SPVX `PALT` tint. The user or import
  profile explicitly maps each palette index to a Spall material key.
- Import one selected `.vox` model as one SPVX asset by default. A future scene
  import may create scene instances and SPVX parts from a multi-model file.

### `.vox` export

The default exporter writes only a static asset (or a deliberately flattened
selected part). It rejects, with a precise report, any export that cannot be
represented without data loss: an animated hierarchy, non-unit part transform,
extent outside the selected `.vox` profile's coordinate range, more than 255
usable palette colours, unmapped material keys, or tint/material combinations
that need palette quantization. A caller may request a documented lossy
conversion; it must name the conversion in the output report.

The primary reference for the base `.vox` chunk and palette layout is
[MagicaVoxel's published format note](https://github.com/ephtracy/voxel-model/blob/master/MagicaVoxel-file-format-vox.txt).
Vengi is a useful future interoperability reference for a chunked voxel scene
graph with palettes and animation, but SPVX deliberately begins with the
smaller reusable-asset scope:
[Vengi format specification](https://vengi-voxel.github.io/vengi/FormatSpec/).

## 9. Compatibility and test fixtures

Every SPVX reader/writer implementation must have fixtures for:

- an empty asset; negative coordinates; a single run; adjacent compatible runs;
  and a 32³ brick-boundary-crossing asset;
- multiple portable material keys and an unmapped-key rejection;
- sRGB tint preservation and linear-colour conversion;
- a part hierarchy and a looped transform clip with event markers;
- material-baseline, part-profile, and joint-profile resolution for a mixed
  metal/wood assembly, including a weak glued seam and a strong welded seam;
- duplicate/unknown profile keys, invalid slots, duplicate/reversed joint
  pairs, unmapped activation profiles, and an inactive joint without a
  face-adjacent rest interface;
- malformed lengths, zstd output-limit exhaustion, invalid UTF-8, duplicate
  IDs, bad parent order, overlapping/unsorted runs, bad run endpoints, and a
  mismatched hash;
- `.vox` Z-up conversion, a custom palette, coordinate/palette limit rejection,
  and an explicit lossy-export report.

Round-trip guarantees are intentionally scoped:

- SPVX → editor → SPVX preserves cells, portable material keys, sRGB tints,
  cell size, pivot, part graph, structural profiles/joints, animation data,
  tags, and logical `HASH` when the project mapping is unchanged.
- SPVX → runtime preserves only features that the selected runtime path
  implements. Unsupported authored data is retained by the asset/editor path;
  it is not silently transformed into gameplay state.
- SPVX ↔ `.vox` is not generally lossless. Every rejected or lossy field is
  reported explicitly.

## 10. Deferred extensions

Future work may assign optional/required feature bits and new chunks for:

- voxel-frame delta clips (`VDEL`) with an explicit server-authority model;
- skeletal voxel rigs/weights;
- named sockets and additional game-specific collision/destruction annotations;
- texture/material-detail metadata that remains separate from simulation
  material identity;
- scene-level documents and instances; and
- a streaming/package form for very large asset collections.

None of these extensions may reinterpret a v1 material key, cell coordinate,
or static `VOXL` run.
