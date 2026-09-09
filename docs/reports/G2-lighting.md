# T14 dynamic lighting and temporal stability — evidence

Ticket: ENG-21 / T14. This report collects the measured evidence for T14. It is
**not** the G2 gate (ENG-22 / T15): cross-GPU quality and the p95 frame-cost
targets remain T15 work.

Reference adapter: **NVIDIA GeForce RTX 4080 SUPER, D3D12, pinned `wgpu 24.0.5`**,
debug build. All figures are single measurements from GPU timestamp queries or
mean sRGB luminance over a fixed image band, not percentiles.

## What T14 built (against the frozen T13 contract)

T13 froze the resource/pass contract in `docs/lighting-decision.md`. T14 is
purely additive to it:

1. **`LightingUpdate` / `LightingVolume::apply_update`** — a plain,
   simulation-free description of what changed (world AABBs cleared to air plus
   solid regions refilled in order). A cell is recorded dirty only when its
   material actually changes.
2. **Partial re-upload + bounded re-trace** — `take_dirty` yields the minimal
   `(cell, material)` set, coalesced into `write_buffer` runs; `set_trace_region`
   bounds the trace to the dirty cell-AABB grown by a halo. The denoise still
   runs over the whole cache.
3. **`LightingUpdate::moving_box`** — the update for a box body moving between
   two world AABBs, re-asserting the caller's static geometry so a moving body
   never erases what overlaps its old bounds.
4. **Temporal accumulation pass** (`temporal_main`, step 3b after the spatial
   denoise) — each frame's denoised estimate is blended into a persistent
   history clamped to the neighbourhood min/max of the current estimate. Cells
   inside the just-re-traced region take the current frame outright, so an edit
   is never masked by stale history.

`capture_scene` (the T13 acceptance path) is unchanged and never runs the
temporal pass — the T13 colored-room probes/leakage/cell count are identical to
the pre-T14 baseline (`open 0.36867`, `closed 0.23797`, leakage `0.0`,
`2097152` cells).

## Bounded re-trace and next-frame edit visibility

`rapid_destruction_reexposes_the_band_with_a_bounded_retrace` — remove one
represented 0.5 m occluder column from an emitter → receiver scene:

| | base | after the edit (next frame) |
| --- | ---: | ---: |
| shadowed band luminance (sRGB) | 22.77 | 26.27 |
| cells re-uploaded | — | 80 (the column) |
| cells re-traced | 2 097 152 (full) | 27 200 (~1.3 %) |
| GPU trace time | 1.43 ms | 0.11 ms |

The edit reaches the rendered indirect lighting in the **next frame**, the
re-trace is bounded to ~1.3 % of the cache, and the bounded trace is ~13×
cheaper than a full trace.

## Moving body — no ghost, no erased geometry

`moving_body_leaves_no_ghost_and_keeps_swept_geometry` — a cache-only occluder
moves out of an emitter → receiver path and back:

| step | shadowed band luminance (sRGB) | re-traced cells |
| --- | ---: | ---: |
| base (body in path) | 22.77 | 2 097 152 |
| body clears the path | 28.46 (recovers — no ghost) | ~65 000 (~3 %) |
| body returns | 22.77 (bit-identical to the base frame) | ~58 000 (~3 %) |

The returned frame is bit-identical to the base — the moving body left nothing
behind and the receiver wall it swept past was never erased.
`temporal_accumulation_keeps_a_moving_occluder_ghost_free` reruns this with the
temporal pass on (`temporal_weight = 0.1`) and confirms the same clear /
re-form / return-to-base behaviour.

## Moving camera — world-anchored cache does not smear

`panning_camera_temporal_matches_no_accumulation` — a static lit room viewed
from three yawed cameras, run once with accumulation off (`temporal_weight
= 1.0`) and once with heavy accumulation (`0.1`). The measured band luminance
matches within **1.0 sRGB unit** at every pose: because the lighting cache is
world-anchored, camera motion cannot shift or smear the accumulated lighting.

## Edit-commit → lighting latency

`cargo xtask capture --scene lighting-sequence` runs the `rapid_destruction`
edit followed by settle frames with `temporal_weight = 0.1`, and reports frame-
and millisecond-latency (millisecond figures use the provisional G2 client-frame
target, `16.7 ms`, as the nominal frame time):

| quantity | frames | ms (@ 16.7 ms/frame) |
| --- | ---: | ---: |
| edit → first frame that reflects it | 1 | 16.7 |
| edit → band within 5 % of its settled value | 1 | 16.7 |

Measured band luminance across the run (1920×1080, `temporal_weight = 0.1`,
8 settle frames): base **22.76** → edit frame **26.14** → settled **26.21**. The
edit frame is already ~99.7 % of the settled value; full bit-level settling
takes ~7 more frames but the visible difference is < 0.3 %. The edit step
re-uploaded 80 cells, re-traced 27 200 (~1.3 % of the cache) in **0.10 ms**, and
the temporal pass cost **0.11 ms**.

This is the mechanism latency for a single-frame-per-step capture; a wall-clock
figure against a live 60 Hz tick loop is deferred with the game loop.

## Known limits (carried into T15 / future work)

- **Bounded re-trace lag.** A lighting change is refreshed only inside the dirty
  AABB plus the halo. A far-reaching light change — a bright emitter moving many
  metres — leaves stale radiance beyond the halo until the next full re-trace.
  The gate fixtures keep light changes local; a periodic full re-trace or a halo
  sized to the ~48-cell trace reach covers the general case.
- **Blocky one-bounce trace.** Twelve fixed rays give directional colour
  bleeding; the spatial filter plus temporal accumulation reduce cell noise but
  do not add bounces.
- **Sub-0.5 m geometry** is invisible to the cache (unchanged from T13).
- **Cross-GPU quality and p95 frame cost** are unmeasured here — T15 / ENG-22.

## Reproduce

```powershell
cargo xtask capture --scene colored-room --width 1920 --height 1080 --output .local/runs/t13-lighting
cargo xtask capture --scene lighting-sequence --output .local/runs/t14-sequence
cargo test -p spall_render --test capture_gpu -- --ignored --test-threads=1
```
