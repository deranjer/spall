//! ENG-60 diagnostic probe. History: the pinned wgpu 24 Vulkan backend used to
//! crash natively (`STATUS_ACCESS_VIOLATION`, 0xC0000005) on the recorded
//! NVIDIA Windows driver while `ScenePipeline::new` compiled its pipelines.
//! D3D12 with the identical shaders was unaffected. **Fixed 2026-09-18**: the
//! trigger was `shaders/tonemap.wgsl`'s vertex shader dynamically indexing a
//! small `array<vec2<f32>, 3>` fullscreen-triangle constant table in a
//! pipeline whose fragment shader also reads a uniform buffer — a naga/driver
//! defect, not a resource/binding bug in this crate's shadow or comparison-
//! sampling code. Replacing the array index with equivalent index arithmetic
//! (identical `x`/`y` for every `vertex_index`) avoids it. Full isolation
//! trail and evidence: `docs/reports/ENG-60.md`.
//!
//! This binary prints full adapter/driver identification, then builds a series
//! of isolated one-pipeline cases — the ones that originally bounded the fault
//! (`no-sample` .. `varying+sample-level`) plus the `tone-*` cases that pinned
//! it down to `create_tone_pipeline` and then to the exact minimal repro
//! (`tone-bisect-*`) and fix (`tone-real-fix`) — and finally the full offscreen
//! capture path. Every step is committed to a synced log file
//! (`SPALL_PROBE_LOG`) before it runs, so a hard native crash still names the
//! GPU call that faulted. Kept as a permanent regression tool: if a future
//! change reintroduces a similar construct and Vulkan starts crashing again,
//! these cases are the starting point for re-isolating it.
//!
//! Usage:
//!   SPALL_WGPU_BACKEND=vulkan SPALL_PROBE_LOG=probe.log \
//!     cargo run -p spall_render --example vulkan_shadow_probe -- <mode>
//!
//! Add `RUST_LOG=wgpu_hal=trace` with any `log` subscriber on the path to see
//! the last Vulkan loader call before a native crash.
//!
//! Modes (argument 1; default `full`):
//!   info                 enumerate adapters + selected device, then exit
//!   spirv                dump naga SPIR-V for the T12 + case shaders, then exit
//!   case:<name>          build exactly one pipeline (see below), then exit
//!   capture[:WxH]        real six-view capture_scene() + timings (default 1920x1080)
//!   full                 pipelines + shadow raster + opaque depth-array sample + readback
//!
//! case:<name> — increment 1 (`no-sample` .. `varying+sample-level`) bounded
//! the fault to somewhere inside the real T12/T13/T14 pipelines
//! (`case:pipelines` — now builds clean; historically the minimal crash).
//! Increment 2 (`tone-*`) narrowed it to `create_tone_pipeline` and then to
//! the exact minimal repro and fix:
//!   no-sample             fragment varying, NO texture sample                    (ok)
//!   sample-no-varying     texture sample, coord from @builtin(position)          (ok)
//!   varying+sample        fragment varying used as textureSample coord          (ok)
//!   varying+sample-level  ... textureSampleLevel (explicit LOD)                  (ok)
//!   pipelines             real ScenePipeline::new() (3 compute + 3 graphics)     (ok, was CRASH)
//!   tone-real             real tonemap.wgsl, standalone, first pipeline built    (ok, was CRASH)
//!   tone-bisect-vertex    unsafe (production) triangle table, minimal fragment   (CRASH)
//!   tone-bisect-triangle-only   safe triangle table, minimal fragment            (ok)
//!   tone-bisect-uniform-read    safe triangle + fragment reads a uniform field   (CRASH)
//!   tone-bisect-no-array-index  index arithmetic instead of array + uniform read (ok)
//!   tone-real-fix         real tonemap.wgsl fragment body + index-arithmetic vertex (ok — the fix)
//! See the module doc above and `docs/reports/ENG-60.md` for the full trail
//! (`tone-bisect-clamp`, `tone-bisect-extra-binding`, `tone-bisect-combo`,
//! `tone-no-uniform`, `tone-uniform-unused`, `tone-no-branch*`,
//! `tone-branch-no-helper-safe-triangle`, `tone-branchless-fix-candidate`,
//! `tone-bisect-dedup-scaled`, `tone-bisect-unorm-format`,
//! `tone-bisect-srgb-format`, `tone-uniform-separate-group`, `tone-fix-candidate`).

use std::io::Write;

