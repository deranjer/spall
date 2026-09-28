# ENG-94 (R1) — Game window on the shared direct-light pipeline

Rendering programme R1–R6 (ENG-94..99), started 2026-09-26. This report is the
audit that scoped the programme and the evidence for R1, the first ticket. It
does not claim any G2 gate result: T15/G2 acceptance is unchanged.

## 1. Audit: what actually ran where (2026-09-26)

| Feature | Implemented in | Used by the interactive game before R1? |
| --- | --- | --- |
| Cascaded sun shadows, PBR direct light, ACES tone map (T12) | `spall_render::pipeline` | **No.** Captures and the editor viewport only. |
| Manifest material render fields (albedo/roughness/metalness/emissive) | `spall_core::MaterialManifest` | **No.** The client ignored them and used a hard-coded id-to-colour palette plus per-voxel brightness jitter. |
| 128^3 @ 0.5 m indirect cache, temporal, dirty updates (T13/T14) | `spall_render::indirect`, `capture` | **No.** Capture-only. The editor viewport passes no lighting volume. |
| Environments (Studio/Daylight/Overcast/Sunset/Night) | `spall_render::environment` (2026-09-26) | Lighting terms only, in a separate hand-copied shader. |
| Authored per-cell tint | SPVX/editor only | **No, and nowhere else.** Runtime cells are `MaterialId` (u16). `sandbox::editor_scene` drops tints on import. |
| Terrain/body upload | `spall_client::window` | Whole terrain instance buffer re-created and re-uploaded **every frame**. |

Contract findings:

- `docs/architecture.md` says terrain and bodies render as greedy meshes; the
  client draws instanced cubes. R1 keeps instanced cubes (measured below) and
  leaves the mesh/instance decision to R3 with cost evidence.
- `docs/lighting-decision.md` still cites `wgpu 24.0.5`; the workspace is on
  wgpu 30. The frozen T13 cache surface is unchanged by R1; the stamp is
  historical and any later measurement must be re-taken on the current stack.
- The manifest contract says albedo is **linear**. The sandbox manifest values
  (for example grass `[0.25, 0.55, 0.2]`) read as sRGB-authored, so a correct
  linear pipeline renders them pastel. Correcting them changes the canonical
  manifest hash (handshake and persisted worlds), so it is an explicit decision
  under R2, not a silent edit here.

## 2. What R1 changed

- `spall_render::instances`: `CubeInstance` (48 B: offset, material, size,
  rotation) and a shared unit cube; `InstanceSet` is a resident, growable GPU
  buffer that reuses its allocation while data fits.
- `opaque.wgsl`/`shadow.wgsl` gain `vs_cube`; `fs_main` is shared with the mesh
  path. `ScenePipeline` builds `opaque_cube`/`shadow_cube` and can tone-map
  straight to a host swapchain format (`new_for_output`, sRGB only).
- Direct pass now adds emission (`base_color * emissive`), the frozen T13
  material encoding, so emissive semantics are shared. Emissive panels in
  *Shaded* captures are therefore brighter than in pre-R1 G2 images.
  `IndirectOnly` is unchanged.
- `spall_render::materials_from_manifest`: manifest -> material table indexed by
  `MaterialId`. Undefined ids are loud magenta. Emission is fitted onto the
  base colour (exact for surface-tinted emitters; black-albedo emitters take the
  emission colour as base).
- `spall_render::GameRenderer`: shadows -> HDR -> tone map into a host surface
  view; terrain, body and overlay instance sets; non-blocking timestamp timing.
- `spall_client::window`: private shader, palette, jitter and per-frame terrain
  upload removed. Terrain uploads only when a rebuild lands or `F1` toggles it;
  bodies re-pose into a reused buffer; the capsule overlay is lit but
  non-shadow-casting. `F4` cycles albedo/normals/depth/cascades/roughness.
  Body cells now use the volume's own cell size (they were always drawn at
  0.25 m regardless).
- Environment defaults preserved: Studio for legacy scenes, Daylight for the
  runtime, `--environment` overrides.

