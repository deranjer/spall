# ENG-97 (R4, first pass) — One diffuse bounce in the interactive runtime

Third ticket of the rendering programme (R1 = ENG-94, R3 = ENG-96). This pass
puts a lit, one-bounce indirect term into the game window and the editor
viewport for **terrain**, and evaluates the frozen 0.5 m cache against thin
geometry. Moving bodies, dirty-region and camera-scroll updates, temporal
accumulation and streamed boundaries are R4b (ENG-101); nothing here claims a
G2 gate result.

## 1. Reconciliation with the frozen T13/T14 contract

`docs/lighting-decision.md` freezes the cache *geometry* (128³ cells, 0.5 m,
camera-local, derived), the CPU occupancy format (`u32` material per cell,
`0` = air), the derived-only boundary, and the T13 pass order for the T13/T14
capture prototype. This pass keeps all of that unchanged:

- The occupancy is exactly the frozen layout, shared with R3's sky visibility
  (one 8 MiB upload feeds both).
- The T13/T14 trace/denoise/temporal passes and their capture evidence are
  untouched and still used by `sandbox-capture`.
- The runtime bounce is a **new pass** (`bounce.wgsl`) rather than a change to
  the T13 trace, because the T13 trace's sources are fixed-brightness
  approximations (`albedo × (emissive + 0.12·up)`, plus a fixed sky colour for
  escaped rays), which the brief rules out, and its radiance format is a single
  omnidirectional `vec4` per cell, which cannot keep a floor from lighting
  itself (§3). Its radiance buffer is therefore a *new* derived buffer, not the
  frozen `array<vec4<f32>>` pair. Consolidating the two implementations is left
  to R6 with the G2 fixtures.

## 2. What the pass does

For every air cell within two cells of a surface, 42 fixed rays (six axes, twelve
edge diagonals, twenty-four shallow rays 19° off a face plane) are walked through
the occupancy with an exact 3D DDA. The first occupied cell a ray enters is a
bounce source, and its outgoing radiance is what that surface is doing *now*:

`L = albedo × ( sun × N·L × sun-visibility + sky-visibility × sky radiance ) + albedo × emissive`

- Sun visibility is a shadow ray through the same occupancy; sky visibility is
  R3's per-face sky-visibility volume; emission is the material's. No fixed
  brightness stands in for any of them.
- **No double counting.** Rays that reach the sky or leave the cache contribute
  nothing: skylight is applied by the opaque pass from sky visibility, so adding
  it here would count it twice. Open ground under a bright sky has nothing to
  bounce (measured: indirect-only peak 0/255).
- **Unknown space** is opaque and emits nothing (`UNKNOWN_CELL`), never sky.
- Each cell stores six face radiances (a directional ambient cube: the
  cosine-weighted average over the rays in that face's hemisphere, misses
  counting zero), packed as shared-exponent RGB (4 bytes per face, 48 MiB for the
  cache). A receiver reads the three faces its normal points to, so a surface is
  never lit by "light from behind" itself.
- Shading reflects `albedo × bounce`; **furnace calibration** (a sealed room
  whose every surface emits L = 0.5 with albedo 0.5) gives 0.2502 against the
  expected 0.25.

Terrain rebuilds send a new occupancy only when it differs from the last one,
and the cache stays anchored until the player drifts 8 m from its centre.
Before this, a stationary player triggered a full lighting recompute every
~1.4 s (each terrain recheck): a live run showed 28–32 ms frame spikes on
every recompute at idle GPU clocks; with the change a stationary player shows
one recompute and a steady 17 ms frame maximum.

## 3. Findings that shaped the design

1. **A first version with one omnidirectional radiance per cell lit floors with
   themselves.** With 14 symmetric rays, 5 of 14 pointed at the ground, so open
   ground in the indirect-only view was 153/255 (a floor bouncing its own lit
   radiance back onto itself). Moving to per-face storage fixed it (0/255) and
   the furnace still calibrates.
2. **14 rays were too few.** The first directional version put zero weight on
   light arriving below 35° elevation (horizontal rays have no cosine weight; the
   next ray was at 35°), so a lamp 1.5 m away on the same floor gave 0.0. The 42
   ray set adds shallow rays; the same lamp now lights the floor (0.070 at 2 m,
   0 at 9 m). **Remaining limit:** light arriving below ~19° elevation
   (a small emitter several metres away on the same plane) is still poorly
   sampled; more rays or importance sampling is the fix, recorded for R4b.
3. **The opaque pass's storage-buffer budget is 4 on downlevel devices.** The
   bounce bindings first made 5; the sky occupancy buffer was dropped from the
   display bind group (an air marker lives in an unused byte of the visibility
   words) rather than raising limits.

## 4. Evaluation of the 0.5 m cache against thin geometry (measured)

`bounce_gpu` (emitter on one side of a wall, receiver on the other, 1 m behind):

| Wall | Bounce reaching the receiver (of the unobstructed value 0.0704) |
| --- | --- |
| none | 100 % |
| one voxel (0.25 m, half a cell) | **0.0 %** |
| one cell (0.5 m) | **0.0 %** |