use glam::{Mat4, Vec3};
use spall_mesh::MeshStrategy;
use spall_mesh::fixtures::{acceptance_shapes, mesh_shape};
use spall_render::{
    CASCADE_COUNT, Camera, CaptureOptions, DebugView, GpuMesh, OffscreenTarget, RenderContext,
    Scene, SceneItem, ScenePipeline, UploadBudget, capture_scene, to_gpu,
};

fn mark(step: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, ">>> {step}");
    let _ = out.flush();
    // A crash inside the driver discards buffered stdio, so also commit each
    // marker to disk with an explicit fsync. `SPALL_PROBE_LOG` names the file.
    let Ok(path) = std::env::var("SPALL_PROBE_LOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{step}");
        let _ = f.flush();
        let _ = f.sync_all();
    }
}

fn backends_from_env() -> wgpu::Backends {
    match std::env::var("SPALL_WGPU_BACKEND").as_deref() {
        Ok("dx12") => wgpu::Backends::DX12,
        Ok("vulkan") => wgpu::Backends::VULKAN,
        Ok("gl") => wgpu::Backends::GL,
        _ => wgpu::Backends::all(),
    }
}

/// A self-contained fullscreen-triangle shader. `varying` routes the sample
/// coordinate through a `@location(0)` vertex→fragment output; without it the
/// fragment shader derives the coordinate from `@builtin(position)`. `sample`
/// toggles the `textureSample`; `level` makes it an explicit-LOD sample.
fn case_shader(varying: bool, sample: bool, level: bool) -> String {
    let call = match (sample, level) {
        (false, _) => "vec4<f32>(0.2, 0.4, 0.6, 1.0)".to_string(),
        (true, false) => "vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0)".to_string(),
        (true, true) => "vec4<f32>(textureSampleLevel(tex, samp, uv, 0.0).rgb, 1.0)".to_string(),
    };
    let (vs_out, vs_body, fs_in, fs_uv) = if varying {
        (
            "struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };",
            "var out: VsOut; out.position = vec4<f32>(p, 0.0, 1.0); out.uv = p * 0.5 + vec2<f32>(0.5); return out;",
            "in: VsOut",
            "let uv = in.uv;",
        )
    } else {
        (
            "",
            "return vec4<f32>(p, 0.0, 1.0);",
            "@builtin(position) c: vec4<f32>",
            "let uv = c.xy * 0.001;",
        )
    };
    let vs_ret = if varying {
        "VsOut"
    } else {
        "@builtin(position) vec4<f32>"
    };
    format!(
        "@group(0) @binding(0) var tex: texture_2d<f32>;\n\
         @group(0) @binding(1) var samp: sampler;\n\
         {vs_out}\n\
         @vertex fn vs_main(@builtin(vertex_index) i: u32) -> {vs_ret} {{\n\
         \tlet tri = array<vec2<f32>,3>(vec2(-1.0,-3.0), vec2(-1.0,1.0), vec2(3.0,1.0));\n\
         \tlet p = tri[i];\n\
         \t{vs_body}\n\
         }}\n\
         @fragment fn fs_main({fs_in}) -> @location(0) vec4<f32> {{\n\
         \t{fs_uv}\n\
         \treturn {call};\n\
         }}\n"
    )
}

fn dump_spirv(name: &str, wgsl: &str) {
    let module = match naga::front::wgsl::parse_str(wgsl) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[spirv] {name}: wgsl parse error: {e}");
            return;
        }
    };
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .expect("naga validate");
    let spv =
        naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None)
            .expect("naga spv-out");
    let bytes: Vec<u8> = spv.iter().flat_map(|w| w.to_le_bytes()).collect();
    let path = std::env::temp_dir().join(format!("spall-eng60-{name}.spv"));
    std::fs::write(&path, &bytes).unwrap();
    println!("[spirv] {name}: {} words -> {}", spv.len(), path.display());
}

fn print_adapters(instance: &wgpu::Instance, backends: wgpu::Backends) {
    for adapter in instance.enumerate_adapters(backends) {
        let i = adapter.get_info();
        println!(
            "  backend={:?} name={:?} vendor=0x{:04x} device=0x{:04x} type={:?} driver={:?} driver_info={:?}",
            i.backend, i.name, i.vendor, i.device, i.device_type, i.driver, i.driver_info
        );
    }
    let _ = std::io::stdout().flush();
}

