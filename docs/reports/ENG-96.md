# ENG-96 (R3) — Natural direct lighting: skylight visibility, enclosed interiors, shadow technique

Second ticket of the rendering programme (R1 = ENG-94). Scope: the *direct* half
of natural lighting in the shared renderer (game window and editor viewport):
sunlight with soft, contact-correct shadows, and skylight that is *visibility
aware* so enclosed rooms are dark. Indirect bounce is R4 (ENG-97); nothing here
claims a G2 gate result.

## 1. Baseline shadow technique (documented decision)

Cascaded shadow maps with percentage-closer soft shadows (PCSS):

| Aspect | Choice |
| --- | --- |
| Maps | 4 cascades of 2048², bounding-sphere fit per cascade, split scheme 0.65 log + 0.35 uniform, light-space texel snapping (unchanged from T12). |
| Bias | Normal-offset in world texel units, `0.75 + 1.5·(1 − N·L)` texels along the normal, plus a slope-scaled depth bias `0.5 + 1.0·(1 − N·L)` texels. Derived per cascade from the light matrix, so the same numbers work at every distance. |
| Edges | 16-tap Vogel-disk PCF (hardware bilinear compare), rotated per pixel by interleaved gradient noise. |
| Softness | **Physically motivated, not just filtered.** A 16-tap blocker search finds the average occluder depth; the filter radius is `occluder distance × tan(sun half-angle)`, so penumbrae widen with the receiver-to-occluder distance. A floor of one texel keeps edges antialiased (that floor is the only "filtered" softness). The sun size is an `Environment` parameter (`sun_angular_diameter_deg`: Studio 2°, Daylight 1°, Overcast 6°, Sunset 1.2°, Night 0.6°; physical sunlight is 0.53°, larger values are artistic and stated). |
| Contact detail | Cascade 0 texels are ~4 cm at the game's camera; measured: shadow reaches within 3 cm of an object's base, with no acne on lit faces. |
| Distance | Shadows extend to the camera far plane (300 m in the game). Cascade 3 texels are ~29 cm at that range, coarser than a 25 cm voxel; there is no cascade blending, so a seam is possible at split distances. Occluders more than 24 m above a receiver are not searched for (penumbra clamps). |
| Moving bodies | Bodies are drawn into the same four maps as terrain every frame, so they cast and receive shadows with no special case. |
| Known limits | The per-pixel rotation is fixed to the screen (a faint pattern in wide penumbrae, no temporal shimmer). No cascade blending. Contact AO is not in the game path yet (see §4). |

Exposure is held fixed across every comparison below; no result here depends on an exposure change.

## 2. Visibility-aware skylight

Before R3 the game's ambient light was a hemispheric constant applied to every
surface, so a sealed room was lit exactly like open ground. Now:

- A camera-local occupancy grid — the **frozen T13 layout** (128³ cells of
  0.5 m, one `u32` per cell, `0` = air), *not* enlarged or replaced — is built
  by the client from the replica's terrain volume and uploaded to the GPU.
  It is derived, client-local data: it never feeds collision, topology,
  replication or persistence (`spall_client::sky`).
- A compute pass (`sky_visibility.wgsl`) computes, for every air cell within two
  cells of a surface, how much sky each of the six axis directions sees:
  5 cosine-weighted rays per direction (axis + four 45° tilts), exact 3D-DDA
  traversal, up to 48 cells (24 m). A ray reaches the sky if it travels that far
  or leaves through the top of the cache; it is blocked by a solid or unknown
  cell, or by leaving through a side or the bottom.
- The opaque pass reads the three faces the surface normal points to, blends the
  8 nearest air cells (solid and unknown cells contribute nothing), and scales
  the environment's sky/ground radiance by that visibility (an ambient cube).
  Fully open, this reproduces the old hemispheric ambient exactly; buried or
  enclosed, it goes to zero. Outside the cache, or with no occupancy supplied
  (captures, the G2 fixtures), the legacy ambient is used.
- **Unknown space is not sky.** A brick that is not resident is unknown. A
  complete, non-streamed world (the interactive client with no
  `client_residency`) treats an absent brick as empty; a streamed session marks
  it `UNKNOWN_CELL`, which blocks rays instead of leaking light.
- `F5` in the game window switches this off (legacy ambient) for comparison;
  `F4` cycles debug views, including the new **shadow visibility** and **sky
  visibility** views.