Known differences from the editor viewport (deliberate, recorded): the cube
path has no baked AO (`ao = 1`); no per-voxel jitter; no lighting volume.

## 3. Evidence (measured on this machine)

Machine: NVIDIA GeForce RTX 4080 SUPER, D3D12 (wgpu 30), Windows 11.

**Editor/game parity** — `cargo test -p spall_render --test game_renderer_gpu
the_game_and_editor_paths -- --ignored`. The same slab + pillar + rotated
cube, camera and environment through `GameRenderer` and `ViewportRenderer`:
mean |diff| 0.000/255 and 0.000 % of channels differing by >12, for all five
environments. (The first run of this test exposed a real bug: sequential
`vertex_attr_array!` offsets misplaced `rotation` after the padding, leaving
bodies unrotated. Layout is now explicit and unit-tested against
`offset_of!`.) Images: `.local/runs/eng-94-parity/`.

**Shadows / materials** — same test file: terrain pillar and a body each darken
the slab where the sun is blocked (`terrain_and_moving_bodies_cast_shadows`);
an emissive material outshines the same geometry in stone, and the albedo debug
view encodes linear albedo to sRGB once
(`emissive_materials_glow_and_debug_views_show_albedo`). All 3 pass.

**Regression** — `capture_gpu` (10 tests, including the Vulkan pipeline-compile
probe) and `viewport_gpu` pass; `spall_render` lib 46+, `spall_client` lib 40.

**Frame cost, headless, release, 1920x1080, 58 564 terrain + 200 body cubes,
Daylight, 150 frames after 30 warm-up** (`game_renderer_frame_cost_at_forest_scale`,
`.local/runs/eng-94-perf/summary.json`):

| Pass | p50 | p95 |
| --- | ---: | ---: |
| shadow (4 cascades) | 1.47 ms | 2.03 ms |
| opaque | 0.39 ms | 0.67 ms |
| tone map | 0.009 ms | 0.010 ms |
| CPU record+submit | 0.20 ms | 0.25 ms |

Instance memory 4.0 MiB. Against the provisional G2 12 ms GPU / 16.7 ms client
targets this is a headline for *direct light only*; indirect (R4) is not
included, and this is one adapter, one scene.

**Live window** — `cargo xtask play --editor-scene
fixtures/terrain-trees-forest/scenes/main.ron --environment sunset` (debug
build, 1296x759): 60 fps, frame 16.6 ms avg / ~17-18 ms max, scene GPU
~3.3 ms (shadow 2.7 / opaque 0.6), terrain buffer upload 0.0 ms between
rebuilds (previously ~1.2 ms, every frame; confirmed in the 2026-09-26 Daylight
run log `.local/runs/eng-94-live/play-daylight.log`). Screenshot: `.local/runs/eng-94-live/sunset.png` — orange sky,
long low-sun tree shadows on the ground and on other trees, HUD intact.
Inspected by eye; no moving footage was recorded.

## 4. Not done / unrun

- No live editor-versus-game capture of the same camera (renderer-level parity
  is proven; window chrome, HUD and surface format are not compared).
- Forest greens are **not** retained: no tint representation exists (R2), and
  manifest palette values are pastel under a correct linear pipeline.
- No skylight visibility, enclosed-room darkness, or indirect light in the game
  (R3/R4). The hemispheric ambient is still unconditional.
- Non-sRGB surface formats are not supported by the tone-map pass (it asserts).
  The window prefers an sRGB format; a surface offering none would fail at
  startup rather than be double- or under-encoded.
- Vulkan and integrated/other-vendor GPUs unmeasured for the game path
  (Vulkan pipeline creation is covered by the existing probe test only).
- Debug views for shadow visibility, direct-only and tint are R6.

## 5. Next

R2 (ENG-95) tint/appearance representation and the manifest-palette decision;
R3 (ENG-96) visibility-aware skylight and shadow technique; R5 (ENG-98) is
independent and can start any time.