/// Build one render pipeline from `wgsl` with a texture+sampler bind group
/// layout. Returns only if the driver did not crash. `format` lets ENG-60
/// isolation compare an SRGB color target (what `create_tone_pipeline` uses)
/// against a plain UNORM one with an otherwise byte-identical pipeline.
fn build_one_pipeline_fmt(
    ctx: &RenderContext,
    label: &str,
    wgsl: &str,
    format: wgpu::TextureFormat,
) {
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
    let bgl = ctx
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
    let layout = ctx
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe-layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
    mark(&format!(
        "  vkCreateGraphicsPipelines({label}, {format:?})..."
    ));
    let _pipeline = ctx
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("probe-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });
    ctx.wait();
    mark(&format!("  {label}: pipeline OK (no crash)"));
}

/// Build one render pipeline from `wgsl` with a texture+sampler bind group
/// layout. Returns only if the driver did not crash.
fn build_one_pipeline(ctx: &RenderContext, label: &str, wgsl: &str) {
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
    let bgl = ctx
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
    let layout = ctx
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe-layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
    mark(&format!("  vkCreateGraphicsPipelines({label})..."));
    let _pipeline = ctx
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("probe-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });
    ctx.wait();
    mark(&format!("  {label}: pipeline OK (no crash)"));
}

/// `tone-bisect-no-array-index`: the exact `tone-bisect-uniform-read` repro
/// (fragment reads `globals.exposure`, multiplies it into the sampled
/// color) but the vertex shader generates the fullscreen-triangle position
/// with bit tricks on `vertex_index` instead of indexing a local
/// `array<vec2<f32>, 3>` constant table. Isolates whether *dynamically
/// indexing a small vec2 constant array by `@builtin(vertex_index)`*, not
/// the uniform read itself, is what naga/the driver mishandles.
const TONE_BISECT_NO_ARRAY_INDEX_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32(i32(index << 1u) & 2) * 2.0 - 1.0;
    let y = f32(i32(index) & 2) * 2.0 - 1.0;
    var out: VsOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x, y) * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(linear * globals.exposure, 1.0);
}
"#;

/// `tone-real-fix`: the REAL, byte-identical `tonemap.wgsl` fragment body
/// (uniform-driven `if`/early-return, `aces_fitted` helper, full 3-binding
/// layout) with ONLY the vertex shader's `array<vec2<f32>,3>` dynamic
/// indexing replaced by the bit-trick fullscreen-triangle formula that
/// `tone-bisect-no-array-index` proved avoids the crash. This is the
/// candidate fix for `create_tone_pipeline` — if it builds clean, the fix is
/// real and can be applied to `shaders/tonemap.wgsl` verbatim.
const TONE_REAL_FIX_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let x = f32(i32(index << 1u) & 2) * 2.0 - 1.0;
    let y = f32(i32(index) & 2) * 2.0 - 1.0;
    var out: VsOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>(x, y) * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    if globals.debug_passthrough > 0.5 {
        return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    return vec4<f32>(aces_fitted(linear * globals.exposure), 1.0);
}
"#;

/// `tone-uniform-separate-group`: the exact minimal repro from
/// `tone-bisect-uniform-read` (texture-sample result multiplied by a uniform
/// scalar) but with the uniform buffer moved to its OWN bind group
/// (`@group(1)`) instead of sharing `@group(0)` with the texture+sampler.
/// Tests a real, deployable structural workaround: does *separating* the
/// uniform from the texture/sampler descriptor set avoid the crash, with the
/// same shader math otherwise?
fn build_tone_uniform_separate_group(ctx: &RenderContext) {
    let wgsl = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(1) @binding(0) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(linear * globals.exposure, 1.0);
}
"#;
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("probe-tone-separate-group"),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
    let bgl0 = ctx
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe-tone-separate-bgl0"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
    let bgl1 = ctx
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe-tone-separate-bgl1"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
    let layout = ctx
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe-tone-separate-layout"),
            bind_group_layouts: &[&bgl0, &bgl1],
            push_constant_ranges: &[],
        });
    mark("  vkCreateGraphicsPipelines(tone-uniform-separate-group)...");
    let _pipeline = ctx
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("probe-tone-separate-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });
    ctx.wait();
    mark("  tone-uniform-separate-group: pipeline OK (no crash)");
}

fn case_wgsl(name: &str) -> Option<String> {
    Some(match name {
        "no-sample" => case_shader(true, false, false),
        "sample-no-varying" => case_shader(false, true, false),
        "varying+sample" => case_shader(true, true, false),
        "varying+sample-level" => case_shader(true, true, true),
        _ => return None,
    })
}