Limits of the resolution (measured against the brief's thin-wall concern):

- A cell is occupied if *any* voxel in it is, so a wall of one 0.25 m voxel is
  thinner than a cell but still blocks: over-occlusion, never leakage. Tested:
  a sealed room with one-voxel walls and roof is fully dark.
- The smallest opening that lets skylight through is about one cell (0.5 m). A
  one-voxel window or slit is treated as closed to *skylight*. Direct sunlight
  through it is still handled by the shadow maps, at their own resolution.
- Moving bodies and detached debris are **not** part of the occupancy: they
  cast sun shadows but do not block skylight (R4, which owns old/new-bounds
  updates).
- The occupancy is rebuilt with each terrain rebuild, not patched. Edit-to-skylight
  latency is therefore the client's terrain-recheck interval (500 ms) plus a
  rebuild plus one frame; it was not separately measured (R6).
- Ambient occlusion supplements but does not replace this: the instanced-cube
  path carries no baked AO (`ao = 1`); the mesh path multiplies its baked AO as
  before.

## 3. Evidence (measured on this machine)

RTX 4080 SUPER, D3D12, wgpu 30, Windows 11. GPU tests are `--ignored`; run with
`cargo test -p spall_render --test <name> -- --ignored --nocapture`.
Images are under `.local/runs/eng-96-shadows/`, `eng-96-sky/`, `eng-96-live/`.

**Shadows** (`shadow_technique_gpu`)
- Penumbra width (10–90 %), same 4 m strip, 6° sun: occluder 0.6 m up → 5.0 px;
  6 m up → 49.0 px. Ratio ≈ 10×, the physical ratio of the two occluder
  distances along the light ray.
- Contact: shadow visibility 0.00 at 3 cm from the base of a resting cube, 1.00
  on open ground, 1.00 on the cube's own lit top (no acne).
- Existing acceptance still passes: `capture_gpu` 10/10 (including the Vulkan
  pipeline-compile probe with the new shader), `viewport_gpu`, R1's
  `game_renderer_gpu` (editor/game pixel parity, all 5 environments, still 0.000
  mean difference).

**Skylight** (`sky_visibility_gpu`, sun disabled so all light is skylight)

| Room (6 m, one-voxel walls) | near door | mid room | side wall | sky vis. at door |
| --- | ---: | ---: | ---: | ---: |
| closed | 0.000 | 0.000 | 0.000 | 0.000 |
| doorway 1.5 m | 0.235 | 0.000 | 0.089 | 0.467 |
| no roof | 0.586 | 0.646 | 0.167 | 0.875 |
| closed, legacy ambient (before R3) | — | 0.665 | — | — |

(Mean luminance 0–1 at fixed exposure.) Closed = fully dark; a doorway lights
the floor near it and falls off; removing the roof lights the room; the old
ambient lit the sealed room as brightly as open ground. Also: unknown space
overhead blocks skylight (0.665 → 0.000); open ground is unchanged from the
legacy ambient (within 0.02); the editor mesh path and the game cube path agree
pixel-for-pixel with skylight enabled (mean difference 0.000/255).

**Live, game window** — `cargo xtask play --editor-scene fixtures/lighting-room`
(three identical houses: sealed, doorway, roofless; new `lighting_room`
example). Outdoors the doorway house shows a black interior with a faint
skylight wash on the back wall, cast shadows on the lawn; standing inside the
sealed room is pitch black, and `F5` (legacy ambient) shows the same room
unconditionally lit. Screenshots: `.local/runs/eng-96-live/` (`30-a.png`,
`07-legacy.png`, `08-skyvis.png`). Inspected by eye; no moving footage. A person
also drove the window during some runs (the player walked into a house on its
own), which is why some captures are from inside.

**Cost, headless, release, 1920×1080**, 65 k-cube forest-scale scene + 200
bodies (`sky_visibility_cost_at_forest_scale`, `.local/runs/eng-96-perf/summary.json`):

| | shadow maps | opaque (incl. PCSS + sky) | tone map |
| --- | ---: | ---: | ---: |
| legacy ambient, p50 / p95 | 1.38 / 1.91 ms | 0.50 / 0.80 ms | 0.009 ms |
| sky visibility, p50 / p95 | 1.38 / 1.90 ms | 0.58 / 0.88 ms | 0.009 ms |

Sky-visibility recompute: 1.06 ms on the GPU, run only when the occupancy
changes; GPU memory 24 MiB (8 MiB occupancy + 16 MiB visibility). Compared with
R1's forest-scale run (opaque 0.39 ms), PCSS adds about 0.11 ms and sky lookup
about 0.08 ms. In the live debug-build window (1296×759, sharing the GPU with the
desktop) the scene took ~3–5 ms per frame. Against the provisional 12 ms GPU
target this is direct light only; indirect (R4) is not included; single adapter.

## 4. Geometry submission (decision, with measurements)

`cargo test --release -p spall_render --test geometry_paths_gpu -- --ignored`
(`.local/runs/eng-96-perf/geometry-paths.json`), same terrain, 1920×1080:

| | triangles | memory | build (whole volume) | frame wall time p50 / p95 |
| --- | ---: | ---: | ---: | ---: |
| instanced surface cubes (65,909) | 790,908 | 3.0 MiB | 3 ms | 2.47 / 3.19 ms |
| greedy mesh (16,986 quads) | 33,972 | 3.0 MiB | 154 ms | 0.30 / 0.65 ms |

Decision: **the instanced-cube path stays for this ticket** — it is already
integrated, its 2.5 ms is affordable, and swapping submission is a separate
change to the client's rebuild, per-brick lifecycle and body pipeline. But the
mesh path is ~23× fewer triangles and ~8× cheaper per frame, matches
`docs/architecture.md`, and would bring baked AO for free, so the contract
direction stands and the migration is filed as ENG-100 (R7) with these numbers.
The mesh build here is a single whole-volume pass; per-brick incremental
meshing is expected to be much cheaper and is part of that ticket.

## 5. Not done / unrun

- Indirect bounce, emissive illumination of neighbours, colour bleeding (R4).
  Enclosed rooms are therefore *black*, not merely dark, until R4 lands; that is
  the honest result of removing the unconditional fill, not an artistic choice.
  No minimum ambient was added: it would be the same fixed approximation.
- Moving bodies do not affect skylight; incremental old/new-bounds updates (R4).
- The capture tool and G2 fixtures still use the legacy ambient (plus indirect,
  which double counts) — reconciling that belongs with R4.
- Contact AO in the game (comes with the mesh path, ENG-100).
- No live editor-versus-game capture of the same camera; no moving footage; no
  measured edit-to-skylight latency; no Vulkan or non-NVIDIA measurement of the
  new passes (Vulkan pipeline creation is covered by the existing probe).
- Cascade blending and the fixed screen-space penumbra noise pattern.