No leakage through walls at supported sizes, because a cell is occupied if *any*
voxel in it is (over-occlusion, never leakage). The cost is the opposite error:
narrow openings close. A wall with a gap between emitter and receiver, gaps *not*
aligned to the 0.5 m grid:

| Gap | Bounce transmitted |
| --- | --- |
| 0.25 m | 0.0 % |
| 0.50 m | 0.0 % |
| 0.75 m | 84.6 % (alignment dependent) |
| 1.00 m | 84.6 % |
| 1.50 m | 100 % |

**Documented supported sizes:** solid walls of any thickness ≥ one voxel block
completely; an opening is guaranteed to pass light only when it is ≥ 1 m
(two cells) wide, may pass it from 0.75 m depending on grid alignment, and is
closed at ≤ 0.5 m. A one-voxel window or slit does not light a room by bounce or
skylight (direct sun through it is still handled by the shadow maps).

**Decision:** no change to the cache resolution, packing (occupancy), traversal or
caching is proposed by this pass. The 0.5 m cell is adequate for walls, and the
narrow-opening error is on the safe (dark) side and documented. Any change would
need new colored-room and leakage evidence per the freeze list.

## 5. Evidence (measured on this machine)

RTX 4080 SUPER, D3D12, wgpu 30. `cargo test -p spall_render --test bounce_gpu --
--ignored --nocapture`; images in `.local/runs/eng-97-bounce/`, live in
`.local/runs/eng-97-live/`.

- **Colour bleed from actual light.** Floor beside the sunlit face of a red wall,
  indirect-only: (0.212, 0.013, 0.012) — red; 9 m away 0.036; beside the wall's
  *shadowed* face 0.000; with the sun off the same spot falls to 0.025 (≥ 8×
  lower). In the shaded render the bounce raises the floor's red channel by 0.039
  and its blue by 0.000 against the bounce-off frame.
- **Emissive.** An emissive orange cube lights the floor 2 m away
  (0.070, 0.032, 0.007 — orange), 0 at 9 m, with no sun and no sky; the same cube
  in a non-emissive material lights nothing.
- **No double counting:** open ground, indirect-only peak 0/255.
- **Thin walls / gaps:** §4.
- **Editor and game paths agree** with the bounce enabled: mean difference
  0.000/255, 0.000 % of channels above 12.
- Existing acceptance unchanged: `capture_gpu` 10/10 (incl. the Vulkan
  pipeline-compile probe with the new shaders), `sky_visibility_gpu` 6/6,
  `shadow_technique_gpu`, `game_renderer_gpu`, `viewport_gpu`; workspace tests
  pass.
- **Cost** (release, headless, 1080p, ~66 k cubes + 200 bodies,
  `.local/runs/eng-96-perf/summary.json`): opaque pass p50 0.50 ms (legacy ambient)
  → 0.70 ms (sky visibility + bounce); sky-visibility recompute 1.06 ms;
  **bounce recompute 2.25 ms**; cache memory **72 MiB** (8 occupancy + 16 sky
  visibility + 48 bounce). Recomputes run only when the occupancy changes.
- **Live, game window** (`fixtures/lighting-room`, debug build): the shadow-side
  wall of the roofless house now carries a green tint from the sunlit lawn where
  it was flat navy (`.local/runs/eng-97-live/20-bounce-on.png` vs
  `21-bounce-off.png`); `F6` toggles it. At the idle GPU clocks of a vsync-bound
  window the recompute took 8–11 ms (sky) and 15–33 ms (bounce) device time,
  ~10× the headless figure, once per terrain change.

## 6. Not done / unrun

- **Moving bodies** and detached debris neither block skylight nor bounce light,
  and are not lit as bounce sources (R4b, with old/new-bounds updates).
- **Dirty-region updates.** Any change recomputes the whole cache (2 + 1 ms of
  GPU at full clocks, ~40 ms at idle clocks). Terrain edits reach the lighting
  after the client's terrain recheck (500 ms) plus a rebuild; not measured
  end-to-end (R6).
- **Camera scroll** re-centres the cache after 8 m of drift with a full
  recompute; no clipmap scroll.
- **Temporal accumulation / ghosting.** The pass is deterministic and
  full-recompute, so it cannot ghost, but it has no temporal filter and the
  bleed is blocky at 0.5 m (documented; no denoise).
- **Streamed boundaries:** the unknown-cell policy is implemented and unit
  tested at the occupancy builder, but no streamed-world run exercised it.
- **Low-angle light** (§3.2) and one bounce only (no interreflection).
- The sandbox manifest has no emissive materials, so the game has no emissive
  content to demonstrate; emissive is proved in the GPU test only.
- No moving footage, no Vulkan/non-NVIDIA measurement of the new passes, no
  live editor-versus-game capture.
- The capture tool and G2 fixtures still use the T13 trace plus the legacy
  ambient (which double counts) until consolidated.
