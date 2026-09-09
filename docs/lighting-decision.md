# T13 indirect-light feasibility decision

Status: **accepted 2026-09-09** after coordinator review of the single-adapter
rendered controls (see "Fixture and measurements"). The resource/pass contract
in this document is now **frozen for T14** — the exact frozen surface is listed
under "Freeze list (frozen for T14)". Acceptance covers indirect-lighting
*feasibility* only: it is not final visual quality, not a persistent world
format, and makes no cross-GPU, temporal-stability, or p95 frame-cost claim
(those are T14/T15 acceptance work).

## Decision

Use one camera-local `128 x 128 x 128` occupancy/material cache at `0.5 m` per
cell (a `64 m` cube) for the first indirect-lighting gate. Rasterized greedy
meshes remain the full-resolution visibility source; this coarse cache affects
lighting only and can never affect collision, topology, replication, or saved
geometry.

The CPU staging representation is one `u32` material id per cell (`0 = air`,
8 MiB total). The GPU trace writes linear RGB irradiance to an
`array<vec4<f32>>`; a second equally-sized buffer receives the denoised result
(32 MiB each). The prototype therefore retains about 72 MiB for occupancy plus
the two radiance buffers, excluding small uniforms, authored material data, and
driver allocation overhead. This is deliberately simple enough to measure
before choosing texture/brick-atlas packing.

Material records keep linear base color, roughness, metalness, and a nonnegative
emissive multiplier. Emission uses `base_color * emissive`; no runtime light
entity or second material registry is introduced.

## Freeze list (frozen for T14)

Accepted 2026-09-09. T14 builds against exactly this surface; changing any
frozen item requires fresh colored-room / leakage evidence and a new review.

**Frozen — T14 may not change without re-review:**

- Cache geometry: `128 x 128 x 128` cells, `0.5 m` per cell, `64 m` cube,
  camera-local, derived render resource only.
- Hard boundary: the cache can never affect collision, topology, replication, or
  saved geometry.
- CPU staging format: one `u32` material id per cell, `0 = air`.
- GPU radiance format: linear RGB in an `array<vec4<f32>>`, plus a second
  equally-sized buffer for the denoised result.
- Material encoding: linear base color + roughness + metalness + nonnegative
  emissive multiplier; emission `= base_color * emissive`; no light entity, no
  second registry.
- Explicit pass order: the six steps below (T14 temporal work is an inserted
  sub-step after step 3, not a reorder).
- `DebugView::IndirectOnly` semantics: direct sun and T12 ambient disabled.

**Not frozen — T14 may choose or change (additive):**

- Denoiser internals (tap count / weights), provided it stays occupancy-aware
  and never averages through an occupied cell.
- Trace ray count, directions, and maximum cache-cell march distance.
- New temporal history resources (previous radiance, previous cache origin,
  per-cell age / validity) — purely additive.
- Dirty-region update strategy and camera clipmap scroll strategy.
- Later texture / brick-atlas packing, under the same evidence bar and the same
  derived-cache boundary.

## Traversal and sampling

The trace compute pass dispatches `4 x 4 x 4` workgroups across the full cache.
For each air cell it sends twelve fixed, deterministic axis/diagonal rays for
at most 48 cache cells. The first occupied cell contributes its emissive
radiance plus a small sky-seeded diffuse source; a ray leaving the cache samples
the fixed linear sky color. This is one diffuse bounce: the result is not fed
back into the trace and there is no iterative propagation.

The denoise compute pass applies one occupancy-aware seven-tap spatial filter
(centre plus six face neighbours). It never averages through an occupied cache
cell. The HDR opaque pass samples the filtered air cell one cache cell beyond
the raster surface and modulates it by the receiving material's diffuse color.
`DebugView::IndirectOnly` disables the direct sun and T12 ambient terms, so the
prototype cannot pass by showing AO or bloom.

The explicit pass order is:

1. Upload immutable occupancy/material staging data.
2. Trace one-bounce irradiance.
3. Spatially denoise it.
4. Render the existing cascaded sun shadows.
5. Raster HDR opaque geometry, sampling filtered irradiance.
6. Apply the fixed-exposure tone map.

There is no temporal history resource in T13. T14 owns reprojection,
depth/normal rejection, clamping, history invalidation, and dirty updates for
terrain edits and old/new moving-volume bounds. Until T14, each capture builds
the complete lighting cache once from an explicit immutable fixture snapshot.

## Fixture and measurements

Reproduce the measured fixture with:

```powershell
cargo xtask capture --scene colored-room --width 1920 --height 1080 --output .local/runs/t13-lighting
cargo test -p spall_render --test capture_gpu colored_room_produces_real_indirect_only_pixels_and_separate_timings -- --ignored --test-threads=1
cargo test -p spall_render --test capture_gpu indirect_only_render_shows_emitter_transport_and_wall_occlusion -- --ignored --test-threads=1
```

