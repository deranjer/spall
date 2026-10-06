#![cfg(feature = "client")]
//! ENG-102: correlated, single-clock edit-to-presented lighting latency.
//!
//! A real QUIC server hosts `fixtures/lighting-room`'s sealed house; a real,
//! non-scripted "actor" client cuts a hole in its roof; a real "observer"
//! client (the production network/replica code, driven through the exact
//! [`spall_client::window::rebuild_pass`] the interactive window uses, not a
//! re-implementation) receives the change and its lighting cache lights the
//! new opening. Every stage below is timestamped on **one process's one
//! monotonic clock** (server, both clients and the renderer all run as
//! threads of this test binary) — server/client clock alignment across a real
//! network deployment is a different, harder problem this does not model; see
//! `docs/reports/ENG-102.md`.
//!
//! Run: `cargo test -p sandbox --features client --test lighting_latency -- --nocapture`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sandbox::editor_scene;
use sandbox::game::{self, materials};
use spall_client::replica::ReplicaWorld;
use spall_client::window::{TERRAIN_RECHECK_INTERVAL, rebuild_pass};
use spall_client::{BaselineScene, ClientNetConfig, ScriptTarget, ScriptedAction, cut_request};
use spall_core::GlobalCell;
use spall_net::{Fingerprint, JoinToken, TransportConfig};
use spall_render::{
    Camera, DebugView, EnvironmentPreset, GameRenderer, OffscreenTarget, RenderContext,
};
use spall_server::{Scene, ServeConfig};
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;

const SIZE: (u32, u32) = (480, 360);

fn lighting_room() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/lighting-room"
    ))
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spall-lighting-latency-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(path: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text.trim().to_owned();
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "waiting for {path:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn material_at(sim: &Simulation, cell: GlobalCell) -> Sample {
    sim.world()
        .volume_ref(sim.world().terrain_volume_id())
        .unwrap()
        .sample(cell)
        .unwrap()
}

/// The sealed house's roof cell and the interior floor cell directly below it,
/// found by scanning the actual simulated world rather than hand-derived
/// coordinates (checked in `sealed_house_geometry_is_where_this_test_assumes`).
fn sealed_house_probe(sim: &Simulation) -> (GlobalCell, GlobalCell) {
    // Search outward from the house's interior centre first, so the probe
    // column sits well clear of the walls (a robust camera frame), falling
    // back to the wider scan if the geometry ever shifts.
    let center = (29_i64, 37_i64);
    for radius in 0_i64..10 {
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                if dx.abs() != radius && dz.abs() != radius {
                    continue;
                }
                let (x, z) = (center.0 + dx, center.1 + dz);
                if let Sample::Filled(m) = material_at(sim, GlobalCell::new(x, 14, z))
                    && m.0 == 13
                    && matches!(
                        material_at(sim, GlobalCell::new(x, 2, z)),
                        Sample::Empty { .. }
                    )
                    && matches!(
                        material_at(sim, GlobalCell::new(x, 1, z)),
                        Sample::Filled(_)
                    )
                {
                    return (GlobalCell::new(x, 14, z), GlobalCell::new(x, 2, z));
                }
            }
        }
    }
    for x in 20..38 {
        for z in 28..46 {
            if let Sample::Filled(m) = material_at(sim, GlobalCell::new(x, 14, z))
                && m.0 == 13 // sandstone (roof), see editor_scene::material_mapping
                && matches!(material_at(sim, GlobalCell::new(x, 2, z)), Sample::Empty { .. })
                && matches!(material_at(sim, GlobalCell::new(x, 1, z)), Sample::Filled(_))
            {
                return (GlobalCell::new(x, 14, z), GlobalCell::new(x, 2, z));
            }
        }
    }
    panic!("sealed house roof/interior not found at its expected location");
}

#[test]
fn sealed_house_geometry_is_where_this_test_assumes() {
    let scene = editor_scene::load(lighting_room()).expect("scene loads");
    let sim = Simulation::new(SimulationConfig::new(scene.world_setup())).expect("world");
    let (roof, floor) = sealed_house_probe(&sim);
    println!("sealed house: roof {roof:?}, interior floor {floor:?}");
    // Every other cell in the interior column between them is air (truly sealed
    // but for the roof we are about to cut).
    for y in (floor.y + 1)..roof.y {
        assert!(
            matches!(
                material_at(&sim, GlobalCell::new(roof.x, y, roof.z)),
                Sample::Empty { .. }
            ),
            "column ({}, {y}, {}) should be hollow interior",
            roof.x,
            roof.z
        );
    }
}

