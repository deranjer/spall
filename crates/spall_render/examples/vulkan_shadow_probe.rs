//! ENG-60 diagnostic probe: the pinned wgpu 24 Vulkan backend crashes natively
//! (`STATUS_ACCESS_VIOLATION`, 0xC0000005) on the recorded NVIDIA Windows driver
//! while the T12 renderer's pipelines are compiled. D3D12 with the identical
//! shaders is unaffected.
//!
//! This binary prints full adapter/driver identification, then builds a series
//! of isolated one-pipeline cases that pin the trigger, and finally the full
//! offscreen capture path. Every step is committed to a synced log file
//! (`SPALL_PROBE_LOG`) before it runs, so a hard native crash still names the
//! GPU call that faulted.
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
//! case:<name> — the synthetic single-pipeline builds all compile fine on the
//! recorded driver; they bound the fault (a plain varying + texture-sample
//! fragment pipeline is NOT enough to trigger it). `case:pipelines` builds the
//! real T12 pipelines and is the minimal crash:
//!   no-sample             fragment varying, NO texture sample            (ok)
//!   sample-no-varying     texture sample, coord from @builtin(position)  (ok)
//!   varying+sample        fragment varying used as textureSample coord   (ok)
//!   varying+sample-level  ... textureSampleLevel (explicit LOD)          (ok)
//!   pipelines             real ScenePipeline::new() (opaque+shadow+tone) CRASH on Vulkan

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

fn case_wgsl(name: &str) -> Option<String> {
    Some(match name {
        "no-sample" => case_shader(true, false, false),
        "sample-no-varying" => case_shader(false, true, false),
        "varying+sample" => case_shader(true, true, false),
        "varying+sample-level" => case_shader(true, true, true),
        _ => return None,
    })
}

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