The command captures open- and closed-roof variants as both `shaded.png` and
`indirect_only.png`, and writes `summary.json`. The fixture uses red/blue walls,
an emissive orange panel, a fixed camera/exposure, and the same 128-cubed cache.

Measured on Windows 11, NVIDIA GeForce RTX 4080 SUPER, D3D12, pinned `wgpu
24.0.5`, debug build, 1920x1080, GPU timestamp queries enabled:

| Variant | CPU cache upload | GPU trace | GPU denoise | Total measured GPU passes |
| --- | ---: | ---: | ---: | ---: |
| open | 2.970 ms | 1.412 ms | 0.105 ms | 1.655 ms |
| closed | 1.431 ms | 1.430 ms | 0.089 ms | 1.766 ms |

These are single captures, not percentiles. PNG compression dominates the CPU
capture wall time and is excluded from GPU feasibility. Vulkan is not claimed:
ENG-60 separately tracks the pinned wgpu/driver crash in T12's cascaded shadow
comparison path.

The deterministic CPU copy of the shader trace measured probe luminance
`0.368666` in the open room and `0.237972` in the closed room (35.4% darker).
The same ray set measured `0.0` residual emitter contribution behind one aligned
`0.5 m` occupied wall versus the unobstructed control.

Those CPU-copy numbers are corroborated on rendered pixels by
`indirect_only_render_shows_emitter_transport_and_wall_occlusion`, which reads
only `DebugView::IndirectOnly` output — i.e. results that passed through the GPU
trace, the GPU denoise, and the opaque surface sample. Against an
emissive-term-removed control captured from the same geometry and cache, it
requires that a non-emissive stone receiver wall brightens where the represented
panel can reach it, and that inserting one represented `0.5 m` wall between the
panel and the measured band removes at least half of those transported pixels
and lowers the band mean. `colored_room_produces_real_indirect_only_pixels_and_separate_timings`
additionally renders both roof variants and asserts the closed room is darker in
the render, in the same direction as the probe copy. Rendered leakage past the
represented wall is higher than the CPU `0.0` (denoise spread and the fixed
diagonal rays), but stays a minority of the unoccluded transport.

### Acceptance review run — 2026-09-09

Re-run for the acceptance review on the reference adapter (NVIDIA GeForce
RTX 4080 SUPER, Dx12, `gpu_timing_available: true`), 1920x1080,
`.local/runs/t13-review`:

| Variant | GPU trace | GPU denoise | GPU render total | open probe | closed probe | thin-wall leakage | cells |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| open | 1.420 ms | 0.105 ms | 1.670 ms | 0.36867 | 0.23797 | 0.0 | 2097152 |
| closed | 1.443 ms | 0.089 ms | 1.819 ms | 0.36867 | 0.23797 | 0.0 | 2097152 |

Reproduces the prototype numbers. `indirect_only.png` for the two variants was
inspected: the open room shows red/blue wall bleed, warm emitter transport onto
the back wall and floor, and cool sky fill through the open ceiling; the closed
room is visibly darker (sky fill gone) with only the emitter cone and its
ceiling bounce, matching the `35.4%` darker probe. `trace` and `denoise` are
reported as separate GPU timestamp measurements. Blocky directional bleed is
present as documented.

Rendered cross-GPU quality, temporal stability, and p95 frame cost remain
unvalidated and are T14/T15 acceptance work. The single-adapter rendered
controls required for this feasibility acceptance have been reviewed (above);
the contract is frozen for T14 per "Freeze list (frozen for T14)".

## Known limits and T14 contract

- A wall thinner than `0.5 m` that fails to occupy a cache cell is invisible to
  this lighting representation; worst-case leakage is therefore 100% for
  omitted sub-cell geometry. The measured zero above applies only to a wall
  represented in the cache.
- Twelve fixed rays produce blocky, directional color bleeding. The one-pass
  filter reduces cell noise but cannot replace temporal accumulation.
- Full-cache upload/recompute is acceptable for this static feasibility run,
  not for destruction. T14 must update dirty regions and both old/new body
  bounds without clearing overlapping occupancy.
- The material-id buffer and two `vec4<f32>` radiance buffers are intentionally
  memory-heavy. Packing or a brick atlas may be adopted only with equivalent
  colored-room/leakage evidence and the same derived-cache boundary.
- The 1.5 ms-class compute result fits inside the provisional 12 ms G2 GPU
  budget on this adapter, but it does not establish p95 frame cost, temporal
  stability, moving-object correctness, or cross-GPU quality. Those remain T14
  and T15 acceptance work.