/// ENG-60 increment 2 isolation: `ScenePipeline::new`'s fine-grained probe
/// markers (added to `spall_render::pipeline`/`spall_render::indirect`) prove
/// the crash is specifically `create_tone_pipeline` — the 3 T13/T14 compute
/// pipelines and the opaque/shadow render pipelines all complete first, every
/// time, regardless of build order. These `tone-*` cases isolate exactly what
/// about that one pipeline differs from the already-cleared
/// `varying+sample`/`varying+sample-level` cases above: a **3rd binding
/// (a uniform buffer) sharing a bind group with the texture+sampler**, and an
/// `if`-branch in the fragment shader that returns early. Build a standalone
/// pipeline (no `ScenePipeline`, no prior pipelines at all) for each variant
/// and see which one alone reproduces the crash.
fn build_tone_variant(ctx: &RenderContext, label: &str, wgsl: &str, with_uniform: bool) {
    let shader = ctx
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(wgsl.into()),
        });
    let mut entries = vec![
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
    ];
    if with_uniform {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        });
    }
    let bgl = ctx
        .device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("probe-tone-bgl"),
            entries: &entries,
        });
    let layout = ctx
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("probe-tone-layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
    mark(&format!(
        "  vkCreateGraphicsPipelines(tone-variant:{label})..."
    ));
    let _pipeline = ctx
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("probe-tone-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview: None,
            cache: None,
        });
    ctx.wait();
    mark(&format!("  tone-variant:{label}: pipeline OK (no crash)"));
}

/// `tone-real`: byte-identical to `create_tone_pipeline` + `tonemap.wgsl` —
/// same bind group shape, same shader text — but standalone, first pipeline
/// built, nothing else touched. Expected to reproduce the crash alone.
const TONE_REAL_WGSL: &str = include_str!("../src/shaders/tonemap.wgsl");

/// `tone-no-uniform`: drop binding 2 (the uniform buffer) and hardcode the
/// exposure/debug values the shader used to read from it. Same texture
/// sample + `if` branch + helper function otherwise.
const TONE_NO_UNIFORM_WGSL: &str = r#"
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    if false {
        return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    return vec4<f32>(aces_fitted(linear * 1.0), 1.0);
}
"#;

/// `tone-uniform-unused`: keep the 3-binding layout (texture+sampler+uniform
/// in one group) but never read the uniform buffer's contents or branch on
/// it — isolates whether the extra *binding* alone (independent of the
/// shader using it) is the trigger.
const TONE_UNIFORM_UNUSED_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// `tone-bisect-clamp`: byte-identical to the already-cleared
/// `case:varying+sample` (2-binding texture+sampler layout, no uniform, no
/// helper function, no branch) except the fragment shader wraps the sampled
/// color in `clamp(..., vec3(0.0), vec3(1.0))` before returning it — the one
/// operation every crashing `tone-*` variant's reachable code path performs
/// and the one `case:*` shader from increment 1 never tried.
const TONE_BISECT_CLAMP_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let tri = array<vec2<f32>,3>(vec2(-1.0,-3.0), vec2(-1.0,1.0), vec2(3.0,1.0));
    let p = tri[i];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * 0.5 + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(clamp(textureSample(tex, samp, uv).rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// `tone-bisect-extra-binding`: byte-identical to `case:varying+sample`
/// (2-binding shader body: plain `textureSample`, no `clamp`, no helper
/// function, no branch) except the bind group layout AND the shader both
/// declare a 3rd binding — a uniform buffer — that is never read. Isolates
/// whether the unused 3rd (uniform-buffer) binding sharing a bind group with
/// the texture+sampler is, by itself, sufficient.
const TONE_BISECT_EXTRA_BINDING_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let tri = array<vec2<f32>,3>(vec2(-1.0,-3.0), vec2(-1.0,1.0), vec2(3.0,1.0));
    let p = tri[i];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * 0.5 + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0);
}
"#;

/// `tone-bisect-combo`: `case:varying+sample`'s exact vertex shader (original
/// triangle constants and symmetric UV scale, not the tone-map ones) plus
/// BOTH the unused 3rd uniform binding AND the `clamp()` call — the two
/// ingredients that were each independently insufficient
/// (`tone-bisect-clamp`, `tone-bisect-extra-binding`) — to test whether it is
/// their combination that is sufficient.
const TONE_BISECT_COMBO_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let tri = array<vec2<f32>,3>(vec2(-1.0,-3.0), vec2(-1.0,1.0), vec2(3.0,1.0));
    let p = tri[i];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * 0.5 + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(clamp(textureSample(tex, samp, uv).rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// `tone-bisect-vertex`: `case:varying+sample`'s exact 2-binding shader body
/// (no clamp, no 3rd binding) but with the tone-map pipeline's own vertex
/// shader constants: triangle `(-1,-1)(3,-1)(-1,3)` (vs. the case shader's
/// `(-1,-3)(-1,1)(3,1)`) and an **asymmetric** UV scale `vec2(0.5,-0.5)` (a
/// V-flip) instead of the case shader's uniform `0.5`. Isolates whether the
/// vertex-stage constants/UV computation — not the fragment body — matter.
const TONE_BISECT_VERTEX_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0);
}
"#;

