# ENG-60 — Vulkan cascaded-shadow native crash: diagnosis

Follow-up to T12 / ENG-19 (merged PR #51). The T12 report already records that
"the first Vulkan run crashed in the native driver while using the comparison
depth-array path (`STATUS_ACCESS_VIOLATION`)" and that D3D12 is the accepted
Windows default. This report is the requested investigation of that crash.

**Outcome (2026-09-18, increment 2): fixed. Vulkan is restored as an accepted
Windows backend — the `capture_gpu` suite and a 1920x1080 six-view capture
both pass on Vulkan with pixel-identical output to D3D12 (max absolute
channel difference 1/255 across 8,294,400 channels, ordinary cross-backend
float rounding). The crash was never in the cascaded-shadow / comparison-depth
path this report's title names — that was ruled out correctly, but the actual
fault, pinned down in increment 2, was in `create_tone_pipeline`
(`shaders/tonemap.wgsl`): a naga/NVIDIA-driver defect triggered when a vertex
shader dynamically indexes a small `array<vec2<f32>, N>` constant table in a
pipeline whose fragment shader also reads a uniform buffer. The fix replaces
that array index with equivalent index arithmetic — no wgpu/naga upgrade, no
shadow or comparison-sampling change, no resource/binding change. See "Root
cause and fix (increment 2)" below for the full isolation trail.**

**Original increment-1 outcome (superseded below, kept for history): not
fixed under the pinned toolchain. The fault was believed to be in the `naga
24` SPIR-V → NVIDIA Vulkan driver pipeline-compile layer, needing a
`wgpu`/`naga` upgrade outside the scope of a self-contained fix. That
diagnosis correctly ruled out comparison-depth sampling (mutating
`opaque.wgsl`'s sampling mode and stubbing `shadow_visibility()` never changed
the outcome — true, because neither is where the fault was) but never actually
tested the tone-mapping pipeline in isolation, so it could not distinguish
"somewhere in `ScenePipeline::new`" from the real, much narrower answer. The
tone-map vertex shader's constant table has been unchanged since the original
T12 implementation, so this defect was very likely present — and
misattributed to the cascaded-shadow path by this report's own title — from
the start.**

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

## Where it faults (increment 1, 2026-09-08 — superseded, kept for history)

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

## D3D12 baseline (increment 1, historical)

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

## Recommendation (increment 1, historical — see "Recommendation (current)" at the end)

Keep the current guard: `RenderContext::headless()` selects `DX12` on Windows and
only uses `VULKAN` when `SPALL_WGPU_BACKEND=vulkan` is set explicitly, which now
also logs a one-line warning pointing here. Re-test Vulkan when the renderer
moves off the wgpu 24 pin (T14/T15 or a dedicated upgrade task); the
`vulkan_shadow_probe` example and the `windows_vulkan_backend_still_crashes`
ignored test in `tests/capture_gpu.rs` are the regression checks. Restore Vulkan
as an accepted backend only once `case:pipelines` and the `capture_gpu` suite
both pass on it.

## Regression watch — hardening (2026-09-10, historical)

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

(The paragraph above described the plan at the time; it turned out no
`wgpu`/`naga` upgrade was needed. See below.)

## Reproduce (increment 2, before the fix)

Confirmed the crash was still live, byte-for-byte as documented, before making
any change:

```
$ SPALL_ENG60_RECHECK=1 cargo test -p spall_render --test capture_gpu \
    windows_vulkan_backend_still_crashes_compiling_t12_pipelines -- --ignored --nocapture
...
>>> ScenePipeline::new (naga SPIR-V + vkCreateGraphicsPipelines x3)
test windows_vulkan_backend_still_crashes_compiling_t12_pipelines ... ok
```

("ok" here meant the test *confirmed the crash reproduces*, per its old
inverted assertion — the child process exited `0xc0000005`.) Same host, same
adapter (`NVIDIA GeForce RTX 4080 SUPER`, vendor `0x10de`, device `0x2702`),
same driver (`610.88`), same `wgpu 24.0.5` / `wgpu-hal 24.0.4` / `naga 24.0.0`
/ `ash 0.38.0+1.3.281` pin as increment 1.

## Upstream check (wgpu/naga releases, NVIDIA driver)

`cargo info wgpu` on this pin reports latest `30.0.1` (workspace is pinned to
`24.0.5`). Fetched the full upstream `CHANGELOG.md` (gfx-rs/wgpu, `trunk`) and
scanned every entry from `v25.0.0` through `v30.0.1` for anything Vulkan-
pipeline-crash-, descriptor-validation-, or shadow-sampler-shaped. Nothing
matched a fix for this construct. Two adjacent, ultimately-not-this-bug
findings worth recording:

- **wgpu issue [#10234](https://github.com/gfx-rs/wgpu/issues/10234)**
  ("Concurrent device and compute pipeline creation can cause `0xc0000005`
  access violation on Windows/Vulkan", filed 2026-09-01, open): reported on
  wgpu `30.0.1`, **the same NVIDIA driver `610.88`**, on a different GPU (RTX
  5090 Laptop). A linked comment traces it to a pre-1.4.345 Vulkan **loader**
  threading bug (`KhronosGroup/Vulkan-Loader#1866`, "Use recursive mutexes to
  fix deadlocks in loader", merged 2026-02-20). This machine's installed
  loader is `C:\Windows\System32\vulkan-1.dll` version **`1.4.341.0`** —
  exactly the pre-fix vintage. This looked like a strong candidate at first
  (same driver, same class of native crash, same loader era) but a controlled
  experiment ruled it out as *this* bug's mechanism: #10234 is a genuinely
  racy, intermittent fault under *concurrent, multi-threaded* pipeline
  creation across independent adapters/devices (2/5 stress runs in the
  reporter's own data). ENG-60's crash is 100% deterministic on a single
  thread building pipelines sequentially, and — see below — reordering which
  pipeline is built first moves the crash to whatever pipeline it is,
  regardless of how many pipelines were already built on that thread. That is
  inconsistent with a background-thread-contention race and consistent with a
  specific, repeatable shader/pipeline-shape defect. Recorded here because the
  loader is genuinely outdated and could still matter for *other*, unrelated
  Vulkan-on-Windows flakiness on this host; upgrading it is a Vulkan Runtime
  concern (a system component installed independently of the GPU driver) —
  flagged, not attempted, per this task's scope (no unattended system driver
  changes).
- No NVIDIA driver newer than the recorded `610.88` was investigated for
  installation (out of scope — system driver changes are not made
  unattended). Given the fix below does not depend on the driver, this is now
  moot for ENG-60 itself but may still be worth tracking for unrelated Vulkan
  issues.

## Root cause and fix (increment 2, 2026-09-18)

### The probe's `case:pipelines` was exercising more than "the three T12
### pipelines" — and the old marker didn't say which one faulted

Between increment 1 (PR #52, merged as part of `1bd0424`) and increment 1's
hardening (PR #83, `9fc6335`), T13 (`88845c4`, three minutes after `1bd0424`)
wired `IndirectPipeline::new` — three **compute** pipelines
(`spall-t13-trace-pipeline`, `spall-t13-denoise-pipeline`,
`spall-t14-temporal-pipeline`) for the one-bounce lighting prototype — into
`ScenePipeline::new`, ahead of the three T12 **render** pipelines. `case:
pipelines` calls the real `ScenePipeline::new()`, so from that point on it was
building *six* pipelines, not three, but the single coarse
`"ScenePipeline::new (...)"` marker written before the whole call gave no way
to tell which of the six faulted. PR #83 hardened the test's exit-code and
log-suffix checks but never added a marker between the six calls, so it
correctly reconfirmed "the crash is still there" without ever narrowing past
"somewhere in `ScenePipeline::new`".

Fine-grained, fsync'd markers (`crate::probe::mark`, `spall_render::probe`,
gated on `SPALL_PROBE_LOG` exactly like `vulkan_shadow_probe`'s own) were
added around each of the six pipeline-creation calls
(`crates/spall_render/src/indirect.rs`, `crates/spall_render/src/pipeline.rs`).
Re-running `case:pipelines` on Vulkan:

```
ScenePipeline::new: IndirectPipeline::new (3x vkCreateComputePipelines)
  IndirectPipeline: vkCreateComputePipelines(spall-t13-trace-pipeline)
  IndirectPipeline: vkCreateComputePipelines(spall-t13-denoise-pipeline)
  IndirectPipeline: vkCreateComputePipelines(spall-t14-temporal-pipeline)
  IndirectPipeline::new: all 3 compute pipelines OK
ScenePipeline::new: vkCreateGraphicsPipelines(opaque)
ScenePipeline::new: vkCreateGraphicsPipelines(shadow)
ScenePipeline::new: vkCreateGraphicsPipelines(tone_map)
<CRASH — process exits 0xc0000005; no further log lines>
```

All three compute pipelines and the opaque and shadow render pipelines
complete. The fault is **`create_tone_pipeline`** — the ACES tone-map pass —
and nothing upstream of it. To rule out "the 6th/last pipeline built" as the
actual variable (consistent with a driver background-compiler-thread pile-up,
matching the shape of wgpu #10234 above), `tone_map` was moved to build
*first* in `ScenePipeline::new`: it still crashed immediately, before touching
opaque/shadow/indirect at all. The fault is specific to `create_tone_pipeline`
and its shader, independent of build order or pipeline count.

### Minimal-repro bisection

Every experiment below is a standalone pipeline built by
`crates/spall_render/examples/vulkan_shadow_probe.rs` (`case:tone-*`), each
its own process (`SPALL_WGPU_BACKEND=vulkan ./vulkan_shadow_probe.exe
case:<name>`), so one crashing case can never corrupt the next:

| Case | Bind group (group 0) | Vertex shader | Fragment reads uniform? | Result |
| --- | --- | --- | --- | --- |
| `tone-real` | tex + sampler + uniform | production `array` (unsafe¹) | yes (branch + helper fn) | **CRASH** |
| `tone-bisect-triangle-only` | tex + sampler | proven-safe `array`² | no | OK |
| `tone-bisect-vertex` | tex + sampler | production `array` (unsafe¹) | no | **CRASH** |
| `tone-bisect-extra-binding` | tex + sampler + uniform (unused) | proven-safe `array`² | no (declared, unread) | OK |
| `tone-bisect-uniform-read` | tex + sampler + uniform | proven-safe `array`² | **yes** (`linear * globals.exposure`) | **CRASH** |
| `tone-uniform-separate-group` | tex+sampler (group 0), uniform (group 1) | proven-safe `array`² | yes | **CRASH** (rules out "shared bind group" as the cause) |
| `tone-bisect-srgb-format` / `-unorm-format` | tex + sampler + uniform | proven-safe `array`² | yes | **CRASH** both (rules out sRGB output format) |
| `tone-bisect-no-array-index` | tex + sampler + uniform | **index arithmetic**, no array | yes | OK |
| `tone-real-fix` | tex + sampler + uniform | **index arithmetic**, no array | yes (real branch + helper fn, byte-identical to `tone-real`) | **OK — the fix** |

¹ the real `tonemap.wgsl` triangle: `array<vec2<f32>,3>(vec2(-1,-1), vec2(3,-1),
vec2(-1,3))`, indexed by `@builtin(vertex_index)`.
² the triangle already proven safe by increment 1's `case:varying+sample`:
`array<vec2<f32>,3>(vec2(-1,-3), vec2(-1,1), vec2(3,1))`, same indexing.

(Several more intermediate cases — `tone-no-uniform`, `tone-uniform-unused`,
`tone-bisect-clamp`, `tone-bisect-combo`, `tone-no-branch*`,
`tone-branch-no-helper-safe-triangle`, `tone-branchless-fix-candidate`,
`tone-bisect-dedup-scaled`, `tone-fix-candidate` — are kept in the probe and
documented in its module doc comment; they either confirmed a factor was
*not* sufficient alone (a `clamp()` call, an unused extra binding, an `if`
branch, a helper-function call, sRGB vs. UNORM output — none of these alone
reproduces it) or, in `tone-fix-candidate`'s case, showed that swapping only
the triangle's *values* while keeping array-indexing does not fix the real
shader — the indexing itself is the trigger, not which literals fill it.)

**Root cause:** a vertex shader dynamically indexing a small
`array<vec2<f32>, N>` constant table with `@builtin(vertex_index)` (the
standard fullscreen-triangle trick), in a pipeline whose fragment shader also
reads a uniform buffer, corrupts something in NVIDIA driver `610.88`'s SPIR-V
pipeline compiler on Windows/Vulkan — a hard native crash during
`vkCreateGraphicsPipelines`, before any command is ever recorded. This is
independent of: which specific float literals fill the array (a
same-shaped-but-differently-valued table, `tone-bisect-dedup-scaled`, also
crashes); whether the uniform and texture/sampler share a bind group or sit in
separate groups; the color target format (sRGB vs. UNORM); whether the
fragment body branches or calls a helper function; and whether the uniform's
value is used trivially or in the real ACES computation. It requires **both**
the array-indexed vertex position **and** a fragment-stage uniform read —
either alone is fine (`case:varying+sample` from increment 1 has the array
index with no uniform; `tone-bisect-extra-binding` has the uniform declared
but unread). Neither naga's own validator nor D3D12 (byte-identical shaders,
bind groups and pipeline descriptors) sees anything wrong; this is specific to
what NVIDIA's Vulkan driver does with naga's SPIR-V output for this
combination.

No Vulkan validation layer message was obtained — the Vulkan SDK is still not
installed on this host (unchanged from increment 1), and this is a pipeline-
*creation*-time native crash inside the driver's own shader compiler, not a
usage-validation error a layer would catch even if installed.

### The fix

`crates/spall_render/src/shaders/tonemap.wgsl`'s vertex shader now generates
the fullscreen-triangle position with index arithmetic instead of indexing a
local array:

```wgsl
// before (crashed on Vulkan):
let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
out.position = vec4<f32>(p[index], 0.0, 1.0);
out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);

// after (the fix):
let x = f32(i32(index << 1u) & 2) * 2.0 - 1.0;
let y = f32(i32(index) & 2) * 2.0 - 1.0;
out.position = vec4<f32>(x, y, 0.0, 1.0);
out.uv = vec2<f32>(x, y) * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
```

For `index` in `0..3` this produces exactly `(-1,-1)`, `(3,-1)`, `(-1,3)` —
byte-identical to the array it replaces — so the rendered triangle, and every
interpolated `uv` at every pixel inside the visible `[-1,1]²` region, is
unchanged. (This is the standard property of the "big overreaching triangle"
trick: any triangle whose vertices carry their own true NDC position as an
interpolated attribute reconstructs the exact NDC position at every covered
pixel, independent of which specific covering triangle was used — increment
2's own bisection cases exploit this by swapping in a *differently-shaped*
covering triangle and confirming identical downstream behaviour.) This is a
standards-correct, purely-internal rewrite of one pass's vertex shader: it
does not touch cascaded shadows, comparison sampling, any bind group,
resource, or binding, and does not change `RenderContext`'s device/adapter
selection logic beyond removing the now-inaccurate "Vulkan crashes" warning.

`crates/spall_render/src/probe.rs` (new, `pub(crate)`) and the `crate::
probe::mark` calls added to `pipeline.rs` / `indirect.rs` are kept
permanently: they cost one `std::env::var` lookup on the pipeline-creation
path used once at renderer start-up, are silent unless `SPALL_PROBE_LOG` is
set, and are the reason increment 2 could localize this at all. If a future
change reintroduces a similar construct, the same marker trail will show
exactly which of the (now six, or more) pipeline-creation calls faults.

### Acceptance evidence

```
$ SPALL_WGPU_BACKEND=vulkan SPALL_PROBE_LOG=fix-verify.log \
    ./target/debug/examples/vulkan_shadow_probe.exe case:pipelines
...
>>> ScenePipeline::new (naga SPIR-V + vkCreateGraphicsPipelines x3)
>>>   pipelines built OK (no crash)
```

Full log confirms all six pipeline-creation calls (3 compute + 3 graphics)
completed. Full regression suite, both backends, on this host:

```
$ cargo test -p spall_render --test capture_gpu -- --ignored --test-threads=1
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.74s

$ SPALL_WGPU_BACKEND=vulkan cargo test -p spall_render --test capture_gpu -- --ignored --test-threads=1
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.46s
```

(10 = the 9 pre-existing `--ignored` GPU tests plus the renamed ENG-60 guard,
`windows_vulkan_backend_compiles_t12_pipelines_without_crashing`, which now
asserts the pipelines build *cleanly* and would fail loudly if the native
crash ever came back.)

1920×1080 six-view `capture_scene`, cube-and-moving-body fixture, RTX 4080
SUPER, both backends:

| Backend | GPU total (device timestamps) | shadow ms | opaque ms | tone map ms |
| --- | ---: | ---: | ---: | ---: |
| D3D12 | 0.264 ms | 0.041 | 0.094 | 0.121 |
| Vulkan | 0.136 ms | 0.017 | 0.051 | 0.062 |

Both comfortably sub-millisecond for this fixture; across repeated runs the
per-backend numbers vary by roughly 1.5-2x run-to-run (GPU timestamp queries on
a near-instant six-triangle scene are dominated by fixed dispatch overhead,
not fill work), and which backend comes out ahead flips between runs. This is
expected timing noise on a trivial fixture, not a claim that either backend is
faster in general. Six output images
(`albedo`/`depth`/`normals`/`roughness`/`shaded`/`shadow_cascades`.png)
compared byte-for-byte between backends: five of six are **byte-identical**;
`shaded.png` (the final tone-mapped output — the one pass whose shader
changed) differs by a **maximum absolute channel value of 1 (out of 255)** on
a single channel across the entire 1920×1080×4 = 8,294,400-channel image —
ordinary cross-backend floating-point rounding in the ACES division, not a
visual regression. `cargo test --workspace --all-features` is unaffected:
same pass count as before this change, no new failures, no flake in
`spall_net::separate_process_transport` on this run.

## Recommendation (current)

**Vulkan is an accepted Windows backend again.** `RenderContext::headless()`
still defaults to D3D12 on Windows (no reason found to change the default),
but `SPALL_WGPU_BACKEND=vulkan` now builds and runs the full T12/T13/T14
pipeline set without the historical crash, and produces the same images (to
float-rounding tolerance) at the same performance envelope as D3D12. The
regression guard is
`windows_vulkan_backend_compiles_t12_pipelines_without_crashing` in
`tests/capture_gpu.rs`; the `vulkan_shadow_probe` example's `tone-*` cases are
kept as the bisection starting point should a similar construct ever
reintroduce a driver-compiler defect like this one. No `wgpu`/`naga` upgrade
was needed — the fix is entirely inside this crate's own tone-map shader.
