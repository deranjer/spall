# ENG-95 (R2) — Authored tint as material identity

Fourth ticket of the rendering programme. Two steps: (1) the sandbox manifest's
albedos were migrated to linear (done earlier, recorded in the Loopira log); (2)
authored per-cell tint survives edits, body splits, replication and reload by
being a *material*. Nothing here claims a G2 result.

**Status: implemented and validated on this machine; not accepted.** Section 5
records a correction pass: the first version assigned ids from table position and
claimed additive regeneration, which was wrong; and its "network" evidence was a
simulation/checkpoint test, not a client/server run. Both are fixed below.

## 1. Audit

- Runtime cells are one `MaterialId` (u16); that id is what the engine already
  replicates (`spall_protocol`), persists (`spall_store`), splits into detached
  bodies (`spall_sim`/`spall_structure`), and hashes (manifest hash in handshake
  and checkpoints). There is no per-cell colour anywhere below the editor.
- SPVX assets already carry tints: a `PALT` palette (≤ 4096 sRGB entries) and a
  per-run tint slot. The editor-scene importer parsed only the palette *count*
  and dropped every tint, so a forest tree played back as the flat base material.
- The manifest's render fields are the only appearance the renderer reads.

## 2. Decision

**A tint is a material variant.** Options weighed:

| Option | Contract change | Verdict |
| --- | --- | --- |
| Per-cell tint layer in `spall_voxel` bricks + protocol + store | New brick layer, versioned records, hashing, every split/edit path | Correct but a broad change to frozen contracts |
| **Variant materials in the manifest** | None to protocol/store; manifest gains entries | **Chosen** |
| Client-side procedural tint | Loses tint after edit/reload | Rejected |

Variants share their base material's simulation properties exactly (a wood
variant is wood to the physics, structure and drop rules) and differ only in
colour. The first set is 37 variants (grass 12, foliage 12, wood 12, dirt 1; a
median cut of the authored tints, ≤ 12 per key) at ids 100..=136, frozen
literally in `appearance_v3.rs`; the manifest holds 53 materials (15 base + 37
variants + the lamp). (An earlier draft of this report said "96 materials" and
"ids 100..195"; that was wrong.) Server and client
build the identical manifest from that table, so no handshake or protocol change
is needed. Importing an editor scene maps each tinted cell to the nearest variant
of its material (weighted sRGB distance); untinted cells keep the base.

**Manifest migration.** `MaterialManifest::superseding_extension(previous)`
(`spall_core`) accepts a manifest that keeps every previous id, name, simulation
property **and render field** and adds only higher ids. (It first also allowed
render changes, which would have let a regenerated palette recolour saved
variants silently; that is now refused, and a recolour must go through the
explicit `superseding_appearance` migration.)
`sandbox::game::manifest()` extends `manifest_v2_linear()` which supersedes
`legacy_manifest_v1()`; worlds saved under either still restore and are re-stamped
at their next checkpoint. Builds from before this change no longer match the
handshake hash.

**Game rules** key on base materials: drops and inventory use
`appearance::base_material` / `merge_variants`, so tinted wood still drops wood.

## 3. Evidence (measured on this machine)

- **Quantisation error on the forest assets** (`forest_tints_map_to_variants_within_the_measured_error`,
  Euclidean sRGB distance out of 255; every tinted cell maps to a variant, none
  falls back to its base): grass 25,600 cells mean 1.15 / worst 9.0; foliage 4,058
  cells mean 1.90 / worst 20.1; wood 434 cells mean 1.06 / worst 8.7; dirt 0.00.
  The generator prints redmean figures for the same palette (grass mean 1.01,
  foliage 3.07, wood 1.84).
- **Edit, split, save, restore** (`variants_survive_an_edit_a_body_split_and_a_save`,
  the real forest world): a cut through a tinted trunk commits; every cell that is
  still terrain keeps exactly its variant; 427 cells moved into detached bodies
  (936 bodies exist in the forest world), with 3,947 variant cells across bodies,
  all known materials; per material the cells that left terrain never outnumber
  what the bodies hold; a checkpoint restored under the current manifest returns
  the same terrain variants and the same per-material body cell counts.
- **Manifest** (`the_manifest_extends_the_linear_one_and_variants_keep_their_bases_rules`):
  predecessor hashes for v1 and v2 present; every variant's simulation properties
  equal its base's; albedo is the authored colour in linear; ids unique and ≥ 100;
  every earlier material unchanged. `spall_core` tests pin the extension rules.
- **Drops** (`wood_variants_still_drop_wood`): 20 variant + 12 base wood cells
  yield 2 logs.
- **Editor/game parity, side by side** (same scene, spawn camera, daylight):
  `.local/runs/eng-95-parity/editor-forest-daylight.png` (exact authored colours,
  `cargo run -p spall_editor --example parity_capture`) against
  `game-a.png` (live window). The lawn patches, foliage tones, trunk colour and
  shadows read the same. Inspected by eye; no pixel metric across the two
  because the cameras differ by a few pixels of mouse look.