/// `tone-bisect-vertex-symmetric-uv`: identical to `tone-bisect-vertex`
/// (which crashed) except the UV scale is reverted to the symmetric scalar
/// `0.5` (no V-flip) — isolates the triangle constants from the asymmetric
/// `vec2(0.5, -0.5)` multiply.
const TONE_BISECT_VERTEX_SYMMETRIC_UV_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * 0.5 + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0);
}
"#;

/// `tone-bisect-triangle-only`: identical to `case:varying+sample` (original
/// triangle constants) except the UV scale is the asymmetric
/// `vec2(0.5, -0.5)` V-flip — isolates the UV multiply from the triangle
/// constants (the inverse of `tone-bisect-vertex-symmetric-uv`).
const TONE_BISECT_TRIANGLE_ONLY_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let tri = array<vec2<f32>,3>(vec2(-1.0,-3.0), vec2(-1.0,1.0), vec2(3.0,1.0));
    let p = tri[i];
    var out: VsOut;
    out.position = vec4<f32>(p, 0.0, 1.0);
    out.uv = p * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0);
}
"#;

/// `tone-bisect-dedup-scaled`: same shape as `tone-bisect-vertex-symmetric-uv`
/// (which crashed) — a 2-distinct-value, 4x/2x-repeated triangle constant
/// table — but with different literal magnitudes (`-2.0`/`4.0` instead of
/// `-1.0`/`3.0`). Tests whether the trigger is the *pattern* of repeated
/// constants in the array (a plausible SPIR-V constant-dedup compiler bug)
/// or the specific bit values `-1.0`/`3.0`.
const TONE_BISECT_DEDUP_SCALED_WGSL: &str = r#"
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-2.0, -2.0), vec2<f32>(4.0, -2.0), vec2<f32>(-2.0, 4.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * 0.5 + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let uv = in.uv;
    return vec4<f32>(textureSample(tex, samp, uv).rgb, 1.0);
}
"#;

/// `tone-fix-candidate`: byte-identical to `tone-real` (real `tonemap.wgsl`
/// fragment body: uniform-driven `if`/early-return, `aces_fitted` helper,
/// full 3-binding layout) except the vertex shader's fullscreen-triangle
/// constant table is swapped for the already-proven-safe
/// `(-1,-3),(-1,1),(3,1)` values (same NDC coverage, same interpolated `uv`
/// — see the module doc comment). If this alone stops crashing, it is the
/// candidate fix for `create_tone_pipeline`.
const TONE_FIX_CANDIDATE_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    if globals.debug_passthrough > 0.5 {
        return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    return vec4<f32>(aces_fitted(linear * globals.exposure), 1.0);
}
"#;

/// `tone-no-branch-safe-triangle`: `tone-no-branch` (helper fn + uniform
/// `exposure` multiply actually used, NO `if` branch, real 3-binding layout)
/// with the vertex triangle constants swapped for the proven-safe values.
/// Tests whether the branch specifically was required, or whether
/// helper-fn + uniform-buffer-read alone still crashes regardless of the
/// triangle table.
const TONE_NO_BRANCH_SAFE_TRIANGLE_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(aces_fitted(linear * globals.exposure) + vec3<f32>(globals.debug_passthrough * 0.0), 1.0);
}
"#;

/// `tone-branch-no-helper-safe-triangle`: real 3-binding layout + safe
/// triangle constants + the uniform-driven `if`/early-return branch, but the
/// ACES tonemap is inlined (no separate `aces_fitted` function call). Tests
/// whether the branch alone (without a helper-function call) still crashes
/// with the safe triangle.
const TONE_BRANCH_NO_HELPER_SAFE_TRIANGLE_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    if globals.debug_passthrough > 0.5 {
        return vec4<f32>(clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    let x = linear * globals.exposure;
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return vec4<f32>(clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// `tone-branchless-fix-candidate`: real 3-binding layout, real
/// uniform-driven passthrough/tonemap choice, safe triangle constants — but
/// with NO separate helper function and NO `if`/early-return. The
/// passthrough-vs-tonemap choice is expressed with `select()` (compiles to a
/// single `OpSelect`, not a conditional branch) and the ACES math is inlined.
/// Same observable behaviour as the real shader (same result for
/// `debug_passthrough` on/off), no branch, no function call.
const TONE_BRANCHLESS_FIX_CANDIDATE_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    let passthrough = clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0));
    let x = linear * globals.exposure;
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    let toned = clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
    let result = select(toned, passthrough, globals.debug_passthrough > 0.5);
    return vec4<f32>(result, 1.0);
}
"#;

