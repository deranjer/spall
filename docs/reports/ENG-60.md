# ENG-60 — Vulkan cascaded-shadow native crash: diagnosis

Follow-up to T12 / ENG-19 (merged PR #51). The T12 report already records that
"the first Vulkan run crashed in the native driver while using the comparison
depth-array path (`STATUS_ACCESS_VIOLATION`)" and that D3D12 is the accepted
Windows default. This report is the requested investigation of that crash.

**Outcome: not fixed under the pinned toolchain. D3D12 stays the Windows
default; Vulkan stays an unsupported explicit opt-in. The fault is in the
`naga 24` SPIR-V → NVIDIA Vulkan driver pipeline-compile layer, not in this
crate's resource or binding setup, and not in shader runtime behaviour. It
needs a `wgpu`/`naga` upgrade (or an NVIDIA driver that no longer faults on
naga 24 output), both outside the scope of a self-contained fix. ENG-60 stays
open.**

## Environment (recorded)

| Item | Value |
| --- | --- |
| OS | Windows 11 Pro 26200 |
| GPU | NVIDIA GeForce RTX 4080 SUPER (vendor `0x10de`, device `0x2702`) |
| Vulkan ICD | `nvoglv64.dll`, `VkPhysicalDeviceDriverProperties.driverInfo = "610.88"`, `driverName = "NVIDIA"` |
| D3D12 driver | WDDM package `32.0.16.1088` |
| wgpu | `24.0.5` (workspace pin) |
| wgpu-hal | `24.0.4` |
| naga | `24.0.0` |
| ash | `0.38.0+1.3.281` |
| Implicit VK layers present | `VK_LAYER_NV_present`, `VK_LAYER_OBS_HOOK` (OBS Studio) |

## Reproduce

```sh
# Native crash (rc 139 / STATUS_ACCESS_VIOLATION 0xC0000005):
SPALL_WGPU_BACKEND=vulkan cargo test -p spall_render --test capture_gpu -- --ignored --test-threads=1

# Same, isolated, with a synced step log:
cargo build -p spall_render --example vulkan_shadow_probe
SPALL_WGPU_BACKEND=vulkan SPALL_PROBE_LOG=probe.log \
  ./target/debug/examples/vulkan_shadow_probe.exe case:pipelines
cat probe.log   # last line: "ScenePipeline::new (naga SPIR-V + vkCreateGraphicsPipelines x3)"
```

`cargo test` surfaces the exact status:

```
process didn't exit successfully: `...capture_gpu-....exe --ignored --test-threads=1`
(exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
```

## Where it faults

The `vulkan_shadow_probe` example writes a fsync'd marker before every GPU step,
so a hard native crash still names the last call reached. Results:

| Probe mode | Vulkan | D3D12 |
| --- | --- | --- |
| `case:no-sample` (fragment varying, no texture sample) | OK | OK |
| `case:sample-no-varying` (`textureSample`, coord from `@builtin(position)`) | OK | OK |
| `case:varying+sample` (varying used as `textureSample` coord) | OK | OK |
| `case:varying+sample-level` (`textureSampleLevel`) | OK | OK |
| `case:pipelines` (`ScenePipeline::new()` — real opaque+shadow+tonemap) | **CRASH** | OK |
| `full` (pipelines + shadow raster + opaque depth-array sample + readback) | **CRASH** | OK |
| `capture:1920x1080` (six-view `capture_scene`) | **CRASH** | OK |

Conclusions from the matrix:

1. **The crash is at pipeline-creation time**, inside `ScenePipeline::new()` —
   before any draw, bind, or sample is recorded or submitted. `case:pipelines`
   does nothing but build the three T12 render pipelines and it still dies.
2. **It is not the comparison depth-array sample.** Swapping
   `textureSampleCompare` → `textureSampleCompareLevel` in `opaque.wgsl` (explicit
   LOD, `OpImageSampleDrefExplicitLod`, allowed in non-uniform control flow) does
   not change the outcome. Stubbing `shadow_visibility()` to `return 1.0;` before
   the sample loop does not change it either.
3. **It is not this crate's wgpu resource/binding setup.** D3D12 accepts the
   byte-identical `wgpu` calls, shaders, bind-group layouts, texture/array views,
   samplers and pipeline descriptors and passes every `capture_gpu` test.
4. **It is not a lifetime / command-ordering bug.** The device, queue, pipeline
   and views are all still live; nothing has been dropped; no command buffer has
   been built yet.
5. **It is not the implicit Vulkan layers.** Re-run with
   `VK_LOADER_LAYERS_DISABLE=*` (kills `VK_LAYER_OBS_HOOK` and
   `VK_LAYER_NV_present`) — still crashes. `DISABLE_VULKAN_OBS_CAPTURE=1` alone —
   still crashes.
6. **It is not the GPU timestamp-query feature.** `SPALL_DISABLE_GPU_TIMESTAMPS=1`
   (device created without `TIMESTAMP_QUERY`) — still crashes.
7. A minimal synthetic fullscreen pipeline with a `@location(0)` varying **and** a
   `textureSample` compiles fine (`case:varying+sample`). The trigger needs
   something specific to the T12 shaders (e.g. the `array<mat4x4<f32>, 4>` in the
   uniform block, the runtime-sized `storage` array + `arrayLength`, the
   `flat`-interpolated `u32` vertex attribute, or a combination). NVIDIA's Vulkan
   shader compiler runs on background threads, so a per-pipeline `probe_mark`
   bisect could not pin the exact construct without a native debugger, and the
   Vulkan SDK / validation layers are not installed on the reference host.

naga's own validator accepts all these modules and emits SPIR-V without error
(`vulkan_shadow_probe spirv` dumps `spall-eng60-*.spv`); the fault is in what the
NVIDIA driver does with naga 24's SPIR-V, which the D3D12/HLSL path never sees.

## D3D12 baseline (accepted)

`cargo test -p spall_render --test capture_gpu -- --ignored --test-threads=1`
→ `3 passed` on D3D12.

Six-view `capture_scene` at 1920×1080, cube fixture, RTX 4080 SUPER / D3D12,
device timestamp-query pass times:

| shadow ms | opaque ms | tone map ms | GPU total ms |
| ---: | ---: | ---: | ---: |
| 0.045 | 0.056 | 0.050 | 0.152 |

(Consistent with `docs/reports/T12.md`; CPU total is dominated by PNG encode and
is not a GPU figure.)

Vulkan produces no timings or images — it never returns from `ScenePipeline::new()`.

## Recommendation

Keep the current guard: `RenderContext::headless()` selects `DX12` on Windows and
only uses `VULKAN` when `SPALL_WGPU_BACKEND=vulkan` is set explicitly, which now
also logs a one-line warning pointing here. Re-test Vulkan when the renderer
moves off the wgpu 24 pin (T14/T15 or a dedicated upgrade task); the
`vulkan_shadow_probe` example and the `windows_vulkan_backend_still_crashes`
ignored test in `tests/capture_gpu.rs` are the regression checks. Restore Vulkan
as an accepted backend only once `case:pipelines` and the `capture_gpu` suite
both pass on it.

## Regression watch — hardening (2026-09-10)

The 2026-09-09 reporting audit flagged that the merged
`windows_vulkan_backend_still_crashes_compiling_t12_pipelines` test asserted only
`!status.success()` on a `cargo run`, so a compile error, a missing adapter, a
hang, or an ordinary Rust panic would all have counted as "the crash still
reproduces". The test now separates those cases:

1. **Build the probe as its own step.** A build failure fails the test with a
   "this is a build failure, not a Vulkan result" message instead of being
   folded into the crash assertion.
2. **Run the built binary directly**, not through `cargo run`, so cargo's own
   exit status can never stand in for the child's.
3. **Wall-clock deadline (120 s).** The documented fault is a *fast* native crash
   during pipeline compilation; a hang is killed and reported as a distinct
   failure to investigate, not a pass.
4. **Exact exit code.** Exit `2` (probe's "no usable adapter") → inconclusive
   skip. Success → the crash is gone, fail loudly. Non-zero must equal the
   documented native `STATUS_ACCESS_VIOLATION` (`0xC0000005` /
   `-1_073_741_819`); a Rust panic (`101`) or any other code fails with "the
   failure mode has changed".
5. **Crash site.** The probe's fsync'd `SPALL_PROBE_LOG` must end on the
   `ScenePipeline::new (...)` marker — proof the fault is still at
   pipeline creation and not earlier (adapter enumeration, device creation).

A green run of this test remains confirmation of a *known* failure on the pinned
toolchain, never Vulkan acceptance. Acceptance still requires the full
`capture_gpu` suite and 1080p six-view captures to pass on both D3D12 **and**
Vulkan, which needs the `wgpu`/`naga` upgrade this report recommends.