/// A `Camera` inside the sealed house, near one interior corner at eye
/// height, looking across the room at the floor patch under the roof hole --
/// the same "stand inside, look at the lit patch" framing
/// `sky_visibility_gpu.rs`'s room tests use, rather than trying to squint
/// straight down through a small hole from outside (a much narrower, harder
/// to aim sightline).
fn hole_camera(probe_m: glam::Vec3) -> Camera {
    let eye = glam::Vec3::new(probe_m.x - 2.875, probe_m.y + 1.075, probe_m.z - 2.875);
    Camera::looking_along(
        eye,
        probe_m - eye,
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    )
}

fn srgb_to_linear(byte: u8) -> f32 {
    let c = f32::from(byte) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luma_at(camera: &Camera, rgba: &[u8], p: glam::Vec3) -> f32 {
    let ndc = camera.project(p).expect("in front of the camera");
    let x = (((ndc[0] * 0.5 + 0.5) * SIZE.0 as f32) as u32).min(SIZE.0 - 1);
    let y = (((0.5 - ndc[1] * 0.5) * SIZE.1 as f32) as u32).min(SIZE.1 - 1);
    let i = ((y * SIZE.0 + x) * 4) as usize;
    srgb_to_linear(rgba[i + 1])
}

/// Stage timestamps for one edit, all on this process's monotonic clock.
#[derive(Default)]
struct Stages {
    committed: Option<Instant>,
    client_receipt: Option<Instant>,
    rebuild_dispatched: Option<Instant>,
    rebuild_done: Option<Instant>,
    upload_done: Option<Instant>,
    lit: Option<Instant>,
    presented: Option<Instant>,
}

fn ms(a: Instant, b: Instant) -> f64 {
    b.saturating_duration_since(a).as_secs_f64() * 1000.0
}

fn report(stages: &Stages, label: &str) {
    let c = stages.committed.expect("commit recorded");
    let r = stages.client_receipt.expect("receipt recorded");
    let d = stages.rebuild_dispatched.expect("dispatch recorded");
    let b = stages.rebuild_done.expect("rebuild recorded");
    let u = stages.upload_done.expect("upload recorded");
    let l = stages.lit.expect("lit recorded");
    let p = stages.presented.expect("presented recorded");
    println!(
        "{label}: commit->receipt {:.1} ms | receipt->dispatch (recheck wait) {:.1} ms | dispatch->rebuilt {:.1} ms | rebuild->uploaded {:.1} ms | uploaded->lit (sweep) {:.1} ms | lit->presented {:.1} ms || TOTAL commit->presented {:.1} ms",
        ms(c, r),
        ms(r, d),
        ms(d, b),
        ms(b, u),
        ms(u, l),
        ms(l, p),
        ms(c, p)
    );
}

/// Runs the server + actor + observer, drives the observer's real
/// `rebuild_pass`/lighting pipeline by hand (no window, so every stage can be
/// timestamped), and returns the filled-in stage clock. `stationary` selects
/// whether the observer's simulated position moves during the wait for
/// `rebuild_dispatched` -- `false` exercises the `TERRAIN_RECHECK_INTERVAL`
/// (500 ms) polling path (a nearby edit with no player movement to trigger an
/// immediate rebuild); `true` exercises the `moved_far_enough` path (typical
/// case: the player is walking, so the very next simulated frame dispatches).
fn measure(stationary: bool) -> Stages {
    let scene = editor_scene::load(lighting_room()).expect("scene loads");
    let sim = Simulation::new(SimulationConfig::new(scene.world_setup())).expect("world");
    let (roof, floor) = sealed_house_probe(&sim);
    let probe_m = glam::Vec3::new(
        (floor.x as f32 + 0.5) * 0.25,
        (floor.y as f32 + 0.5) * 0.25,
        (floor.z as f32 + 0.5) * 0.25,
    );

    // GPU context, renderer and the sealed (pre-edit, roof intact) baseline are
    // all built and settled *before* the server or any client starts -- a real
    // client's GPU device and first frame exist long before any edit happens,
    // and including that one-time setup cost in the edit-latency figures below
    // would be dishonest (device/pipeline creation, not edit responsiveness).
    let ctx = RenderContext::headless().expect("a GPU adapter");
    let materials = spall_render::materials_from_manifest(&game::manifest());
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        SIZE,
        None,
    );
    let mut target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_intensity = 0.0; // isolate skylight-through-the-hole, not direct sun
    let camera = hole_camera(probe_m);
    let frame =
        |renderer: &mut GameRenderer, target: &mut OffscreenTarget, view: DebugView| -> Vec<u8> {
            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
            renderer.render(
                &ctx.device,
                &ctx.queue,
                &mut encoder,
                target.color_view(),
                &camera,
                &environment,
                view,
            );
            target.copy_to_readback(&mut encoder);
            ctx.queue.submit([encoder.finish()]);
            target.read_rgba(&ctx).expect("readback")
        };
    let sealed_volume = sim
        .world()
        .volume_ref(sim.world().terrain_volume_id())
        .expect("terrain volume");
    let sealed_instances = spall_client::window::build_instances(
        sealed_volume,
        [
            f64::from(probe_m.x),
            f64::from(probe_m.y),
            f64::from(probe_m.z),
        ],
    );
    let (sealed_sky, _) = spall_client::sky::build_sky_occupancy(
        sealed_volume,
        [
            f64::from(probe_m.x),
            f64::from(probe_m.y),
            f64::from(probe_m.z),
        ],
        true,
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &sealed_instances);
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&sealed_sky));
    let mut sealed_frame = frame(&mut renderer, &mut target, DebugView::Shaded);
    while renderer.lighting_is_sweeping() {
        sealed_frame = frame(&mut renderer, &mut target, DebugView::Shaded);
    }
    let sealed_luma = luma_at(&camera, &sealed_frame, probe_m);

    let dir = scratch(if stationary { "stationary" } else { "moving" });
    let token = JoinToken::generate().unwrap();
    let mut config = ServeConfig::headless("127.0.0.1:0".parse().unwrap(), Scene::Custom, token);
    config.custom_world = Some(scene.into_custom_world());
    config.max_ticks = 400;
    config.quiescence_ticks = 0;
    config.min_clients = 1;
    config.max_clients = 4;
    config.paced = true;
    config.startup_timeout = Duration::from_secs(30);
    config.log_json = dir.join("server.jsonl");
    config.fingerprint_out = Some(dir.join("server.fingerprint"));
    config.addr_out = Some(dir.join("server.addr"));
    config.transport = TransportConfig::for_tests();
    config.dev_unvalidated_actions = true;

    let committed_at: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let commit_slot = committed_at.clone();
    let commit_handler: spall_server::CommittedEditHandler = Box::new(move |_, _, _, removed| {
        if !removed.is_empty() {
            let mut slot = commit_slot.lock().unwrap();
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
        }
    });
    let server = std::thread::spawn(move || {
        spall_server::serve_with_game_content_and_commit_handler(
            config,
            game::tool_catalog(),
            game::manifest(),
            game::contact_damage_profiles(),
            None,
            None,
            commit_handler,
        )
    });
    let fingerprint = Fingerprint::from_hex(&wait_for(&dir.join("server.fingerprint"))).unwrap();
    let addr: std::net::SocketAddr = wait_for(&dir.join("server.addr")).parse().unwrap();

    // Observer: connects, receives the baseline, then sits idle -- we drive
    // its lighting pipeline ourselves instead of through a window.
    let observer_slot: Arc<Mutex<Option<Arc<Mutex<ReplicaWorld>>>>> = Arc::new(Mutex::new(None));
    let slot = observer_slot.clone();
    let observer_cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: Vec::new(),
        movement_script: Vec::new(),
        late_join: true,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 380,
        idle_grace: Duration::from_secs(20),
        overall_timeout: Duration::from_secs(60),
        log_json: dir.join("observer.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: Some(Arc::new(move |replica| {
            *slot.lock().unwrap() = Some(replica);
        })),
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    };
    let observer = std::thread::spawn(move || {
        spall_client::run_replication_client_with_manifest(observer_cfg, game::manifest())
    });

    let replica = loop {
        if let Some(r) = observer_slot.lock().unwrap().clone() {
            break r;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let initial_hash = replica.lock().unwrap().terrain_hash();

    // The actor: a separate live client that cuts the roof hole. Scripted at
    // an early tick so the observer's baseline (already installed above) does
    // not include it.
    let actor_cfg = ClientNetConfig {
        connect_addr: addr,
        server_fingerprint: fingerprint,
        join_token: token,
        script: vec![ScriptedAction {
            at_tick: 20,
            request: cut_request(1, 0, [roof.x, roof.y, roof.z], 4),
            target: ScriptTarget::Terrain,
        }],
        movement_script: Vec::new(),
        late_join: false,
        baseline_scene: BaselineScene::BridgeCut,
        run_ticks: 200,
        idle_grace: Duration::from_secs(10),
        overall_timeout: Duration::from_secs(40),
        log_json: dir.join("actor.jsonl"),
        summary_json: None,
        transport: TransportConfig::for_tests(),
        client_residency: None,
        baseline_budget_bytes: spall_client::segmented::DEFAULT_CLIENT_BASELINE_BUDGET_BYTES,
        on_replica_ready: None,
        interactive: None,
        client_authoritative: false,
        admin_script: Vec::new(),
    };
    let actor = std::thread::spawn(move || {
        spall_client::run_replication_client_with_manifest(actor_cfg, game::manifest())
    });

    // Poll for the observer's replica to pick up the change (this is the
    // "client receipt" instant: fine-grained but polled, so it carries this
    // loop's ~0.2 ms sampling error, noted in the report).
    let mut stages = Stages::default();
    let receipt_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let hash = replica.lock().unwrap().terrain_hash();
        if hash != initial_hash {
            stages.client_receipt = Some(Instant::now());
            break;
        }
        assert!(
            Instant::now() < receipt_deadline,
            "the observer never saw the cut land"
        );
        std::thread::sleep(Duration::from_micros(200));
    }
    // The commit handler fires synchronously on the server's authoritative
    // thread strictly before the fan-out the observer just detected, so this
    // is available by now without an extra wait.
    stages.committed = *committed_at.lock().unwrap();

    // Reproduce the interactive window's real dispatch gating
    // (`due_for_recheck` / `moved_far_enough` in `window.rs`'s
    // `RedrawRequested` handler, which is not itself a callable function) at a
    // representative 60 Hz frame cadence, using the exact production
    // `TERRAIN_RECHECK_INTERVAL` constant.
    let mut last_dispatch_at: Option<Instant> =
        Some(stages.client_receipt.unwrap() - Duration::from_millis(1));
    // A rebuild was already "in flight" conceptually before the edit in a
    // live session; approximate the worst case (`stationary`) by starting the
    // recheck clock at receipt itself, and the best case (`!stationary`, the
    // player is moving) by making every frame `moved_far_enough`.
    if stationary {
        last_dispatch_at = Some(stages.client_receipt.unwrap());
    }
    loop {
        let now = Instant::now();
        let due_for_recheck = last_dispatch_at.is_none_or(|t| now - t >= TERRAIN_RECHECK_INTERVAL);
        if !stationary || due_for_recheck {
            stages.rebuild_dispatched = Some(now);
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    let mut sky_anchor = None;
    let mut last_sky = None;
    // `absent_is_open = true`: matches production's own choice
    // (`!net_config_streams_residency` in `window.rs`) for an observer with no
    // `client_residency` streaming configured, same as here.
    let outcome = rebuild_pass(
        &replica,
        [
            f64::from(probe_m.x),
            f64::from(probe_m.y),
            f64::from(probe_m.z),
        ],
        &mut sky_anchor,
        &mut last_sky,
        true,
    )
    .expect("replica has terrain");
    stages.rebuild_done = Some(Instant::now());
    println!(
        "rebuild_pass internal elapsed (volume walk only, excl. lock wait): {:.1} ms",
        outcome.elapsed.as_secs_f64() * 1000.0
    );

    renderer.set_terrain(&ctx.device, &ctx.queue, &outcome.instances);
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, outcome.sky.as_ref());
    stages.upload_done = Some(Instant::now());

    let mut frames = 0;
    let lit_frame = frame(&mut renderer, &mut target, DebugView::Shaded);
    while renderer.lighting_is_sweeping() && frames < 30 {
        let _ = frame(&mut renderer, &mut target, DebugView::Shaded);
        frames += 1;
    }
    let _ = frame(&mut renderer, &mut target, DebugView::Shaded);
    stages.lit = Some(Instant::now());

    // "Presented": the frame after the sweep is done is the first one whose
    // pixels reflect it; render once more to stand in for swapchain present
    // (a real present adds a variable vsync wait this headless path skips --
    // noted in the report, and estimated separately from live sessions).
    let presented_frame = frame(&mut renderer, &mut target, DebugView::Shaded);
    stages.presented = Some(Instant::now());

    let luma = luma_at(&camera, &presented_frame, probe_m);
    println!("probe luma: sealed {sealed_luma:.4} -> lit {luma:.4} ({frames} sweep frames)");
    {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.local/runs/eng-102-latency-diag");
        std::fs::create_dir_all(&dir).unwrap();
        let tag = if stationary { "stat" } else { "move" };
        image::save_buffer(
            dir.join(format!("shaded-{tag}.png")),
            &presented_frame,
            SIZE.0,
            SIZE.1,
            image::ColorType::Rgba8,
        )
        .unwrap();
        let albedo_frame = frame(&mut renderer, &mut target, DebugView::Albedo);
        image::save_buffer(
            dir.join(format!("albedo-{tag}.png")),
            &albedo_frame,
            SIZE.0,
            SIZE.1,
            image::ColorType::Rgba8,
        )
        .unwrap();
        let ndc = camera.project(probe_m);
        println!(
            "probe world {probe_m:?}, ndc {ndc:?}, outcome.sky present {}",
            outcome.sky.is_some()
        );
    }
    assert!(
        luma > sealed_luma * 3.0 && luma > sealed_luma + 0.003,
        "the opened roof must light the interior floor measurably above the sealed baseline: sealed {sealed_luma} vs lit {luma}"
    );
    let _ = lit_frame;
    let _ = &lit_frame;

    let actor_summary = actor.join().unwrap().expect("actor client");
    assert!(actor_summary.transactions_applied >= 1 || actor_summary.actions_sent >= 1);
    // Let the observer and server wind down cleanly.
    let _ = observer.join().unwrap();
    let _ = server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    stages
}

#[test]
fn edit_to_presented_latency_player_moving() {
    let stages = measure(false);
    report(&stages, "player moving (immediate dispatch)");
}

#[test]
fn edit_to_presented_latency_player_stationary() {
    let stages = measure(true);
    report(&stages, "player stationary (recheck-interval wait)");
}

#[allow(dead_code)]
fn touch_materials_import() {
    let _ = materials::LAMP;
}

#[test]
fn diag_minimal_skylight_sanity() {
    use spall_render::{
        CubeInstance, Material, cache_origin_around, indirect::LightingVolume, mark_world_box,
    };
    let ctx = RenderContext::headless().expect("gpu");
    let materials = vec![
        Material::default(),
        Material::new([0.8, 0.8, 0.8], 0.9, 0.0),
    ];
    let mut renderer = GameRenderer::new(
        &ctx.device,
        &ctx.queue,
        spall_render::pipeline::COLOR_FORMAT,
        &materials,
        SIZE,
        None,
    );
    let ground = CubeInstance::new(
        [0.0, -0.25, 0.0],
        1,
        [60.0, 0.5, 60.0],
        CubeInstance::IDENTITY_ROTATION,
    );
    renderer.set_terrain(&ctx.device, &ctx.queue, &[ground]);
    let mut volume = LightingVolume::empty(cache_origin_around(glam::Vec3::ZERO));
    mark_world_box(
        &mut volume,
        glam::Vec3::new(-30.0, -0.5, -30.0),
        glam::Vec3::new(30.0, 0.0, 30.0),
        1,
    );
    renderer.set_sky_occupancy(&ctx.device, &ctx.queue, Some(&volume));
    let target = OffscreenTarget::new(&ctx.device, SIZE.0, SIZE.1);
    let mut environment = EnvironmentPreset::Daylight.environment();
    environment.sun_intensity = 0.0;
    let camera = Camera::looking_along(
        glam::Vec3::new(2.0, 14.0, 6.0),
        glam::Vec3::new(0.0, -1.0, -0.35),
        60_f32.to_radians(),
        SIZE.0 as f32 / SIZE.1 as f32,
    );
    let mut rgba = Vec::new();
    for _ in 0..3 {
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.render(
            &ctx.device,
            &ctx.queue,
            &mut encoder,
            target.color_view(),
            &camera,
            &environment,
            DebugView::Shaded,
        );
        target.copy_to_readback(&mut encoder);
        ctx.queue.submit([encoder.finish()]);
        rgba = target.read_rgba(&ctx).expect("readback");
    }
    let luma = luma_at(&camera, &rgba, glam::Vec3::new(2.0, 0.0, 0.0));
    println!("minimal sanity ground luma: {luma:.3}");
    assert!(luma > 0.02, "minimal ambient sanity failed: {luma}");
}