/// `tone-bisect-uniform-read`: minimal fragment body that actually *reads*
/// one uniform field (`globals.exposure`) and uses it, vs.
/// `tone-bisect-extra-binding`'s declared-but-unread uniform. Real
/// 3-binding layout, safe triangle. Isolates whether *reading* the uniform
/// buffer's contents in the fragment shader (as opposed to merely declaring
/// the binding) is the trigger, independent of branches or helper functions.
const TONE_BISECT_UNIFORM_READ_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -3.0), vec2<f32>(-1.0, 1.0), vec2<f32>(3.0, 1.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(linear * globals.exposure, 1.0);
}
"#;

/// `tone-no-branch`: keep the uniform buffer, read it, but remove the
/// `if`-with-early-return — always compute the ACES path.
const TONE_NO_BRANCH_WGSL: &str = r#"
struct ToneGlobals { exposure: f32, debug_passthrough: f32, _pad: vec2<f32>, };
@group(0) @binding(0) var hdr: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(0) @binding(2) var<uniform> globals: ToneGlobals;
struct VsOut { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var out: VsOut;
    out.position = vec4<f32>(p[index], 0.0, 1.0);
    out.uv = p[index] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5);
    return out;
}
fn aces_fitted(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51; let b = 0.03; let c = 2.43; let d = 0.59; let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}
@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let linear = textureSample(hdr, linear_sampler, in.uv).rgb;
    return vec4<f32>(aces_fitted(linear * globals.exposure) + vec3<f32>(globals.debug_passthrough * 0.0), 1.0);
}
"#;