- Existing acceptance: workspace tests pass (the forest still fits the exact
  collider budget with the 53-material manifest).

## 4. Limits / not done

- **Quantised, not exact:** the game shows the nearest of ≤ 12 colours per key,
  not the exact authored colour (mean error ~1–2/255 on the forest). More
  variants per key cost only manifest entries (4096 max); the generator's
  `VARIANTS_PER_KEY` sets it.
- **Palette growth is append-only.** A new asset with colours far from every
  existing variant of its key gets new rows at ids from 256 when the generator is
  re-run (`cargo run -p spall_editor --example appearance_palette`); existing
  rows are never renumbered, renamed or recoloured, and a candidate within
  redmean distance 6 of an existing variant reuses it. The generator's output is
  `appearance_extensions.rs`; the shipped one is empty, because regenerating from
  today's assets adds nothing (every candidate maps to a frozen variant).
  Recolouring or remapping an existing variant is not an extension: it needs a
  new versioned manifest migration in `sandbox::game`.
- Only keys the sandbox maps get variants (grass, foliage, wood, stone,
  sandstone, dirt). `stone.granite` and `sandstone` have no authored tints in the
  fixtures, so they have none.
- Per-voxel tint on player-placed or generated terrain is not part of this: a
  placed block is its base material.
- The editor still previews exact colours; it does not preview the quantised
  palette.
- Moving footage and a pixel-level editor/game metric are unrun.

## 5. Correction pass: identity, migration and network evidence

**What was wrong.** (a) `appearance::variants()` assigned ids as `100 + table
position`, and the generator sorted colours, so regenerating after an asset change
could renumber or recolour existing variants while claiming an additive manifest
extension; v3 was rebuilt from that mutable table, so even "history" moved. (b)
`superseding_extension` allowed render changes. (c) Nothing exercised real client
replication or late join; the checkpoint test does not establish either.

**Now.**
- Variant identity is explicit `(id, key, name, colour)` rows. `appearance_v3.rs`
  freezes the shipped v3 rows (ids 100..=136); `appearance_extensions.rs` is the
  generator's append-only output from `EXTENSION_ID_BASE` = 256, above the lamp
  (200) and every hand-assigned id. `manifest_v3_variants()` is built from the
  frozen rows only.
- Historical manifests are pinned by content hash in
  `examples/sandbox/tests/appearance_evolution.rs` (v1, v2, v3, v4); the refactor
  was checked to leave all four hashes byte-identical to before it.
- `superseding_extension` now refuses any change to an existing material's render
  fields (`spall_core` test).
- Tests (`appearance_evolution.rs`, all pass): an extended palette keeps every
  earlier id, name, simulation and render value and lists v1–v4 as restorable
  predecessors; extension rows that reuse the lamp id, an existing variant id or a
  free id below the current highest are refused; swapping two variants' colours is
  not an extension; a world checkpointed under today's manifest restores under a
  grown palette with identical material ids on a strided sample of the forest, and
  is **refused** under a manifest that recoloured a variant. The generator's
  append/idempotence logic has unit tests (`appearance_palette` example).

**Real client/server evidence** (`examples/sandbox/tests/tint_replication.rs`,
`cargo test -p sandbox --features client --test tint_replication`, 92 s): a QUIC
server hosts the tinted forest with the game manifest; an early client cuts a
trunk; a late joiner arrives after the commit. Server, early and late client agree
on the world hash (936 detached bodies, 1,114 baseline bricks on the late join);
both replicas hold identical body cell tallies; every replicated terrain cell
keeps its authored variant (4,312 tinted terrain cells around the trunk); 440
cells moved into bodies and the bodies hold 3,947 variant cells across 18
materials, all known to the manifest.

**Bug found and fixed by that test (not tint-specific).** The first run dropped
the early client silently: the trunk cut detaches ~936 small bodies, and the
inline split encoding (chosen when each blob fits `MAX_SPLIT_BASELINE_BLOB`) came
to a 100,728-byte control record against the 65,536-byte limit. The server's
writer treated the send error as end-of-stream and the client reported `passed`
with the world one edit behind. `spall_sim::commit` now checks the re-encoded
transaction as a whole, falls back to the bulk form, and fails the commit if even
that cannot fit (the trunk cut is now 46,643 bytes). The server logs an error when
a topology send fails. Not done: the client control loop still ends without saying
why on a stream error (`Err(_) => break` in `net.rs`), so a similar drop is only
visible from the server log.

**Observation, unmeasured cause.** With the 936 bodies the paced server ran 500
ticks in about 90 s in a debug build; this is recorded for ENG-99's frame/tick
measurements, not diagnosed here.

## 6. Still open before acceptance

- The editor still previews exact colours; parity metrics with the quantised
  palette are ENG-99.
- Tint on player-placed/generated terrain; moving footage.
- Nothing here is release-build measured: the network test ran in a debug build.