fn main() {
    let backends = backends_from_env();
    let mode = std::env::args().nth(1).unwrap_or_else(|| "full".into());
    mark(&format!("probe start; backends={backends:?} mode={mode}"));

    if mode == "spirv" {
        dump_spirv("opaque", include_str!("../src/shaders/opaque.wgsl"));
        dump_spirv("tonemap", include_str!("../src/shaders/tonemap.wgsl"));
        dump_spirv("no-sample", &case_shader(true, false, false));
        dump_spirv("sample-no-varying", &case_shader(false, true, false));
        dump_spirv("varying+sample", &case_shader(true, true, false));
        // ENG-60 increment 2: the two minimal vertex-shader variants that
        // pin the trigger down to the fullscreen-triangle constant table —
        // one crashes, one does not, with byte-identical fragment shaders.
        dump_spirv("bisect-vertex-crash", TONE_BISECT_VERTEX_WGSL);
        dump_spirv("bisect-triangle-only-ok", TONE_BISECT_TRIANGLE_ONLY_WGSL);
        return;
    }

    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends,
        ..Default::default()
    });
    mark("enumerate_adapters");
    print_adapters(&instance, backends);

    mark("RenderContext::headless");
    let ctx = match RenderContext::headless() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("no usable adapter: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "  selected adapter={:?} backend={:?} gpu_timestamps={}",
        ctx.adapter_name(),
        ctx.backend(),
        ctx.supports_gpu_timestamps()
    );
    let _ = std::io::stdout().flush();

    if mode == "info" {
        return;
    }

    if let Some(stripped) = mode.strip_prefix("case:") {
        if stripped == "pipelines" {
            mark("ScenePipeline::new (naga SPIR-V + vkCreateGraphicsPipelines x3)");
            let _p = ScenePipeline::new(&ctx.device);
            ctx.wait();
            mark("  pipelines built OK (no crash)");
            return;
        }
        match stripped {
            "tone-real" => {
                build_tone_variant(&ctx, "tone-real", TONE_REAL_WGSL, true);
                return;
            }
            "tone-no-uniform" => {
                build_tone_variant(&ctx, "tone-no-uniform", TONE_NO_UNIFORM_WGSL, false);
                return;
            }
            "tone-uniform-unused" => {
                build_tone_variant(&ctx, "tone-uniform-unused", TONE_UNIFORM_UNUSED_WGSL, true);
                return;
            }
            "tone-no-branch" => {
                build_tone_variant(&ctx, "tone-no-branch", TONE_NO_BRANCH_WGSL, true);
                return;
            }
            "tone-bisect-clamp" => {
                build_tone_variant(&ctx, "tone-bisect-clamp", TONE_BISECT_CLAMP_WGSL, false);
                return;
            }
            "tone-bisect-extra-binding" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-extra-binding",
                    TONE_BISECT_EXTRA_BINDING_WGSL,
                    true,
                );
                return;
            }
            "tone-bisect-combo" => {
                build_tone_variant(&ctx, "tone-bisect-combo", TONE_BISECT_COMBO_WGSL, true);
                return;
            }
            "tone-bisect-vertex" => {
                build_tone_variant(&ctx, "tone-bisect-vertex", TONE_BISECT_VERTEX_WGSL, false);
                return;
            }
            "tone-bisect-vertex-symmetric-uv" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-vertex-symmetric-uv",
                    TONE_BISECT_VERTEX_SYMMETRIC_UV_WGSL,
                    false,
                );
                return;
            }
            "tone-bisect-triangle-only" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-triangle-only",
                    TONE_BISECT_TRIANGLE_ONLY_WGSL,
                    false,
                );
                return;
            }
            "tone-bisect-dedup-scaled" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-dedup-scaled",
                    TONE_BISECT_DEDUP_SCALED_WGSL,
                    false,
                );
                return;
            }
            "tone-fix-candidate" => {
                build_tone_variant(&ctx, "tone-fix-candidate", TONE_FIX_CANDIDATE_WGSL, true);
                return;
            }
            "tone-no-branch-safe-triangle" => {
                build_tone_variant(
                    &ctx,
                    "tone-no-branch-safe-triangle",
                    TONE_NO_BRANCH_SAFE_TRIANGLE_WGSL,
                    true,
                );
                return;
            }
            "tone-branch-no-helper-safe-triangle" => {
                build_tone_variant(
                    &ctx,
                    "tone-branch-no-helper-safe-triangle",
                    TONE_BRANCH_NO_HELPER_SAFE_TRIANGLE_WGSL,
                    true,
                );
                return;
            }
            "tone-branchless-fix-candidate" => {
                build_tone_variant(
                    &ctx,
                    "tone-branchless-fix-candidate",
                    TONE_BRANCHLESS_FIX_CANDIDATE_WGSL,
                    true,
                );
                return;
            }
            "tone-bisect-uniform-read" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-uniform-read",
                    TONE_BISECT_UNIFORM_READ_WGSL,
                    true,
                );
                return;
            }
            "tone-uniform-separate-group" => {
                build_tone_uniform_separate_group(&ctx);
                return;
            }
            "tone-bisect-unorm-format" => {
                build_one_pipeline_fmt(
                    &ctx,
                    "tone-bisect-unorm-format",
                    TONE_BISECT_UNIFORM_READ_WGSL,
                    wgpu::TextureFormat::Rgba8Unorm,
                );
                return;
            }
            "tone-bisect-srgb-format" => {
                build_one_pipeline_fmt(
                    &ctx,
                    "tone-bisect-srgb-format",
                    TONE_BISECT_UNIFORM_READ_WGSL,
                    wgpu::TextureFormat::Rgba8UnormSrgb,
                );
                return;
            }
            "tone-bisect-no-array-index" => {
                build_tone_variant(
                    &ctx,
                    "tone-bisect-no-array-index",
                    TONE_BISECT_NO_ARRAY_INDEX_WGSL,
                    true,
                );
                return;
            }
            "tone-real-fix" => {
                build_tone_variant(&ctx, "tone-real-fix", TONE_REAL_FIX_WGSL, true);
                return;
            }
            _ => {}
        }
        match case_wgsl(stripped) {
            Some(wgsl) => build_one_pipeline(&ctx, stripped, &wgsl),
            None => {
                eprintln!("unknown case {stripped:?}");
                std::process::exit(2);
            }
        }
        return;
    }

    // `capture[:WxH]` runs the real six-view capture_scene() and prints timings —
    // the acceptance capture for ENG-60. Defaults to 1920x1080.
    if let Some(rest) = mode.strip_prefix("capture") {
        let (w, h) = rest
            .strip_prefix(':')
            .and_then(|d| d.split_once('x'))
            .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
            .unwrap_or((1920u32, 1080u32));
        let shapes = acceptance_shapes();
        let cube = shapes.iter().find(|s| s.name == "cube").unwrap();
        let mesh = mesh_shape(&cube.volume, MeshStrategy::Greedy).mesh;
        let mut scene = Scene::new(Camera {
            aspect: w as f32 / h as f32,
            ..Default::default()
        })
        .with_item(SceneItem::new(
            "ground",
            mesh.clone(),
            Mat4::from_scale(Vec3::new(4.0, 0.2, 4.0)),
        ))
        .with_item(SceneItem::new(
            "moving-body",
            mesh,
            Mat4::from_translation(Vec3::new(0.4, 1.4, 0.2)) * Mat4::from_rotation_y(0.55),
        ));
        scene.frame_all(Vec3::new(1.1, 0.8, 1.2));
        let dir = std::env::temp_dir().join(format!("spall-eng60-capture-{}x{}", w, h));
        mark(&format!(
            "capture_scene {w}x{h} six views -> {}",
            dir.display()
        ));
        let report = capture_scene(
            &ctx,
            &scene,
            &dir,
            &CaptureOptions {
                width: w,
                height: h,
                ..Default::default()
            },
        )
        .expect("capture_scene");
        println!(
            "  adapter={:?} backend={} images={} items_drawn={} triangles={}",
            report.adapter,
            report.backend,
            report.images.len(),
            report.items_drawn,
            report.triangles
        );
        println!(
            "  cpu_total={:.2}ms readback={:.2}ms encode={:.2}ms",
            report.timing.cpu_total_millis,
            report.timing.cpu_readback_millis,
            report.timing.cpu_encode_millis
        );
        println!(
            "  gpu_render={:?}ms gpu_passes={:?}",
            report.timing.gpu_render_millis, report.timing.gpu_passes
        );
        mark("capture done (clean)");
        return;
    }

    if mode != "full" {
        eprintln!("unknown mode {mode:?}");
        std::process::exit(2);
    }

    // ---- full offscreen capture path (matches tests/capture_gpu.rs) --------
    let shapes = acceptance_shapes();
    let cube = shapes.iter().find(|s| s.name == "cube").unwrap();
    let mesh = mesh_shape(&cube.volume, MeshStrategy::Greedy).mesh;
    let mut scene = Scene::new(Camera {
        aspect: 1.0,
        ..Default::default()
    })
    .with_item(SceneItem::new(
        "ground",
        mesh.clone(),
        Mat4::from_scale(Vec3::new(4.0, 0.2, 4.0)),
    ))
    .with_item(SceneItem::new(
        "moving-body",
        mesh,
        Mat4::from_translation(Vec3::new(0.4, 1.4, 0.2)) * Mat4::from_rotation_y(0.55),
    ));
    scene.frame_all(Vec3::new(1.1, 0.8, 1.2));

    mark("ScenePipeline::new (naga SPIR-V + vkCreateGraphicsPipelines x3)");
    let pipeline = ScenePipeline::new(&ctx.device);
    ctx.wait();
    mark("  pipelines built OK");

    let materials = pipeline.material_buffer(&ctx.device, &scene.materials);
    let target = OffscreenTarget::new(&ctx.device, 256, 256);

    mark("upload meshes");
    let mut draws: Vec<GpuMesh> = Vec::new();
    for item in &scene.items {
        let (v, idx) = to_gpu(&item.mesh, item.model);
        draws.push(GpuMesh::create(&ctx.device, &v, &idx, UploadBudget::default()).unwrap());
    }
    ctx.wait();
    mark("  meshes uploaded OK");

    let sun_dir = Vec3::new(-0.4, -0.82, -0.4).normalize();
    let (light_matrices, _) = ScenePipeline::cascade_data(&scene.camera, sun_dir);

    mark("shadow cascades: raster into depth-array layers");
    let mut enc = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("probe-shadow"),
        });
    for (cascade, matrix) in light_matrices.into_iter().enumerate() {
        let bind = pipeline.shadow_bind_group(&ctx.device, &ctx.queue, cascade, matrix);
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("probe-shadow-pass"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: pipeline.shadow_layer(cascade),
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline.shadow());
        pass.set_bind_group(0, &bind, &[]);
        for g in &draws {
            pass.set_vertex_buffer(0, g.vertex_buffer.slice(..));
            pass.set_index_buffer(g.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..g.index_count, 0, 0..1);
        }
    }
    ctx.queue.submit([enc.finish()]);
    ctx.wait();
    mark("  shadow raster OK");

    mark("opaque pass: build scene bind group + draw + sample depth array");
    let scene_bind = pipeline.scene_bind_group(
        &ctx.device,
        &ctx.queue,
        &scene.camera,
        DebugView::ShadowCascades,
        1.0,
        &materials,
    );
    let mut enc = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("probe-opaque"),
        });
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("probe-hdr-opaque-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.hdr_view(),
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: target.depth_view(),
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline.opaque());
        pass.set_bind_group(0, &scene_bind, &[]);
        for g in &draws {
            pass.set_vertex_buffer(0, g.vertex_buffer.slice(..));
            pass.set_index_buffer(g.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..g.index_count, 0, 0..1);
        }
    }
    ctx.queue.submit([enc.finish()]);
    ctx.wait();
    mark("  opaque sample OK");
    mark("probe end (clean)");
    let _ = CASCADE_COUNT;
}
